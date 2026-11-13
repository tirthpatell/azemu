//! The per-connection task: protocol header and SASL negotiation, the `open`/`close` exchange,
//! the select loop (socket, commands from service handles, timers) and frame dispatch.
//! Session and link frames are handled in `session.rs` and `link.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;
use tracing::{debug, trace};

use crate::api::*;
use crate::error::{condition, Error};
use crate::frame::*;
use crate::performatives::*;
use crate::sasl::{outcome, Sasl};
use crate::state::*;

/// Stop pulling new deliveries while this much output is waiting for the socket.
const MAX_PENDING_OUTPUT: usize = 2 * 1024 * 1024;
/// How long to wait for the peer's `close` after we sent ours.
const CLOSE_WAIT: Duration = Duration::from_secs(5);
/// Stop reading from the socket while this much output is waiting for it: a client that sends
/// requests and never reads the answers cannot make the buffer grow without bound.
const MAX_PENDING_OUTPUT_BEFORE_INPUT_PAUSES: usize = 8 * 1024 * 1024;
/// A peer's idle-timeout below this is treated as this: we send an empty frame every half of it,
/// and a tiny value would make that a busy loop.
const MIN_PEER_IDLE: Duration = Duration::from_millis(200);

/// Counts one connection against `ServerConfig::max_connections` until dropped.
struct ConnectionSlot(Arc<std::sync::atomic::AtomicUsize>);

impl ConnectionSlot {
    fn acquire(cfg: &ServerConfig) -> Option<ConnectionSlot> {
        use std::sync::atomic::Ordering::SeqCst;
        let n = cfg.connection_count.fetch_add(1, SeqCst);
        let slot = ConnectionSlot(cfg.connection_count.clone());
        if n >= cfg.max_connections {
            return None; // dropping `slot` gives the count back
        }
        Some(slot)
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(crate) struct Conn {
    pub cfg: Arc<ServerConfig>,
    pub handler: Box<dyn ConnectionHandler>,
    pub tx: UnboundedSender<Command>,
    pub out: BytesMut,
    scratch: Vec<u8>,
    pub sessions: HashMap<u16, Session>,
    pub link_index: HashMap<LinkId, (u16, u32)>,
    pub next_link_id: LinkId,
    pub peer_max_frame: u32,
    peer_idle: Option<Duration>,
    open_received: bool,
    close_sent: bool,
    /// Stop once the output buffer is flushed.
    pub done: bool,
    pub pump_needed: bool,
    last_recv: Instant,
    last_sent: Instant,
    started: Instant,
    close_deadline: Option<Instant>,
}

/// Serve one accepted connection until it ends. `io` has not been read from yet.
pub async fn serve_connection<IO>(mut io: IO, info: ConnectionInfo, factory: SharedFactory, cfg: Arc<ServerConfig>)
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut inbuf = BytesMut::with_capacity(8192);
    let hs = tokio::time::timeout(cfg.handshake_timeout, handshake(&mut io, &mut inbuf, &cfg)).await;
    match hs {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            debug!("amqp handshake with {} failed: {e}", info.peer);
            return;
        }
        Err(_) => {
            debug!("amqp handshake with {} timed out", info.peer);
            return;
        }
    }
    let Some(_slot) = ConnectionSlot::acquire(&cfg) else {
        debug!("amqp connection from {} refused: {} connections already", info.peer, cfg.max_connections);
        let open = Open {
            container_id: cfg.container_id.clone(),
            max_frame_size: cfg.max_frame_size,
            channel_max: cfg.channel_max,
            ..Default::default()
        };
        let close = Close {
            error: Some(Error::new(
                condition::RESOURCE_LIMIT_EXCEEDED,
                format!("The limit of {} concurrent connections has been reached.", cfg.max_connections),
            )),
        };
        let _ = write_frame(&mut io, &Frame { channel: 0, body: FrameBody::Amqp(Performative::Open(open), bytes::Bytes::new()) }).await;
        let _ = write_frame(&mut io, &Frame { channel: 0, body: FrameBody::Amqp(Performative::Close(close), bytes::Bytes::new()) }).await;
        let _ = io.shutdown().await;
        return;
    };
    let (tx, rx) = unbounded_channel();
    let handler = factory.new_connection(&info, ConnHandle { tx: tx.clone() });
    let now = Instant::now();
    let conn = Conn {
        cfg,
        handler,
        tx,
        out: BytesMut::with_capacity(8192),
        scratch: Vec::new(),
        sessions: HashMap::new(),
        link_index: HashMap::new(),
        next_link_id: 1,
        peer_max_frame: MIN_MAX_FRAME_SIZE,
        peer_idle: None,
        open_received: false,
        close_sent: false,
        done: false,
        pump_needed: false,
        last_recv: now,
        last_sent: now,
        started: now,
        close_deadline: None,
    };
    conn.run(io, inbuf, rx, &info).await;
}

async fn read_exact_buf<IO: AsyncRead + Unpin>(io: &mut IO, buf: &mut BytesMut, n: usize) -> std::io::Result<()> {
    while buf.len() < n {
        if io.read_buf(buf).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
    }
    Ok(())
}

async fn write_frame<IO: AsyncWrite + Unpin>(io: &mut IO, frame: &Frame) -> std::io::Result<()> {
    io.write_all(&frame_bytes(frame)).await
}

fn is_amqp_header(h: &[u8], proto: u8) -> bool {
    h.len() >= 8 && &h[..4] == b"AMQP" && h[4] == proto && h[5] == 1 && h[6] == 0 && h[7] == 0
}

/// Protocol header exchange, optionally with SASL first. On return the AMQP header has been
/// answered and `inbuf` may hold the start of the peer's `open`.
async fn handshake<IO: AsyncRead + AsyncWrite + Unpin>(io: &mut IO, inbuf: &mut BytesMut, cfg: &ServerConfig) -> std::io::Result<()> {
    read_exact_buf(io, inbuf, 8).await?;
    let hdr = inbuf.split_to(8);
    if is_amqp_header(&hdr, 3) {
        io.write_all(&HEADER_SASL).await?;
        write_frame(io, &Frame { channel: 0, body: FrameBody::Sasl(Sasl::Mechanisms { mechanisms: cfg.sasl_mechanisms.clone() }) }).await?;
        // wait for sasl-init (answer a challenge-less exchange: every mechanism and credential is accepted)
        loop {
            match decode_frame(inbuf, 65536) {
                Ok(Some(Frame { body: FrameBody::Sasl(Sasl::Init { mechanism, .. }), .. })) => {
                    trace!("sasl init with mechanism {mechanism}");
                    write_frame(
                        io,
                        &Frame { channel: 0, body: FrameBody::Sasl(Sasl::Outcome { code: outcome::OK, additional_data: None }) },
                    )
                    .await?;
                    break;
                }
                Ok(Some(Frame { body: FrameBody::Sasl(Sasl::Response { .. }), .. })) => {
                    write_frame(
                        io,
                        &Frame { channel: 0, body: FrameBody::Sasl(Sasl::Outcome { code: outcome::OK, additional_data: None }) },
                    )
                    .await?;
                    break;
                }
                Ok(Some(_)) | Err(_) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad SASL frame")),
                Ok(None) => {
                    if io.read_buf(inbuf).await? == 0 {
                        return Err(std::io::ErrorKind::UnexpectedEof.into());
                    }
                }
            }
        }
        read_exact_buf(io, inbuf, 8).await?;
        let hdr2 = inbuf.split_to(8);
        if !is_amqp_header(&hdr2, 0) {
            io.write_all(&HEADER_AMQP).await?;
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "expected AMQP header after SASL"));
        }
        io.write_all(&HEADER_AMQP).await?;
        Ok(())
    } else if is_amqp_header(&hdr, 0) {
        io.write_all(&HEADER_AMQP).await?;
        Ok(())
    } else {
        // Unsupported protocol (TLS inside AMQP, wrong version, not AMQP): answer with what we speak.
        let _ = io.write_all(&HEADER_AMQP).await;
        let _ = io.shutdown().await;
        Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unsupported protocol header"))
    }
}

impl Conn {
    async fn run<IO>(mut self, io: IO, mut inbuf: BytesMut, mut rx: UnboundedReceiver<Command>, info: &ConnectionInfo)
    where
        IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut rd, mut wr) = tokio::io::split(io);
        if !inbuf.is_empty() {
            self.process_input(&mut inbuf);
        }
        loop {
            self.pump();
            if self.done && self.out.is_empty() {
                break;
            }
            let deadline = self.next_deadline();
            tokio::select! {
                r = rd.read_buf(&mut inbuf), if !self.done && self.out.len() <= MAX_PENDING_OUTPUT_BEFORE_INPUT_PAUSES => {
                    match r {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            self.last_recv = Instant::now();
                            self.process_input(&mut inbuf);
                        }
                    }
                }
                w = wr.write(&self.out), if !self.out.is_empty() => {
                    match w {
                        Ok(0) | Err(_) => break,
                        Ok(n) => self.out.advance(n),
                    }
                }
                cmd = rx.recv() => {
                    if let Some(c) = cmd {
                        self.command(c);
                        // drain whatever else is queued so one iteration handles a burst
                        while let Ok(c) = rx.try_recv() {
                            self.command(c);
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => self.on_timer(),
            }
        }
        let _ = wr.shutdown().await;
        self.finish(info);
    }

    /// Tell the service that every link and the connection are gone.
    fn finish(&mut self, info: &ConnectionInfo) {
        let ids: Vec<(u16, u32)> = self.sessions.iter().flat_map(|(c, s)| s.links.keys().map(move |h| (*c, *h))).collect();
        for (ch, h) in ids {
            self.notify_detach(ch, h, DetachReason::ConnectionClosed);
        }
        self.handler.on_close();
        debug!("amqp connection from {} closed", info.peer);
    }

    fn next_deadline(&self) -> Instant {
        let far = Instant::now() + Duration::from_secs(3600);
        let mut d = far;
        if let Some(idle) = self.cfg.idle_timeout {
            d = d.min(self.last_recv + idle);
        }
        if !self.open_received {
            d = d.min(self.started + self.cfg.handshake_timeout);
        }
        if let Some(p) = self.peer_idle {
            d = d.min(self.last_sent + p / 2);
        }
        if let Some(c) = self.close_deadline {
            d = d.min(c);
        }
        d
    }

    fn on_timer(&mut self) {
        let now = Instant::now();
        if let Some(c) = self.close_deadline {
            if now >= c {
                self.done = true;
                return;
            }
        }
        if !self.open_received && now >= self.started + self.cfg.handshake_timeout {
            self.fatal(Error::new(condition::RESOURCE_LIMIT_EXCEEDED, "no open frame received"));
            return;
        }
        if let Some(idle) = self.cfg.idle_timeout {
            if now >= self.last_recv + idle && !self.close_sent {
                self.fatal(Error::new(condition::RESOURCE_LIMIT_EXCEEDED, "idle timeout expired"));
                return;
            }
        }
        if let Some(p) = self.peer_idle {
            if now >= self.last_sent + p / 2 {
                self.send_frame(&Frame { channel: 0, body: FrameBody::Empty });
            }
        }
    }

    // ---- sending -------------------------------------------------------------------------

    pub(crate) fn send_frame(&mut self, f: &Frame) {
        self.scratch.clear();
        encode_frame(f, &mut self.scratch);
        self.out.extend_from_slice(&self.scratch);
        self.last_sent = Instant::now();
    }

    pub(crate) fn send(&mut self, channel: u16, p: Performative) {
        trace!("-> [{channel}] {p:?}");
        self.send_frame(&Frame { channel, body: FrameBody::Amqp(p, bytes::Bytes::new()) });
    }

    pub(crate) fn output_backed_up(&self) -> bool {
        self.out.len() > MAX_PENDING_OUTPUT
    }

    /// Send a `close` with the error and stop once it is flushed.
    pub(crate) fn fatal(&mut self, error: Error) {
        debug!("closing connection: {error}");
        if !self.close_sent {
            self.close_sent = true;
            self.send(0, Performative::Close(Close { error: Some(error) }));
        }
        self.done = true;
    }

    // ---- receiving -----------------------------------------------------------------------

    fn process_input(&mut self, inbuf: &mut BytesMut) {
        loop {
            if self.done {
                inbuf.clear();
                return;
            }
            match decode_frame(inbuf, self.cfg.max_frame_size) {
                Ok(None) => return,
                Ok(Some(frame)) => {
                    if let Err(e) = self.on_frame(frame) {
                        self.fatal(e);
                    }
                }
                Err(FrameError::Framing(m)) => return self.fatal(Error::new(condition::CONNECTION_FRAMING_ERROR, m)),
                Err(FrameError::Decode(m)) => return self.fatal(Error::new(condition::DECODE_ERROR, m)),
            }
        }
    }

    fn on_frame(&mut self, frame: Frame) -> Result<(), Error> {
        let channel = frame.channel;
        match frame.body {
            FrameBody::Empty => Ok(()),
            FrameBody::Sasl(_) => Err(Error::new(condition::NOT_ALLOWED, "unexpected SASL frame")),
            FrameBody::Amqp(perf, payload) => {
                trace!("<- [{channel}] {perf:?}");
                if !self.open_received && !matches!(perf, Performative::Open(_)) {
                    return Err(Error::new(condition::NOT_ALLOWED, "expected open"));
                }
                match perf {
                    Performative::Open(o) => self.on_open(o),
                    Performative::Close(c) => {
                        if let Some(e) = &c.error {
                            debug!("peer closed with error: {e}");
                        }
                        if !self.close_sent {
                            self.close_sent = true;
                            self.send(0, Performative::Close(Close { error: None }));
                        }
                        self.done = true;
                        Ok(())
                    }
                    Performative::Begin(b) => self.on_begin(channel, b),
                    Performative::End(e) => self.on_end(channel, e),
                    Performative::Attach(a) => self.on_attach(channel, a),
                    Performative::Flow(f) => self.on_flow(channel, f),
                    Performative::Transfer(t) => self.on_transfer(channel, t, payload),
                    Performative::Disposition(d) => self.on_disposition(channel, d),
                    Performative::Detach(d) => self.on_detach(channel, d),
                }
            }
        }
    }

    fn on_open(&mut self, open: Open) -> Result<(), Error> {
        if self.open_received {
            return Err(Error::new(condition::ILLEGAL_STATE, "open received twice"));
        }
        self.open_received = true;
        self.peer_max_frame = open.max_frame_size.max(MIN_MAX_FRAME_SIZE);
        self.peer_idle = open.idle_timeout.filter(|t| *t > 0).map(|t| Duration::from_millis(t as u64).max(MIN_PEER_IDLE));
        self.handler.on_open(&open)?;
        let reply = Open {
            container_id: self.cfg.container_id.clone(),
            hostname: None,
            max_frame_size: self.cfg.max_frame_size,
            channel_max: self.cfg.channel_max,
            idle_timeout: self.cfg.idle_timeout.map(|d| d.as_millis().min(u32::MAX as u128) as u32),
            offered_capabilities: self.cfg.offered_capabilities.clone(),
            properties: self.cfg.open_properties.clone(),
            ..Default::default()
        };
        self.send(0, Performative::Open(reply));
        Ok(())
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Close(error) => {
                if !self.close_sent {
                    self.close_sent = true;
                    self.send(0, Performative::Close(Close { error }));
                    self.close_deadline = Some(Instant::now() + CLOSE_WAIT);
                }
            }
            other => self.link_command(other),
        }
    }

    pub(crate) fn pump(&mut self) {
        if self.pump_needed && !self.done && !self.close_sent {
            self.pump_needed = false;
            self.pump_links();
        }
    }
}
