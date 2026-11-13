//! A small AMQP 1.0 client for tests, built on this crate's own codec. It is not a general
//! client: one session on channel 0, links addressed by the handle it chose, every wait has a
//! timeout and unexpected situations panic with a message (it is meant for `#[tokio::test]`).

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::Error;
use crate::frame::*;
use crate::message::Message;
use crate::performatives::*;
use crate::sasl::Sasl;
use crate::terminus::{DeliveryState, Source, Target, TargetKind};
use crate::types::Value;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A complete delivery received on a link.
#[derive(Debug, Clone)]
pub struct Received {
    pub delivery_id: u32,
    pub tag: Vec<u8>,
    pub message_format: u32,
    pub settled: bool,
    pub payload: Bytes,
}

impl Received {
    pub fn message(&self) -> Message {
        Message::decode(&self.payload).expect("received message decodes")
    }
}

#[derive(Default)]
pub struct LinkState {
    pub name: String,
    /// Our role.
    pub role: Option<Role>,
    /// The server's attach response.
    pub attach: Option<Attach>,
    /// Credit the server gave us (we are the sender).
    pub credit: u32,
    pub delivery_count: u32,
    pub inbox: VecDeque<Received>,
    /// `Some(error)` once the server detached.
    pub detached: Option<Option<Error>>,
    /// The server answered a drain with a flow.
    pub drain_flow: Option<Flow>,
    partial: Option<(Received, BytesMut)>,
}

pub struct TestClient {
    io: Box<dyn Io>,
    buf: BytesMut,
    pub peer_open: Option<Open>,
    pub peer_begin: Option<Begin>,
    pub links: HashMap<u32, LinkState>,
    /// Dispositions the server sent for deliveries we sent: delivery-id -> (state, settled).
    pub dispositions: HashMap<u32, (Option<DeliveryState>, bool)>,
    /// Dispositions the server sent as the sender, for deliveries it sent us: delivery-id ->
    /// (state, settled).
    pub sender_dispositions: HashMap<u32, (Option<DeliveryState>, bool)>,
    pub closed: Option<Option<Error>>,
    pub session_ended: Option<Option<Error>>,
    pub remote_incoming_window: u32,
    next_outgoing_id: u32,
    next_incoming_id: u32,
    next_delivery_id: u32,
    next_handle: u32,
    /// The channel (session) that `attach`, `send_*`, `settle` and `detach` use. 0 until
    /// [`TestClient::begin_another_session`] switches it.
    pub channel: u16,
    begins: u32,
    pub timeout: Duration,
    pub heartbeats: u32,
    pub idle_timeout_ms: Option<u32>,
}

impl TestClient {
    /// Connect with the plain AMQP header (no SASL).
    pub async fn connect(addr: std::net::SocketAddr) -> TestClient {
        let s = TcpStream::connect(addr).await.expect("connect");
        s.set_nodelay(true).ok();
        let mut c = TestClient::from_io(Box::new(s));
        c.io.write_all(&HEADER_AMQP).await.expect("write header");
        c.expect_header(&HEADER_AMQP).await;
        c
    }

    /// Connect with SASL (`mechanism`, optional initial response), then the AMQP header.
    pub async fn connect_sasl(addr: std::net::SocketAddr, mechanism: &str, initial: Option<&[u8]>) -> TestClient {
        let s = TcpStream::connect(addr).await.expect("connect");
        s.set_nodelay(true).ok();
        Self::sasl_over(Box::new(s), mechanism, initial).await
    }

    pub async fn sasl_over(io: Box<dyn Io>, mechanism: &str, initial: Option<&[u8]>) -> TestClient {
        let mut c = TestClient::from_io(io);
        c.io.write_all(&HEADER_SASL).await.expect("write header");
        c.expect_header(&HEADER_SASL).await;
        let f = c.read_frame().await.expect("sasl mechanisms");
        match f.body {
            FrameBody::Sasl(Sasl::Mechanisms { mechanisms }) => assert!(mechanisms.iter().any(|m| m == mechanism), "{mechanisms:?}"),
            other => panic!("expected sasl-mechanisms, got {other:?}"),
        }
        c.write_frame(&Frame {
            channel: 0,
            body: FrameBody::Sasl(Sasl::Init {
                mechanism: mechanism.into(),
                initial_response: initial.map(|b| b.to_vec()),
                hostname: None,
            }),
        })
        .await;
        let f = c.read_frame().await.expect("sasl outcome");
        match f.body {
            FrameBody::Sasl(Sasl::Outcome { code, .. }) => assert_eq!(code, 0, "sasl outcome code"),
            other => panic!("expected sasl-outcome, got {other:?}"),
        }
        c.io.write_all(&HEADER_AMQP).await.expect("write amqp header");
        c.expect_header(&HEADER_AMQP).await;
        c
    }

    pub fn from_io(io: Box<dyn Io>) -> TestClient {
        TestClient {
            io,
            buf: BytesMut::new(),
            peer_open: None,
            peer_begin: None,
            links: HashMap::new(),
            dispositions: HashMap::new(),
            sender_dispositions: HashMap::new(),
            closed: None,
            session_ended: None,
            remote_incoming_window: 0,
            next_outgoing_id: 0,
            next_incoming_id: 0,
            next_delivery_id: 0,
            next_handle: 0,
            channel: 0,
            begins: 0,
            timeout: Duration::from_secs(5),
            heartbeats: 0,
            idle_timeout_ms: None,
        }
    }

    async fn expect_header(&mut self, h: &[u8; 8]) {
        while self.buf.len() < 8 {
            let n = tokio::time::timeout(self.timeout, self.io.read_buf(&mut self.buf)).await.expect("header timeout").expect("read");
            assert!(n > 0, "connection closed while waiting for the protocol header");
        }
        let got = self.buf.split_to(8);
        assert_eq!(&got[..], &h[..], "protocol header");
    }

    pub async fn write_raw(&mut self, b: &[u8]) {
        self.io.write_all(b).await.expect("write");
    }

    pub async fn write_frame(&mut self, f: &Frame) {
        self.io.write_all(&frame_bytes(f)).await.expect("write frame");
    }

    pub async fn send(&mut self, channel: u16, p: Performative) {
        self.write_frame(&Frame { channel, body: FrameBody::Amqp(p, Bytes::new()) }).await;
    }

    /// Next frame within the timeout, `None` on timeout or EOF.
    pub async fn read_frame_for(&mut self, dur: Duration) -> Option<Frame> {
        loop {
            match decode_frame(&mut self.buf, u32::MAX) {
                Ok(Some(f)) => return Some(f),
                Ok(None) => {}
                Err(e) => panic!("server sent a bad frame: {e}"),
            }
            match tokio::time::timeout(dur, self.io.read_buf(&mut self.buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return None,
                Ok(Ok(_)) => {}
                Err(_) => return None,
            }
        }
    }

    pub async fn read_frame(&mut self) -> Option<Frame> {
        let t = self.timeout;
        self.read_frame_for(t).await
    }

    /// Read one frame and update the client's view of the connection. Returns false on
    /// timeout / EOF.
    pub async fn step(&mut self, dur: Duration) -> bool {
        let Some(f) = self.read_frame_for(dur).await else { return false };
        match f.body {
            FrameBody::Empty => self.heartbeats += 1,
            FrameBody::Sasl(_) => {}
            FrameBody::Amqp(p, payload) => self.apply(p, payload),
        }
        true
    }

    fn apply(&mut self, p: Performative, payload: Bytes) {
        match p {
            Performative::Open(o) => self.peer_open = Some(o),
            Performative::Begin(b) => {
                self.begins += 1;
                self.next_incoming_id = b.next_outgoing_id;
                self.remote_incoming_window = b.incoming_window;
                self.peer_begin = Some(b);
            }
            Performative::Close(c) => self.closed = Some(c.error),
            Performative::End(e) => self.session_ended = Some(e.error),
            Performative::Attach(a) => {
                if let Some(l) = self.links.get_mut(&a.handle) {
                    l.attach = Some(a);
                }
            }
            Performative::Detach(d) => {
                if let Some(l) = self.links.get_mut(&d.handle) {
                    l.detached = Some(d.error);
                }
            }
            Performative::Flow(f) => {
                let next_in = f.next_incoming_id.unwrap_or(self.next_outgoing_id);
                self.remote_incoming_window = crate::session::remote_window(next_in, f.incoming_window, self.next_outgoing_id);
                if let Some(h) = f.handle {
                    if let Some(l) = self.links.get_mut(&h) {
                        if l.role == Some(Role::Sender) {
                            let dc = f.delivery_count.unwrap_or(l.delivery_count);
                            l.credit = crate::session::remote_credit(dc, f.link_credit.unwrap_or(0), l.delivery_count);
                        }
                        if f.drain {
                            l.drain_flow = Some(f);
                        }
                    }
                }
            }
            Performative::Transfer(t) => {
                self.next_incoming_id = self.next_incoming_id.wrapping_add(1);
                let Some(l) = self.links.get_mut(&t.handle) else { return };
                if t.aborted {
                    l.partial = None;
                    return;
                }
                if l.partial.is_none() {
                    l.partial = Some((
                        Received {
                            delivery_id: t.delivery_id.expect("first transfer has a delivery-id"),
                            tag: t.delivery_tag.clone().unwrap_or_default(),
                            message_format: t.message_format.unwrap_or(0),
                            settled: t.settled.unwrap_or(false),
                            payload: Bytes::new(),
                        },
                        BytesMut::new(),
                    ));
                }
                let (_, acc) = l.partial.as_mut().expect("partial");
                acc.extend_from_slice(&payload);
                if !t.more {
                    let (mut r, acc) = l.partial.take().expect("partial");
                    r.payload = acc.freeze();
                    l.delivery_count = l.delivery_count.wrapping_add(1);
                    l.inbox.push_back(r);
                }
            }
            Performative::Disposition(d) => {
                let last = d.last.unwrap_or(d.first);
                let mut id = d.first;
                loop {
                    if d.role == Role::Receiver {
                        self.dispositions.insert(id, (d.state.clone(), d.settled));
                    } else {
                        self.sender_dispositions.insert(id, (d.state.clone(), d.settled));
                    }
                    if id == last {
                        break;
                    }
                    id = id.wrapping_add(1);
                }
            }
        }
    }

    /// Process frames until `cond` holds; panics after the timeout.
    pub async fn wait_until<F: Fn(&TestClient) -> bool>(&mut self, what: &str, cond: F) {
        let deadline = tokio::time::Instant::now() + self.timeout;
        while !cond(self) {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() || !self.step(left).await {
                if cond(self) {
                    return;
                }
                panic!("timed out waiting for {what}");
            }
        }
    }

    /// Process frames for `dur` (until the timer ends or the peer closes).
    pub async fn pump_for(&mut self, dur: Duration) {
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() || !self.step(left).await {
                return;
            }
        }
    }

    // ---- connection and session ---------------------------------------------------------

    pub async fn open(&mut self, hostname: &str) -> Open {
        self.open_with(Open { container_id: "test-client".into(), hostname: Some(hostname.into()), ..Default::default() }).await
    }

    pub async fn open_with(&mut self, open: Open) -> Open {
        self.idle_timeout_ms = open.idle_timeout;
        self.send(0, Performative::Open(open)).await;
        self.wait_until("open", |c| c.peer_open.is_some() || c.closed.is_some()).await;
        self.peer_open.clone().unwrap_or_else(|| panic!("server closed instead of opening: {:?}", self.closed))
    }

    pub async fn begin(&mut self) -> Begin {
        self.send(
            self.channel,
            Performative::Begin(Begin { next_outgoing_id: 0, incoming_window: 100_000, outgoing_window: 100_000, ..Default::default() }),
        )
        .await;
        self.wait_until("begin", |c| c.peer_begin.is_some()).await;
        self.peer_begin.clone().expect("begin")
    }

    /// Begin one more session on the next free channel and make it the one the link helpers use.
    /// Handle numbers keep counting up across sessions, so every link of the client has its own.
    pub async fn begin_another_session(&mut self) -> u16 {
        let seen = self.begins;
        let ch = self.channel + 1;
        self.send(
            ch,
            Performative::Begin(Begin { next_outgoing_id: 0, incoming_window: 100_000, outgoing_window: 100_000, ..Default::default() }),
        )
        .await;
        self.wait_until("begin", |c| c.begins > seen).await;
        self.channel = ch;
        ch
    }

    /// open + begin.
    pub async fn open_session(&mut self, hostname: &str) {
        self.open(hostname).await;
        self.begin().await;
    }

    pub async fn end_session(&mut self) -> Option<Error> {
        self.send(self.channel, Performative::End(End { error: None })).await;
        self.wait_until("end", |c| c.session_ended.is_some()).await;
        self.session_ended.clone().flatten()
    }

    pub async fn close(&mut self) -> Option<Error> {
        self.send(0, Performative::Close(Close { error: None })).await;
        self.wait_until("close", |c| c.closed.is_some()).await;
        self.closed.clone().flatten()
    }

    // ---- links --------------------------------------------------------------------------

    /// Send `attach` and wait for the answer. `Ok(handle)` when accepted; `Err(error)` when the
    /// server refused (attach with null termini followed by a detach).
    pub async fn attach(&mut self, mut a: Attach) -> Result<u32, Option<Error>> {
        let h = self.next_handle;
        self.next_handle += 1;
        a.handle = h;
        self.links.insert(h, LinkState { name: a.name.clone(), role: Some(a.role), ..Default::default() });
        self.send(self.channel, Performative::Attach(a.clone())).await;
        self.wait_until("attach response", |c| c.links[&h].attach.is_some() || c.links[&h].detached.is_some()).await;
        let resp = self.links[&h].attach.clone();
        let accepted = resp.as_ref().map(|r| r.source.is_some() || r.target.is_some()).unwrap_or(false);
        if !accepted {
            self.wait_until("detach after refused attach", |c| c.links[&h].detached.is_some()).await;
            let err = self.links[&h].detached.clone().flatten();
            // acknowledge the detach
            self.send(self.channel, Performative::Detach(Detach { handle: h, closed: true, error: None })).await;
            return Err(err);
        }
        if a.role == Role::Receiver {
            // we receive: nothing more to wait for
        }
        Ok(h)
    }

    /// Attach a sender link to `address` and wait for the server's initial credit.
    pub async fn attach_sender(&mut self, address: &str) -> Result<u32, Option<Error>> {
        let mut a = Attach::new(format!("sender-{address}-{}", self.next_handle), 0, Role::Sender);
        a.source = Some(Source::with_address("sender"));
        a.target = Some(TargetKind::Target(Target::with_address(address)));
        let h = self.attach(a).await?;
        self.wait_until("initial credit", |c| c.links[&h].credit > 0 || c.links[&h].detached.is_some()).await;
        Ok(h)
    }

    pub async fn attach_sender_with(&mut self, address: &str, snd: SndSettleMode, rcv: RcvSettleMode) -> Result<u32, Option<Error>> {
        let mut a = Attach::new(format!("sender-{address}-{}", self.next_handle), 0, Role::Sender);
        a.source = Some(Source::with_address("sender"));
        a.target = Some(TargetKind::Target(Target::with_address(address)));
        a.snd_settle_mode = snd;
        a.rcv_settle_mode = rcv;
        let h = self.attach(a).await?;
        self.wait_until("initial credit", |c| c.links[&h].credit > 0 || c.links[&h].detached.is_some()).await;
        Ok(h)
    }

    /// Attach a receiver link on `address` (first/unsettled mode) and grant `credit`.
    pub async fn attach_receiver(&mut self, address: &str, credit: u32) -> Result<u32, Option<Error>> {
        self.attach_receiver_source(Source::with_address(address), SndSettleMode::Unsettled, RcvSettleMode::First, credit).await
    }

    pub async fn attach_receiver_source(
        &mut self,
        source: Source,
        snd: SndSettleMode,
        rcv: RcvSettleMode,
        credit: u32,
    ) -> Result<u32, Option<Error>> {
        let mut a = Attach::new(format!("receiver-{}-{}", source.address.clone().unwrap_or_default(), self.next_handle), 0, Role::Receiver);
        a.source = Some(source);
        a.target = Some(TargetKind::Target(Target::with_address("receiver")));
        a.snd_settle_mode = snd;
        a.rcv_settle_mode = rcv;
        let h = self.attach(a).await?;
        if credit > 0 {
            self.flow(h, credit, false).await;
        }
        Ok(h)
    }

    /// Link flow from the receiver side: set absolute credit.
    pub async fn flow(&mut self, handle: u32, credit: u32, drain: bool) {
        let dc = self.links.get(&handle).map(|l| l.delivery_count).unwrap_or(0);
        self.send(
            self.channel,
            Performative::Flow(Flow {
                next_incoming_id: Some(self.next_incoming_id),
                incoming_window: 100_000,
                next_outgoing_id: self.next_outgoing_id,
                outgoing_window: 100_000,
                handle: Some(handle),
                delivery_count: Some(dc),
                link_credit: Some(credit),
                drain,
                ..Default::default()
            }),
        )
        .await;
    }

    /// Send an encoded message; returns the delivery id. Waits for credit and window.
    pub async fn send_payload(&mut self, handle: u32, payload: &[u8], format: u32, settled: bool) -> u32 {
        self.send_payload_with_state(handle, payload, format, settled, None).await
    }

    /// Like [`TestClient::send_payload`] with a delivery state on the transfer (a `transactional-state`).
    pub async fn send_payload_with_state(
        &mut self,
        handle: u32,
        payload: &[u8],
        format: u32,
        settled: bool,
        state: Option<DeliveryState>,
    ) -> u32 {
        self.wait_until("send credit", |c| {
            let l = &c.links[&handle];
            (l.credit > 0 && c.remote_incoming_window > 0) || l.detached.is_some()
        })
        .await;
        assert!(self.links[&handle].detached.is_none(), "link detached while sending: {:?}", self.links[&handle].detached);
        let id = self.next_delivery_id;
        self.next_delivery_id = self.next_delivery_id.wrapping_add(1);
        let tag = id.to_be_bytes().to_vec();
        let first = Transfer {
            handle,
            delivery_id: Some(id),
            delivery_tag: Some(tag),
            message_format: Some(format),
            settled: Some(settled),
            state,
            ..Default::default()
        };
        let max = self.peer_open.as_ref().map(|o| o.max_frame_size).unwrap_or(65536);
        let frames = split_transfer(self.channel, first, payload, max);
        self.next_outgoing_id = self.next_outgoing_id.wrapping_add(frames.len() as u32);
        self.remote_incoming_window = self.remote_incoming_window.saturating_sub(frames.len() as u32);
        let l = self.links.get_mut(&handle).expect("link");
        l.credit = l.credit.saturating_sub(1);
        l.delivery_count = l.delivery_count.wrapping_add(1);
        for f in frames {
            self.write_frame(&f).await;
        }
        id
    }

    pub async fn send_message(&mut self, handle: u32, m: &Message) -> u32 {
        self.send_payload(handle, &m.encode(), 0, false).await
    }

    /// Send a message as transactional work and wait for the server's outcome.
    pub async fn send_message_in_txn(&mut self, handle: u32, m: &Message, txn_id: &[u8]) -> Option<DeliveryState> {
        let state = DeliveryState::Transactional { txn_id: txn_id.to_vec(), outcome: None };
        let id = self.send_payload_with_state(handle, &m.encode(), 0, false, Some(state)).await;
        self.wait_disposition(id).await
    }

    /// The server's attach answer for a link.
    pub fn attach_of(&self, handle: u32) -> Option<Attach> {
        self.links.get(&handle).and_then(|l| l.attach.clone())
    }

    /// Attach a sender link to the transaction coordinator and wait for its credit.
    pub async fn attach_coordinator(&mut self) -> Result<u32, Option<Error>> {
        let mut a = Attach::new(format!("coordinator-{}", self.next_handle), 0, Role::Sender);
        a.source = Some(Source::default());
        a.target = Some(TargetKind::Coordinator(Default::default()));
        let h = self.attach(a).await?;
        self.wait_until("coordinator credit", |c| c.links[&h].credit > 0 || c.links[&h].detached.is_some()).await;
        Ok(h)
    }

    /// `declare` on a coordinator link: the transaction id, or the error of the rejection.
    pub async fn declare(&mut self, coordinator: u32) -> Result<Vec<u8>, Option<Error>> {
        let body = crate::terminus::TxnControl::Declare { global_id: None }.to_value();
        let m = Message { body: crate::message::Body::Value(body), ..Default::default() };
        match self.send_message_wait(coordinator, &m).await {
            Some(DeliveryState::Declared { txn_id }) => Ok(txn_id),
            Some(DeliveryState::Rejected { error }) => Err(error),
            other => panic!("unexpected answer to declare: {other:?}"),
        }
    }

    /// `discharge` on a coordinator link (`fail` true rolls back). The answer: `Ok` for accepted.
    pub async fn discharge(&mut self, coordinator: u32, txn_id: &[u8], fail: bool) -> Result<(), Option<Error>> {
        let body = crate::terminus::TxnControl::Discharge { txn_id: txn_id.to_vec(), fail }.to_value();
        let m = Message { body: crate::message::Body::Value(body), ..Default::default() };
        match self.send_message_wait(coordinator, &m).await {
            Some(DeliveryState::Accepted) => Ok(()),
            Some(DeliveryState::Rejected { error }) => Err(error),
            other => panic!("unexpected answer to discharge: {other:?}"),
        }
    }

    /// Send a message and wait for the server's outcome.
    pub async fn send_message_wait(&mut self, handle: u32, m: &Message) -> Option<DeliveryState> {
        let id = self.send_message(handle, m).await;
        self.wait_disposition(id).await
    }

    pub async fn wait_disposition(&mut self, delivery_id: u32) -> Option<DeliveryState> {
        self.wait_until("disposition", |c| c.dispositions.contains_key(&delivery_id)).await;
        self.dispositions.get(&delivery_id).and_then(|(s, _)| s.clone())
    }

    /// Next delivery on a receiver link, `None` after `dur`.
    pub async fn recv_for(&mut self, handle: u32, dur: Duration) -> Option<Received> {
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            if let Some(r) = self.links.get_mut(&handle).and_then(|l| l.inbox.pop_front()) {
                return Some(r);
            }
            if self.links.get(&handle).map(|l| l.detached.is_some()).unwrap_or(true) {
                return None;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() || !self.step(left).await {
                return self.links.get_mut(&handle).and_then(|l| l.inbox.pop_front());
            }
        }
    }

    pub async fn recv(&mut self, handle: u32) -> Received {
        let t = self.timeout;
        self.recv_for(handle, t)
            .await
            .unwrap_or_else(|| panic!("no delivery on link {handle}: {:?}", self.links.get(&handle).map(|l| &l.detached)))
    }

    /// Settle a received delivery (disposition from the receiver, settled).
    pub async fn settle(&mut self, delivery_id: u32, state: DeliveryState) {
        self.send(
            self.channel,
            Performative::Disposition(Disposition {
                role: Role::Receiver,
                first: delivery_id,
                last: None,
                settled: true,
                state: Some(state),
                batchable: false,
            }),
        )
        .await;
    }

    /// Disposition without settling (second settle mode).
    pub async fn disposition_unsettled(&mut self, delivery_id: u32, state: DeliveryState) {
        self.send(
            self.channel,
            Performative::Disposition(Disposition {
                role: Role::Receiver,
                first: delivery_id,
                last: None,
                settled: false,
                state: Some(state),
                batchable: false,
            }),
        )
        .await;
    }

    /// Wait for the server's settling disposition of a delivery it sent us (peer-lock mode).
    pub async fn wait_sender_disposition(&mut self, delivery_id: u32) -> Option<DeliveryState> {
        self.wait_until("settling disposition", |c| c.sender_dispositions.get(&delivery_id).map(|d| d.1).unwrap_or(false)).await;
        self.sender_dispositions.get(&delivery_id).and_then(|(s, _)| s.clone())
    }

    pub async fn detach(&mut self, handle: u32) -> Option<Error> {
        self.send(self.channel, Performative::Detach(Detach { handle, closed: true, error: None })).await;
        self.wait_until("detach", |c| c.links[&handle].detached.is_some()).await;
        self.links[&handle].detached.clone().flatten()
    }

    /// Wait until the server detaches `handle`; returns its error.
    pub async fn wait_detached(&mut self, handle: u32) -> Option<Error> {
        self.wait_until("detach from the server", |c| c.links[&handle].detached.is_some()).await;
        self.links[&handle].detached.clone().flatten()
    }

    pub fn credit(&self, handle: u32) -> u32 {
        self.links[&handle].credit
    }

    /// A map-valued request/response helper is service specific; this builds a message with an
    /// amqp-value body and the given application properties.
    pub fn value_message(body: Value, props: Vec<(&str, Value)>) -> Message {
        Message {
            application_properties: Some(props.into_iter().map(|(k, v)| (Value::str(k), v)).collect()),
            body: crate::message::Body::Value(body),
            ..Default::default()
        }
    }
}
