//! Connection, session and link behaviour, driven by the in-crate test client against a small
//! in-memory handler.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use azemu_amqp::api::*;
use azemu_amqp::client::TestClient;
use azemu_amqp::error::{condition, Error};
use azemu_amqp::message::{Body, Message};
use azemu_amqp::performatives::*;
use azemu_amqp::serve_connection;
use azemu_amqp::terminus::*;
use azemu_amqp::types::Value;
use parking_lot::Mutex;
use tokio::net::TcpListener;

type Disp = (Vec<u8>, Option<DeliveryState>, bool);

#[derive(Default)]
struct Shared {
    /// (address, delivery) for everything received.
    received: Mutex<Vec<(String, IncomingDelivery)>>,
    /// per address, what the sender links hand out
    outbox: Mutex<HashMap<String, VecDeque<Outgoing>>>,
    dispositions: Mutex<Vec<Disp>>,
    events: Mutex<Vec<String>>,
    handles: Mutex<HashMap<String, LinkHandle>>,
    conn: Mutex<Option<ConnHandle>>,
    open_hostname: Mutex<Option<String>>,
}

struct Handler {
    shared: Arc<Shared>,
    addr: HashMap<LinkId, String>,
}

impl ConnectionHandler for Handler {
    fn on_open(&mut self, open: &Open) -> Result<(), Error> {
        *self.shared.open_hostname.lock() = open.hostname.clone();
        if open.container_id == "reject-me" {
            return Err(Error::new(condition::UNAUTHORIZED_ACCESS, "go away"));
        }
        Ok(())
    }
    fn on_attach(&mut self, req: AttachRequest) -> AttachDecision {
        let address = match req.local_role() {
            Role::Receiver => match &req.attach.target {
                Some(TargetKind::Target(t)) => t.address.clone().unwrap_or_default(),
                _ => String::new(),
            },
            Role::Sender => req.attach.source.as_ref().and_then(|s| s.address.clone()).unwrap_or_default(),
        };
        self.shared.events.lock().push(format!("attach {address}"));
        if address == "refuse" {
            return AttachDecision::Refuse(Error::new(condition::NOT_FOUND, "no such entity"));
        }
        self.addr.insert(req.link, address.clone());
        self.shared.handles.lock().insert(address.clone(), req.handle.clone());
        if address == "defer" {
            return AttachDecision::Defer;
        }
        let mut acc = AttachAccept::default();
        if address == "small" {
            acc.max_message_size = Some(1000);
        }
        AttachDecision::Accept(acc)
    }
    fn on_transfer(&mut self, link: LinkId, d: IncomingDelivery) -> TransferAction {
        let a = self.addr.get(&link).cloned().unwrap_or_default();
        self.shared.received.lock().push((a.clone(), d));
        if a == "defer-settle" {
            TransferAction::Defer
        } else if a == "reject" {
            TransferAction::Settle(DeliveryState::rejected(Error::new("custom:error", "rejected")))
        } else {
            TransferAction::Settle(DeliveryState::Accepted)
        }
    }
    fn on_disposition(&mut self, _l: LinkId, tag: &[u8], state: Option<DeliveryState>, settled: bool) -> Option<DeliveryState> {
        self.shared.dispositions.lock().push((tag.to_vec(), state, settled));
        if tag == b"bad" {
            return Some(DeliveryState::rejected(Error::new("com.microsoft:message-lock-lost", "lock lost")));
        }
        None
    }
    fn pull(&mut self, link: LinkId) -> Option<Outgoing> {
        let a = self.addr.get(&link)?;
        self.shared.outbox.lock().get_mut(a)?.pop_front()
    }
    fn on_detach(&mut self, link: LinkId, reason: DetachReason) {
        let a = self.addr.get(&link).cloned().unwrap_or_default();
        self.shared.events.lock().push(format!(
            "detach {a} {}",
            match reason {
                DetachReason::Peer { error: Some(e), .. } => format!("peer-error:{}", e.condition),
                DetachReason::Peer { .. } => "peer".into(),
                DetachReason::Local => "local".into(),
                DetachReason::SessionEnded => "session-ended".into(),
                DetachReason::ConnectionClosed => "connection-closed".into(),
            }
        ));
    }
    fn on_close(&mut self) {
        self.shared.events.lock().push("close".into());
    }
}

struct Factory(Arc<Shared>);
impl HandlerFactory for Factory {
    fn new_connection(&self, _info: &ConnectionInfo, conn: ConnHandle) -> Box<dyn ConnectionHandler> {
        *self.0.conn.lock() = Some(conn);
        Box::new(Handler { shared: self.0.clone(), addr: HashMap::new() })
    }
}

async fn start(cfg: ServerConfig) -> (SocketAddr, Arc<Shared>) {
    let shared = Arc::new(Shared::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let factory: SharedFactory = Arc::new(Factory(shared.clone()));
    let cfg = Arc::new(cfg);
    tokio::spawn(async move {
        loop {
            let Ok((s, peer)) = listener.accept().await else { return };
            let info = ConnectionInfo { peer, local: addr, tls: false, server_name: None };
            tokio::spawn(serve_connection(s, info, factory.clone(), cfg.clone()));
        }
    });
    (addr, shared)
}

async fn connected(cfg: ServerConfig) -> (TestClient, Arc<Shared>) {
    let (addr, shared) = start(cfg).await;
    let mut c = TestClient::connect(addr).await;
    c.open_session("localhost").await;
    (c, shared)
}

fn msg(body: &str) -> Message {
    Message::data(body.as_bytes().to_vec())
}

fn wake(shared: &Shared, address: &str) {
    shared.handles.lock().get(address).expect("link handle").wake();
}

fn push(shared: &Shared, address: &str, tag: &[u8], body: &str, settled: bool) {
    shared.outbox.lock().entry(address.to_string()).or_default().push_back(Outgoing {
        tag: tag.to_vec(),
        message_format: 0,
        settled,
        payload: msg(body).encode(),
    });
}

async fn eventually<F: Fn() -> bool>(what: &str, f: F) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---- handshake -------------------------------------------------------------------------------

#[tokio::test]
async fn plain_amqp_header_open_close() {
    let (addr, shared) = start(ServerConfig::default()).await;
    let mut c = TestClient::connect(addr).await;
    let o = c.open("myns.servicebus.localhost").await;
    assert_eq!(o.container_id, "azemu");
    assert_eq!(o.max_frame_size, 65536);
    assert_eq!(o.idle_timeout, Some(240_000));
    assert_eq!(shared.open_hostname.lock().as_deref(), Some("myns.servicebus.localhost"));
    assert!(c.close().await.is_none());
    eventually("on_close", || shared.events.lock().iter().any(|e| e == "close")).await;
}

#[tokio::test]
async fn every_sasl_mechanism_is_accepted() {
    for mech in ["ANONYMOUS", "PLAIN", "EXTERNAL", "MSSBCBS"] {
        let (addr, _) = start(ServerConfig::default()).await;
        let init: Option<&[u8]> = if mech == "PLAIN" { Some(b"\0user\0pass") } else { None };
        let mut c = TestClient::connect_sasl(addr, mech, init).await;
        c.open_session("localhost").await;
        c.close().await;
    }
}

#[tokio::test]
async fn unknown_protocol_header_gets_the_amqp_header_back_and_is_closed() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (addr, _) = start(ServerConfig::default()).await;
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"AMQP\x02\x01\x00\x00").await.unwrap(); // TLS-in-AMQP header
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut got)).await.unwrap().unwrap();
    assert_eq!(got, b"AMQP\x00\x01\x00\x00");
}

#[tokio::test]
async fn open_error_from_the_service_closes_the_connection() {
    let (addr, _) = start(ServerConfig::default()).await;
    let mut c = TestClient::connect(addr).await;
    c.send(0, Performative::Open(Open { container_id: "reject-me".into(), ..Default::default() })).await;
    c.wait_until("close", |c| c.closed.is_some()).await;
    let err = c.closed.clone().unwrap().unwrap();
    assert_eq!(err.condition, condition::UNAUTHORIZED_ACCESS);
}

#[tokio::test]
async fn garbage_after_the_header_closes_with_a_framing_error() {
    let (addr, _) = start(ServerConfig::default()).await;
    let mut c = TestClient::connect(addr).await;
    c.write_raw(&[0, 0, 0, 4, 2, 0, 0, 0]).await;
    c.wait_until("close", |c| c.closed.is_some()).await;
    assert_eq!(c.closed.clone().unwrap().unwrap().condition, condition::CONNECTION_FRAMING_ERROR);
}

#[tokio::test]
async fn a_frame_before_open_is_refused() {
    let (addr, _) = start(ServerConfig::default()).await;
    let mut c = TestClient::connect(addr).await;
    c.send(0, Performative::Begin(Begin::default())).await;
    c.wait_until("close", |c| c.closed.is_some()).await;
    assert_eq!(c.closed.clone().unwrap().unwrap().condition, condition::NOT_ALLOWED);
}

#[tokio::test]
async fn oversize_frames_are_a_framing_error() {
    let cfg = ServerConfig { max_frame_size: 1024, ..Default::default() };
    let (addr, _) = start(cfg).await;
    let mut c = TestClient::connect(addr).await;
    c.open("h").await;
    c.write_raw(&[0, 0, 0x10, 0, 2, 0, 0, 0]).await; // claims 4096 bytes
    c.wait_until("close", |c| c.closed.is_some()).await;
    assert_eq!(c.closed.clone().unwrap().unwrap().condition, condition::CONNECTION_FRAMING_ERROR);
}

/// A hostile frame: an `open` whose properties hold 150 arrays of 65,535 nulls, 1.5 KB.
fn amplifying_open_frame(arrays: usize) -> Vec<u8> {
    let array = |count: u32| {
        let mut v = vec![0xf0];
        v.extend_from_slice(&5u32.to_be_bytes());
        v.extend_from_slice(&count.to_be_bytes());
        v.push(0x40);
        v
    };
    let items: Vec<u8> = (0..arrays).flat_map(|_| array(65_535)).collect();
    let mut list = vec![0xd0];
    list.extend_from_slice(&((items.len() + 4) as u32).to_be_bytes());
    list.extend_from_slice(&(arrays as u32).to_be_bytes());
    list.extend_from_slice(&items);
    let mut map_body = vec![0xa3, 1, b'a'];
    map_body.extend_from_slice(&list);
    let mut map = vec![0xd1];
    map.extend_from_slice(&((map_body.len() + 4) as u32).to_be_bytes());
    map.extend_from_slice(&2u32.to_be_bytes());
    map.extend_from_slice(&map_body);
    let mut fields = vec![0xa1, 1, b'x'];
    fields.extend_from_slice(&[0x40; 8]);
    fields.extend_from_slice(&map);
    let mut open = vec![0x00, 0x53, 0x10, 0xd0];
    open.extend_from_slice(&((fields.len() + 4) as u32).to_be_bytes());
    open.extend_from_slice(&10u32.to_be_bytes());
    open.extend_from_slice(&fields);
    let mut frame = Vec::new();
    frame.extend_from_slice(&((open.len() + 8) as u32).to_be_bytes());
    frame.extend_from_slice(&[2, 0, 0, 0]);
    frame.extend_from_slice(&open);
    frame
}

#[tokio::test]
async fn a_frame_that_decodes_to_far_more_values_than_it_has_bytes_closes_with_decode_error() {
    for arrays in [150usize, 5_000] {
        let (addr, _) = start(ServerConfig::default()).await;
        let mut c = TestClient::connect(addr).await;
        let frame = amplifying_open_frame(arrays);
        assert!(frame.len() <= 1 << 16);
        c.write_raw(&frame).await;
        c.wait_until("close", |c| c.closed.is_some()).await;
        assert_eq!(c.closed.clone().unwrap().unwrap().condition, condition::DECODE_ERROR);
    }
}

#[tokio::test]
async fn session_end_and_second_session() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_sender("q").await.unwrap();
    assert!(c.end_session().await.is_none());
    eventually("link notified", || shared.events.lock().iter().any(|e| e == "detach q session-ended")).await;
    let _ = h;
}

#[tokio::test]
async fn begin_on_a_used_channel_is_refused() {
    let (mut c, _) = connected(ServerConfig::default()).await;
    c.send(0, Performative::Begin(Begin::default())).await;
    c.wait_until("close", |c| c.closed.is_some()).await;
    assert_eq!(c.closed.clone().unwrap().unwrap().condition, condition::NOT_ALLOWED);
}

// ---- receiving from the client ----------------------------------------------------------------

#[tokio::test]
async fn attach_receive_and_accept() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_sender("q").await.unwrap();
    let resp = c.links[&h].attach.clone().unwrap();
    assert_eq!(resp.role, Role::Receiver, "server attaches as receiver");
    assert_eq!(resp.target.unwrap().address(), Some("q"));
    assert_eq!(resp.max_message_size, Some(100 * 1024 * 1024));
    assert_eq!(c.credit(h), 1000);

    let outcome = c.send_message_wait(h, &msg("hello")).await;
    assert_eq!(outcome, Some(DeliveryState::Accepted));
    assert!(c.dispositions[&0].1, "server settles");
    let got = shared.received.lock();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, "q");
    assert_eq!(Message::decode(&got[0].1.payload).unwrap().body, Body::Data(vec![b"hello".to_vec()]));
}

#[tokio::test]
async fn presettled_transfers_get_no_disposition() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_sender("q").await.unwrap();
    let id = c.send_payload(h, &msg("x").encode(), 0, true).await;
    eventually("received", || shared.received.lock().len() == 1).await;
    assert!(shared.received.lock()[0].1.settled);
    c.pump_for(Duration::from_millis(100)).await;
    assert!(!c.dispositions.contains_key(&id), "no disposition for a pre-settled delivery");
}

#[tokio::test]
async fn rejected_outcome_carries_the_error() {
    let (mut c, _) = connected(ServerConfig::default()).await;
    let h = c.attach_sender("reject").await.unwrap();
    match c.send_message_wait(h, &msg("x")).await {
        Some(DeliveryState::Rejected { error: Some(e) }) => assert_eq!(e.condition, "custom:error"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn multi_frame_transfers_are_reassembled() {
    let cfg = ServerConfig { max_frame_size: 1024, ..Default::default() };
    let (mut c, shared) = connected(cfg).await;
    let h = c.attach_sender("q").await.unwrap();
    let big: Vec<u8> = (0..20_000u32).map(|i| (i % 253) as u8).collect();
    let m = Message::data(big.clone());
    assert_eq!(c.send_message_wait(h, &m).await, Some(DeliveryState::Accepted));
    let got = shared.received.lock();
    assert_eq!(Message::decode(&got[0].1.payload).unwrap().body, Body::Data(vec![big]));
}

#[tokio::test]
async fn message_over_the_link_limit_is_rejected_with_message_size_exceeded() {
    let (mut c, _) = connected(ServerConfig::default()).await;
    let h = c.attach_sender("small").await.unwrap();
    assert_eq!(c.links[&h].attach.as_ref().unwrap().max_message_size, Some(1000));
    match c.send_message_wait(h, &Message::data(vec![7; 5000])).await {
        Some(DeliveryState::Rejected { error: Some(err) }) => {
            assert_eq!(err.condition, condition::LINK_MESSAGE_SIZE_EXCEEDED);
            assert!(err.description.unwrap().contains("exceeds the limit (1000 bytes)"));
        }
        other => panic!("{other:?}"),
    }
    // the link stays usable
    assert_eq!(c.send_message_wait(h, &Message::data(vec![7; 10])).await, Some(DeliveryState::Accepted));
    // a pre-settled oversize message cannot be answered: the link is detached
    c.send_payload(h, &Message::data(vec![7; 5000]).encode(), 0, true).await;
    let err = c.wait_detached(h).await.unwrap();
    assert_eq!(err.condition, condition::LINK_MESSAGE_SIZE_EXCEEDED);
}

#[tokio::test]
async fn credit_is_topped_up_and_the_session_window_is_refreshed() {
    // 3000 transfers is more than the 2048 window: this only works if the server sends flows.
    let cfg = ServerConfig { session_window: 256, receiver_credit: 100, ..Default::default() };
    let (mut c, shared) = connected(cfg).await;
    let h = c.attach_sender("q").await.unwrap();
    let payload = msg("x").encode();
    for _ in 0..1000 {
        c.send_payload(h, &payload, 0, true).await;
    }
    eventually("all received", || shared.received.lock().len() == 1000).await;
}

#[tokio::test]
async fn deferred_settlement() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_sender("defer-settle").await.unwrap();
    let id = c.send_message(h, &msg("x")).await;
    eventually("received", || shared.received.lock().len() == 1).await;
    c.pump_for(Duration::from_millis(100)).await;
    assert!(!c.dispositions.contains_key(&id), "nothing settled yet");
    let lh = shared.handles.lock().get("defer-settle").unwrap().clone();
    lh.settle(shared.received.lock()[0].1.delivery_id, DeliveryState::Released);
    assert_eq!(c.wait_disposition(id).await, Some(DeliveryState::Released));
}

#[tokio::test]
async fn second_receiver_settle_mode_is_settled_at_once() {
    let (mut c, _shared) = connected(ServerConfig::default()).await;
    let h = c.attach_sender_with("q", SndSettleMode::Unsettled, RcvSettleMode::Second).await.unwrap();
    let id = c.send_message(h, &msg("x")).await;
    c.wait_until("disposition", |c| c.dispositions.contains_key(&id)).await;
    assert_eq!(c.dispositions[&id], (Some(DeliveryState::Accepted), true), "settled right away, no second step");
}

#[tokio::test]
async fn unsettled_terminal_state_is_answered_with_a_settling_disposition() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_receiver_source(Source::with_address("src"), SndSettleMode::Unsettled, RcvSettleMode::Second, 10).await.unwrap();
    push(&shared, "src", b"good", "x", false);
    push(&shared, "src", b"bad", "y", false);
    wake(&shared, "src");
    let good = c.recv(h).await;
    let bad = c.recv(h).await;
    c.disposition_unsettled(good.delivery_id, DeliveryState::modified(true, false)).await;
    assert_eq!(c.wait_sender_disposition(good.delivery_id).await, Some(DeliveryState::modified(true, false)), "the peer's state is echoed");
    c.disposition_unsettled(bad.delivery_id, DeliveryState::Accepted).await;
    match c.wait_sender_disposition(bad.delivery_id).await {
        Some(DeliveryState::Rejected { error: Some(e) }) => assert_eq!(e.condition, "com.microsoft:message-lock-lost"),
        other => panic!("{other:?}"),
    }
}

// ---- attach outcomes --------------------------------------------------------------------------

#[tokio::test]
async fn refused_attach_is_an_attach_then_a_detach_with_the_error() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let err = c.attach_sender("refuse").await.unwrap_err().unwrap();
    assert_eq!(err.condition, condition::NOT_FOUND);
    assert_eq!(err.description.as_deref(), Some("no such entity"));
    // the answer has no target (AMQP 1.0 section 2.6.3; Microsoft.Azure.Amqp does the same); the .NET SDK takes a
    // complete attach before the detach for an open link
    let answer = c.attach_of(0).expect("attach answer");
    assert!(answer.source.is_none() && answer.target.is_none(), "{answer:?}");
    // and for a receiving client the same: no source
    let err = c.attach_receiver("refuse", 5).await.unwrap_err().unwrap();
    assert_eq!(err.condition, condition::NOT_FOUND);
    let answer = c.attach_of(1).expect("attach answer");
    assert!(answer.source.is_none() && answer.target.is_none(), "{answer:?}");
    // the connection is still healthy and the same name can be attached again
    assert!(c.attach_sender("q").await.is_ok());
    let _ = shared;
}

#[tokio::test]
async fn deferred_attach_completes_later() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let task = {
        let shared = shared.clone();
        tokio::spawn(async move {
            eventually("handle", || shared.handles.lock().contains_key("defer")).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            shared.handles.lock().get("defer").unwrap().complete_attach(Ok(AttachAccept::default()));
        })
    };
    let h = c.attach_sender("defer").await.unwrap();
    assert!(c.links[&h].attach.is_some());
    task.await.unwrap();
    assert_eq!(c.send_message_wait(h, &msg("x")).await, Some(DeliveryState::Accepted));
}

#[tokio::test]
async fn deferred_attach_can_be_refused() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let s2 = shared.clone();
    tokio::spawn(async move {
        eventually("handle", || s2.handles.lock().contains_key("defer")).await;
        s2.handles.lock().get("defer").unwrap().complete_attach(Err(Error::new(condition::RESOURCE_LOCKED, "busy")));
    });
    let err = c.attach_sender("defer").await.unwrap_err().unwrap();
    assert_eq!(err.condition, condition::RESOURCE_LOCKED);
}

#[tokio::test]
async fn duplicate_handle_ends_the_session() {
    let (mut c, _) = connected(ServerConfig::default()).await;
    c.attach_sender("q").await.unwrap();
    let mut a = Attach::new("another", 0, Role::Sender); // handle 0 again
    a.target = Some(TargetKind::Target(Target::with_address("q")));
    c.send(0, Performative::Attach(a)).await;
    c.wait_until("session end", |c| c.session_ended.is_some()).await;
    assert_eq!(c.session_ended.clone().unwrap().unwrap().condition, condition::SESSION_HANDLE_IN_USE);
}

#[tokio::test]
async fn transfer_on_an_unknown_handle_ends_the_session() {
    let (mut c, _) = connected(ServerConfig::default()).await;
    c.send(0, Performative::Transfer(Transfer { handle: 9, delivery_id: Some(0), delivery_tag: Some(vec![1]), ..Default::default() }))
        .await;
    c.wait_until("session end", |c| c.session_ended.is_some()).await;
    assert_eq!(c.session_ended.clone().unwrap().unwrap().condition, condition::SESSION_UNATTACHED_HANDLE);
}

// ---- sending to the client --------------------------------------------------------------------

#[tokio::test]
async fn sender_link_respects_credit_and_wakeups() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_receiver("src", 0).await.unwrap();
    assert_eq!(c.links[&h].attach.as_ref().unwrap().role, Role::Sender);
    push(&shared, "src", b"t1", "one", false);
    push(&shared, "src", b"t2", "two", false);
    wake(&shared, "src");
    assert!(c.recv_for(h, Duration::from_millis(200)).await.is_none(), "nothing without credit");

    c.flow(h, 1, false).await;
    let d = c.recv(h).await;
    assert_eq!(d.tag, b"t1");
    assert!(!d.settled);
    assert_eq!(d.message().body, Body::Data(vec![b"one".to_vec()]));
    assert!(c.recv_for(h, Duration::from_millis(200)).await.is_none(), "credit of 1 delivers one message");

    // more credit pulls the second without a wake-up
    c.flow(h, 5, false).await;
    assert_eq!(c.recv(h).await.tag, b"t2");

    // a message that arrives later reaches the waiting receiver through a wake-up
    push(&shared, "src", b"t3", "three", false);
    wake(&shared, "src");
    assert_eq!(c.recv(h).await.tag, b"t3");
}

#[tokio::test]
async fn dispositions_for_sent_deliveries_reach_the_service_with_their_tag() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_receiver("src", 10).await.unwrap();
    push(&shared, "src", b"tag-a", "a", false);
    push(&shared, "src", b"tag-b", "b", false);
    wake(&shared, "src");
    let a = c.recv(h).await;
    let b = c.recv(h).await;
    c.settle(a.delivery_id, DeliveryState::Accepted).await;
    c.settle(b.delivery_id, DeliveryState::modified(true, false)).await;
    eventually("dispositions", || shared.dispositions.lock().len() == 2).await;
    let d = shared.dispositions.lock().clone();
    assert_eq!(d[0], (b"tag-a".to_vec(), Some(DeliveryState::Accepted), true));
    assert_eq!(d[1], (b"tag-b".to_vec(), Some(DeliveryState::modified(true, false)), true));
}

#[tokio::test]
async fn range_disposition() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_receiver("src", 10).await.unwrap();
    for i in 0..3u8 {
        push(&shared, "src", &[i], "x", false);
    }
    wake(&shared, "src");
    let first = c.recv(h).await.delivery_id;
    c.recv(h).await;
    let last = c.recv(h).await.delivery_id;
    c.send(
        0,
        Performative::Disposition(Disposition {
            role: Role::Receiver,
            first,
            last: Some(last),
            settled: true,
            state: Some(DeliveryState::Accepted),
            batchable: false,
        }),
    )
    .await;
    eventually("three dispositions", || shared.dispositions.lock().len() == 3).await;
}

#[tokio::test]
async fn presettled_deliveries_are_sent_settled_and_not_tracked() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_receiver_source(Source::with_address("src"), SndSettleMode::Settled, RcvSettleMode::First, 10).await.unwrap();
    push(&shared, "src", b"p", "x", true);
    wake(&shared, "src");
    let d = c.recv(h).await;
    assert!(d.settled);
    c.settle(d.delivery_id, DeliveryState::Accepted).await;
    c.pump_for(Duration::from_millis(100)).await;
    assert!(shared.dispositions.lock().is_empty(), "settled deliveries are not tracked");
}

#[tokio::test]
async fn large_messages_are_split_by_the_peers_max_frame_size() {
    let (addr, shared) = start(ServerConfig::default()).await;
    let mut c = TestClient::connect(addr).await;
    c.open_with(Open { container_id: "c".into(), max_frame_size: 600, ..Default::default() }).await;
    c.begin().await;
    let h = c.attach_receiver("src", 10).await.unwrap();
    let big = Message::data((0..10_000u32).map(|i| (i % 241) as u8).collect());
    shared.outbox.lock().entry("src".into()).or_default().push_back(Outgoing {
        tag: b"big".to_vec(),
        message_format: 0,
        settled: false,
        payload: big.encode(),
    });
    wake(&shared, "src");
    let d = c.recv(h).await;
    assert_eq!(d.message(), big);
}

#[tokio::test]
async fn drain_returns_unused_credit() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_receiver("src", 0).await.unwrap();
    push(&shared, "src", b"1", "one", false);
    wake(&shared, "src");
    c.flow(h, 5, true).await;
    assert_eq!(c.recv(h).await.tag, b"1");
    c.wait_until("drain answer", |c| c.links[&h].drain_flow.is_some()).await;
    let f = c.links[&h].drain_flow.clone().unwrap();
    assert_eq!(f.link_credit, Some(0));
    assert_eq!(f.delivery_count, Some(5), "all 5 credits are accounted for: 1 sent, 4 drained");
}

#[tokio::test]
async fn session_window_limits_sending() {
    // The client's incoming window is 3 transfers; the server may not send more until it is refreshed.
    let (addr, shared) = start(ServerConfig::default()).await;
    let mut c = TestClient::connect(addr).await;
    c.open("h").await;
    c.send(0, Performative::Begin(Begin { next_outgoing_id: 0, incoming_window: 3, outgoing_window: 100, ..Default::default() })).await;
    c.wait_until("begin", |c| c.peer_begin.is_some()).await;
    let h = c.attach_receiver("src", 0).await.unwrap();
    c.send(
        0,
        Performative::Flow(Flow {
            next_incoming_id: Some(0),
            incoming_window: 3,
            next_outgoing_id: 0,
            outgoing_window: 100,
            handle: Some(h),
            delivery_count: Some(0),
            link_credit: Some(100),
            ..Default::default()
        }),
    )
    .await;
    for i in 0..10u8 {
        push(&shared, "src", &[i], "x", true);
    }
    wake(&shared, "src");
    c.pump_for(Duration::from_millis(300)).await;
    assert_eq!(c.links[&h].inbox.len(), 3, "only the window's worth arrived");
    // refresh the window: next-incoming-id = what we got (3), window 100
    c.send(
        0,
        Performative::Flow(Flow {
            next_incoming_id: Some(3),
            incoming_window: 100,
            next_outgoing_id: 0,
            outgoing_window: 100,
            ..Default::default()
        }),
    )
    .await;
    c.pump_for(Duration::from_millis(300)).await;
    assert_eq!(c.links[&h].inbox.len(), 10);
}

/// Open a connection whose session says `incoming_window` in its begin, attach a receiver link
/// on `src` (the server sends on it) and give it `credit` through a raw flow frame.
async fn windowed_receiver(cfg: ServerConfig, incoming_window: u32, credit: u32) -> (TestClient, Arc<Shared>, u32) {
    let (addr, shared) = start(cfg).await;
    let mut c = TestClient::connect(addr).await;
    c.open("h").await;
    c.send(0, Performative::Begin(Begin { next_outgoing_id: 0, incoming_window, outgoing_window: u32::MAX, ..Default::default() })).await;
    c.wait_until("begin", |c| c.peer_begin.is_some()).await;
    let h = c.attach_receiver("src", 0).await.unwrap();
    let next_in = c.peer_begin.as_ref().unwrap().next_outgoing_id;
    c.send(
        0,
        Performative::Flow(Flow {
            next_incoming_id: Some(next_in),
            incoming_window,
            next_outgoing_id: 0,
            outgoing_window: u32::MAX,
            handle: Some(h),
            delivery_count: Some(0),
            link_credit: Some(credit),
            ..Default::default()
        }),
    )
    .await;
    (c, shared, h)
}

#[tokio::test]
async fn deliveries_flow_when_the_clients_incoming_window_is_2_pow_31_or_more() {
    // The Rust Event Hubs SDK begins its session with incoming-window = 4294967295.
    for window in [2048u32, i32::MAX as u32, 1 << 31, (1 << 31) + 1, u32::MAX - 1, u32::MAX] {
        let (mut c, shared, h) = windowed_receiver(ServerConfig::default(), window, 300).await;
        for i in 0..5u8 {
            push(&shared, "src", &[i], "x", true);
        }
        wake(&shared, "src");
        c.pump_for(Duration::from_millis(300)).await;
        assert_eq!(c.links[&h].inbox.len(), 5, "window {window}");
    }
}

#[tokio::test]
async fn a_link_credit_of_2_pow_31_or_more_is_a_plain_unsigned_number() {
    for credit in [1u32 << 31, u32::MAX] {
        let (mut c, shared, h) = windowed_receiver(ServerConfig::default(), u32::MAX, credit).await;
        for i in 0..5u8 {
            push(&shared, "src", &[i], "x", true);
        }
        wake(&shared, "src");
        c.pump_for(Duration::from_millis(300)).await;
        assert_eq!(c.links[&h].inbox.len(), 5, "credit {credit}");
    }
}

#[tokio::test]
async fn ids_that_wrap_at_2_pow_32_keep_the_stream_going() {
    // The server's session starts 3 below the wrap; a stream of 12 transfers crosses it. The client's
    // flows (window 4 then 2^32-1) carry next-incoming-id values on both sides of the wrap.
    let start_id = u32::MAX - 3;
    let cfg = ServerConfig { initial_outgoing_id: start_id, ..Default::default() };
    let (addr, shared) = start(cfg).await;
    let mut c = TestClient::connect(addr).await;
    c.open("h").await;
    c.send(0, Performative::Begin(Begin { next_outgoing_id: 0, incoming_window: 4, outgoing_window: u32::MAX, ..Default::default() }))
        .await;
    c.wait_until("begin", |c| c.peer_begin.is_some()).await;
    assert_eq!(c.peer_begin.as_ref().unwrap().next_outgoing_id, start_id);
    let h = c.attach_receiver("src", 0).await.unwrap();
    let flow = |next_in: u32, window: u32, dc: u32, credit: u32| {
        Performative::Flow(Flow {
            next_incoming_id: Some(next_in),
            incoming_window: window,
            next_outgoing_id: 0,
            outgoing_window: u32::MAX,
            handle: Some(h),
            delivery_count: Some(dc),
            link_credit: Some(credit),
            ..Default::default()
        })
    };
    c.send(0, flow(start_id, 4, 0, 100)).await;
    for i in 0..12u8 {
        push(&shared, "src", &[i], "x", true);
    }
    wake(&shared, "src");
    c.pump_for(Duration::from_millis(300)).await;
    assert_eq!(c.links[&h].inbox.len(), 4, "the window of 4 is respected before the wrap");
    // 4 transfers seen: next-incoming-id is now u32::MAX + 1 = 0 (wrapped). Window of 4 again: 4 more
    // arrive, and the window that crosses the wrap is not read as a negative number.
    c.send(0, flow(start_id.wrapping_add(4), 4, 4, 100)).await;
    c.pump_for(Duration::from_millis(300)).await;
    assert_eq!(c.links[&h].inbox.len(), 8);
    c.send(0, flow(start_id.wrapping_add(8), u32::MAX, 8, 100)).await;
    c.pump_for(Duration::from_millis(300)).await;
    assert_eq!(c.links[&h].inbox.len(), 12);
    // a flow written before the peer saw the last transfers (next-incoming-id is behind) counts them
    // as in flight: window 2^32-1 and 3 in flight leaves 2^32-4, still open
    for i in 12..16u8 {
        push(&shared, "src", &[i], "x", true);
    }
    c.send(0, flow(start_id.wrapping_add(9), u32::MAX, 12, 100)).await;
    c.pump_for(Duration::from_millis(300)).await;
    assert_eq!(c.links[&h].inbox.len(), 16);
}

// ---- detach, close, heartbeats ----------------------------------------------------------------

#[tokio::test]
async fn peer_detach_notifies_the_service_and_is_answered() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_sender("q").await.unwrap();
    assert!(c.detach(h).await.is_none());
    assert!(shared.events.lock().iter().any(|e| e == "detach q peer"));
}

#[tokio::test]
async fn peer_detach_with_error_is_passed_on() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_sender("q").await.unwrap();
    c.send(0, Performative::Detach(Detach { handle: h, closed: true, error: Some(Error::new("x:y", "z")) })).await;
    c.wait_until("detach", |c| c.links[&h].detached.is_some()).await;
    assert!(shared.events.lock().iter().any(|e| e == "detach q peer-error:x:y"));
}

#[tokio::test]
async fn service_detaches_a_link_with_an_error() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_sender("q").await.unwrap();
    shared.handles.lock().get("q").unwrap().detach(Some(Error::new(condition::RESOURCE_DELETED, "entity deleted")));
    let err = c.wait_detached(h).await.unwrap();
    assert_eq!(err.condition, condition::RESOURCE_DELETED);
    // the peer answers with its detach; no further frames come back and the link can be reused
    c.send(0, Performative::Detach(Detach { handle: h, closed: true, error: None })).await;
    assert!(c.attach_sender("q").await.is_ok());
    assert!(shared.events.lock().iter().any(|e| e == "detach q local"));
}

#[tokio::test]
async fn service_closes_the_connection_with_an_error() {
    let (mut c, shared) = connected(ServerConfig::default()).await;
    let h = c.attach_sender("q").await.unwrap();
    shared.conn.lock().as_ref().unwrap().close(Some(Error::new(condition::CONNECTION_FORCED, "shutting down")));
    c.wait_until("close", |c| c.closed.is_some()).await;
    assert_eq!(c.closed.clone().unwrap().unwrap().condition, condition::CONNECTION_FORCED);
    c.send(0, Performative::Close(Close { error: None })).await;
    eventually("close event", || shared.events.lock().iter().any(|e| e == "close")).await;
    let _ = h;
}

#[tokio::test]
async fn dropping_the_socket_releases_every_link() {
    let (addr, shared) = start(ServerConfig::default()).await;
    {
        let mut c = TestClient::connect(addr).await;
        c.open_session("h").await;
        c.attach_sender("q").await.unwrap();
        c.attach_receiver("src", 1).await.unwrap();
    }
    eventually("link and connection cleanup", || {
        let e = shared.events.lock();
        e.iter().filter(|x| x.ends_with("connection-closed")).count() == 2 && e.iter().any(|x| x == "close")
    })
    .await;
}

#[tokio::test]
async fn server_sends_heartbeats_when_the_peer_asks_for_an_idle_timeout() {
    let (addr, _) = start(ServerConfig::default()).await;
    let mut c = TestClient::connect(addr).await;
    c.open_with(Open { container_id: "c".into(), idle_timeout: Some(400), ..Default::default() }).await;
    c.pump_for(Duration::from_millis(1300)).await;
    assert!(c.heartbeats >= 3, "got {} heartbeats in 1.3s with a 400ms idle timeout", c.heartbeats);
}

#[tokio::test]
async fn idle_peer_is_closed_after_the_advertised_idle_timeout() {
    let cfg = ServerConfig { idle_timeout: Some(Duration::from_millis(300)), ..Default::default() };
    let (addr, _) = start(cfg).await;
    let mut c = TestClient::connect(addr).await;
    let o = c.open("h").await;
    assert_eq!(o.idle_timeout, Some(300));
    c.pump_for(Duration::from_millis(1500)).await;
    assert_eq!(c.closed.clone().unwrap().unwrap().condition, condition::RESOURCE_LIMIT_EXCEEDED);
}

#[tokio::test]
async fn heartbeats_from_the_peer_keep_the_connection_alive() {
    let cfg = ServerConfig { idle_timeout: Some(Duration::from_millis(400)), ..Default::default() };
    let (addr, _) = start(cfg).await;
    let mut c = TestClient::connect(addr).await;
    c.open("h").await;
    for _ in 0..8 {
        c.write_frame(&azemu_amqp::frame::Frame { channel: 0, body: azemu_amqp::frame::FrameBody::Empty }).await;
        c.pump_for(Duration::from_millis(150)).await;
    }
    assert!(c.closed.is_none(), "{:?}", c.closed);
    assert!(c.close().await.is_none());
}

#[tokio::test]
async fn many_concurrent_connections() {
    let (addr, shared) = start(ServerConfig::default()).await;
    let mut tasks = Vec::new();
    for i in 0..20 {
        tasks.push(tokio::spawn(async move {
            let mut c = TestClient::connect(addr).await;
            c.open_session("h").await;
            let h = c.attach_sender("q").await.unwrap();
            for _ in 0..10 {
                assert_eq!(c.send_message_wait(h, &msg(&format!("{i}"))).await, Some(DeliveryState::Accepted));
            }
            c.close().await;
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(shared.received.lock().len(), 200);
}

// ---- limits -----------------------------------------------------------------------------------

fn raw_frame(body: Performative, payload: &[u8]) -> azemu_amqp::frame::Frame {
    azemu_amqp::frame::Frame { channel: 0, body: azemu_amqp::frame::FrameBody::Amqp(body, bytes::Bytes::copy_from_slice(payload)) }
}

#[tokio::test]
async fn connections_over_the_limit_are_closed_with_resource_limit_exceeded() {
    let cfg = ServerConfig { max_connections: 2, ..Default::default() };
    let (addr, _) = start(cfg).await;
    let mut a = TestClient::connect(addr).await;
    a.open_session("h").await;
    let mut b = TestClient::connect(addr).await;
    b.open_session("h").await;
    let mut c = TestClient::connect(addr).await;
    c.open("h").await;
    c.wait_until("close", |c| c.closed.is_some()).await;
    let e = c.closed.clone().unwrap().unwrap();
    assert_eq!(e.condition, condition::RESOURCE_LIMIT_EXCEEDED);
    assert!(e.description.unwrap_or_default().contains("2 concurrent connections"));
    // a slot is free again once a connection ends
    assert!(a.close().await.is_none());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut d = TestClient::connect(addr).await;
    d.open_session("h").await;
    assert!(d.close().await.is_none());
    assert!(b.close().await.is_none());
}

#[tokio::test]
async fn a_session_over_the_limit_closes_the_connection() {
    let cfg = ServerConfig { max_sessions: 2, ..Default::default() };
    let (addr, _) = start(cfg).await;
    let mut c = TestClient::connect(addr).await;
    c.open_session("h").await;
    c.begin_another_session().await;
    c.send(2, Performative::Begin(Begin { next_outgoing_id: 0, incoming_window: 100, outgoing_window: 100, ..Default::default() })).await;
    c.wait_until("close", |c| c.closed.is_some()).await;
    assert_eq!(c.closed.clone().unwrap().unwrap().condition, condition::RESOURCE_LIMIT_EXCEEDED);
}

#[tokio::test]
async fn a_link_over_the_limit_is_refused_and_the_others_keep_working() {
    let cfg = ServerConfig { max_links: 3, ..Default::default() };
    let (addr, shared) = start(cfg).await;
    let mut c = TestClient::connect(addr).await;
    c.open_session("h").await;
    let h1 = c.attach_sender("q1").await.unwrap();
    let _h2 = c.attach_sender("q2").await.unwrap();
    let _h3 = c.attach_sender("q3").await.unwrap();
    let err = c.attach_sender("q4").await.unwrap_err().unwrap();
    assert_eq!(err.condition, condition::RESOURCE_LIMIT_EXCEEDED);
    // the service never heard of the fourth
    assert!(!shared.events.lock().iter().any(|e| e == "attach q4"));
    assert_eq!(c.send_message_wait(h1, &msg("still works")).await, Some(DeliveryState::Accepted));
    // a link that ended frees its place
    assert!(c.detach(h1).await.is_none());
    assert!(c.attach_sender("q5").await.is_ok());
}

#[tokio::test]
async fn a_handle_above_handle_max_ends_the_session() {
    let (addr, _) = start(ServerConfig::default()).await;
    let mut c = TestClient::connect(addr).await;
    c.open_session("h").await;
    let mut a = Attach::new("big-handle", 70_000, Role::Sender);
    a.source = Some(Source::with_address("s"));
    a.target = Some(TargetKind::Target(Target::with_address("q")));
    c.send(0, Performative::Attach(a)).await;
    c.wait_until("end", |c| c.session_ended.is_some()).await;
    assert_eq!(c.session_ended.clone().unwrap().unwrap().condition, condition::NOT_ALLOWED);
}

#[tokio::test]
async fn half_received_messages_are_bounded_per_connection() {
    let cfg = ServerConfig { max_partial_bytes: 2 * 1024 * 1024, max_frame_size: 65536, ..Default::default() };
    let (addr, _) = start(cfg).await;
    let mut c = TestClient::connect(addr).await;
    c.open_session("h").await;
    let l1 = c.attach_sender("a").await.unwrap();
    let l2 = c.attach_sender("b").await.unwrap();
    let chunk = vec![7u8; 60_000];
    // two messages that never finish: 1.2 MB each on two links
    for round in 0..20 {
        for (l, id) in [(l1, 0u32), (l2, 1u32)] {
            let t = Transfer {
                handle: l,
                delivery_id: if round == 0 { Some(id) } else { None },
                delivery_tag: if round == 0 { Some(vec![id as u8]) } else { None },
                message_format: if round == 0 { Some(0) } else { None },
                settled: Some(false),
                more: true,
                ..Default::default()
            };
            if c.closed.is_some() {
                break;
            }
            c.write_frame(&raw_frame(Performative::Transfer(t), &chunk)).await;
            c.pump_for(Duration::from_millis(5)).await;
        }
    }
    c.wait_until("close", |c| c.closed.is_some()).await;
    assert_eq!(c.closed.clone().unwrap().unwrap().condition, condition::RESOURCE_LIMIT_EXCEEDED);
}

#[tokio::test]
async fn a_tiny_peer_idle_timeout_does_not_make_the_server_send_a_frame_every_millisecond() {
    let (addr, _) = start(ServerConfig::default()).await;
    let mut c = TestClient::connect(addr).await;
    c.open_with(Open { container_id: "c".into(), idle_timeout: Some(1), ..Default::default() }).await;
    c.pump_for(Duration::from_millis(1000)).await;
    assert!(c.heartbeats <= 15, "got {} empty frames in one second", c.heartbeats);
}

#[allow(dead_code)]
fn _value_helper() -> Value {
    Value::Null
}
