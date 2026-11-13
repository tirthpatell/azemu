//! The service-facing API of the server: a trait the service implements per connection, and
//! handles it uses to push work back into the connection task.
//!
//! One tokio task owns each connection (socket, sessions, links, flow-control counters). The
//! [`ConnectionHandler`] is called inline from that task with plain `&mut self`, so it needs no
//! locks of its own and must not block. Everything that has to happen later or from another task
//! goes through a [`LinkHandle`] / [`ConnHandle`], which post commands to the connection task.
//!
//! Sending is pull based: the connection task owns link credit and the session window, and asks
//! the handler for the next delivery ([`ConnectionHandler::pull`]) only when both allow one. The
//! service calls [`LinkHandle::wake`] when something new is available for a sender link.

use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::Error;
use crate::performatives::{Attach, Open, RcvSettleMode, Role, SndSettleMode};
use crate::terminus::{DeliveryState, Source, TargetKind};
use crate::types::Value;

/// Identifies a link within one connection. Never reused.
pub type LinkId = u64;

/// Where a connection came from.
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub peer: SocketAddr,
    pub local: SocketAddr,
    pub tls: bool,
    pub server_name: Option<String>,
}

/// Server-wide settings.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub container_id: String,
    /// Largest frame we accept (advertised in `open`).
    pub max_frame_size: u32,
    pub channel_max: u16,
    /// Idle timeout we advertise; the connection is closed when nothing arrives for this long.
    pub idle_timeout: Option<Duration>,
    /// Transfers a peer may have in flight per session (our `incoming-window`).
    pub session_window: u32,
    /// Link credit we grant to peers that send to us, topped up as deliveries arrive.
    pub receiver_credit: u32,
    /// Default `max-message-size` of a link (a service may override per link).
    pub max_message_size: u64,
    /// SASL mechanisms offered when the peer starts with the SASL header.
    pub sasl_mechanisms: Vec<String>,
    /// Properties and capabilities in our `open`.
    pub open_properties: Option<Value>,
    pub offered_capabilities: Vec<String>,
    /// How long to wait for the protocol header and SASL exchange.
    pub handshake_timeout: Duration,
    /// Connections served at the same time through this config; one more is answered with an
    /// `open` and a `close` carrying `amqp:resource-limit-exceeded`. Azure allows 5,000 AMQP
    /// connections per namespace (Standard).
    pub max_connections: usize,
    /// Counts the connections of `max_connections`. Configs that share a counter share the limit.
    pub connection_count: Arc<AtomicUsize>,
    /// Sessions (`begin`) per connection. Another `begin` closes the connection with
    /// `amqp:resource-limit-exceeded`.
    pub max_sessions: usize,
    /// Links per connection (all sessions). Another `attach` is refused with
    /// `amqp:resource-limit-exceeded`.
    pub max_links: usize,
    /// Bytes of half-received multi-frame messages a connection may hold (all links together).
    /// More closes the connection with `amqp:resource-limit-exceeded`.
    pub max_partial_bytes: usize,
    /// The `next-outgoing-id` our sessions start with. AMQP lets a session start anywhere in the 32-bit
    /// range and the ids wrap; 0 is what Service Bus and Event Hubs use. Tests set a value near 2^32 to
    /// check the wrap.
    pub initial_outgoing_id: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            container_id: "azemu".to_string(),
            max_frame_size: 65536,
            channel_max: 4095,
            idle_timeout: Some(Duration::from_secs(240)),
            session_window: 2048,
            receiver_credit: 1000,
            max_message_size: 100 * 1024 * 1024,
            sasl_mechanisms: ["ANONYMOUS", "PLAIN", "EXTERNAL", "MSSBCBS"].iter().map(|s| s.to_string()).collect(),
            open_properties: None,
            offered_capabilities: vec![],
            handshake_timeout: Duration::from_secs(30),
            max_connections: 5000,
            connection_count: Arc::new(AtomicUsize::new(0)),
            max_sessions: 1000,
            max_links: 10_000,
            max_partial_bytes: 64 * 1024 * 1024,
            initial_outgoing_id: 0,
        }
    }
}

/// Creates one handler per accepted connection.
pub trait HandlerFactory: Send + Sync + 'static {
    fn new_connection(&self, info: &ConnectionInfo, conn: ConnHandle) -> Box<dyn ConnectionHandler>;
}

/// The peer's attach, as shown to the service.
pub struct AttachRequest {
    pub link: LinkId,
    /// The session (channel) the link is attached on.
    pub session: u16,
    /// Handle to wake, detach or settle on this link later.
    pub handle: LinkHandle,
    pub attach: Attach,
}

impl AttachRequest {
    /// Our role on this link: the peer's role flipped.
    pub fn local_role(&self) -> Role {
        self.attach.role.flip()
    }
}

/// What we put in the attach response. `None` fields echo the peer's value.
#[derive(Debug, Clone, Default)]
pub struct AttachAccept {
    pub source: Option<Source>,
    pub target: Option<TargetKind>,
    pub properties: Option<Value>,
    pub offered_capabilities: Vec<String>,
    pub max_message_size: Option<u64>,
    pub snd_settle_mode: Option<SndSettleMode>,
    pub rcv_settle_mode: Option<RcvSettleMode>,
}

#[allow(clippy::large_enum_variant)]
pub enum AttachDecision {
    Accept(AttachAccept),
    /// Answer with a null-terminus attach and a detach carrying `error`.
    Refuse(Error),
    /// The service will answer later with [`LinkHandle::complete_attach`].
    Defer,
}

/// A complete incoming message (all frames reassembled).
#[derive(Debug, Clone)]
pub struct IncomingDelivery {
    pub delivery_id: u32,
    pub tag: Vec<u8>,
    pub message_format: u32,
    /// The sender settled it already; no disposition is sent.
    pub settled: bool,
    /// State given on the transfer (for example a transactional state).
    pub state: Option<DeliveryState>,
    pub payload: Bytes,
}

pub enum TransferAction {
    /// Settle now with this outcome.
    Settle(DeliveryState),
    /// The service settles later with [`LinkHandle::settle`], or the delivery was pre-settled.
    Defer,
}

/// A message to send on a sender link.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub tag: Vec<u8>,
    pub message_format: u32,
    /// Send pre-settled (receive-and-delete).
    pub settled: bool,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum DetachReason {
    /// The peer detached the link.
    Peer {
        closed: bool,
        error: Option<Error>,
    },
    /// We detached it (service request, protocol error).
    Local,
    SessionEnded,
    ConnectionClosed,
}

/// Implemented by the service, one instance per connection.
///
/// All methods run on the connection task. They must return quickly.
pub trait ConnectionHandler: Send + 'static {
    /// The peer's `open` arrived. Return an error to close the connection with it.
    fn on_open(&mut self, _open: &Open) -> Result<(), Error> {
        Ok(())
    }

    /// The peer wants to attach a link.
    fn on_attach(&mut self, req: AttachRequest) -> AttachDecision;

    /// A complete message arrived on a link where we are the receiver.
    fn on_transfer(&mut self, link: LinkId, delivery: IncomingDelivery) -> TransferAction;

    /// The peer settled or changed the state of a delivery we sent. `tag` is the tag of the
    /// [`Outgoing`] that was pulled for it.
    ///
    /// When the peer sent an unsettled terminal state (it waits for us to settle: receiver
    /// settle mode `second`, or a client that wants to confirm the outcome), the connection
    /// answers with a settled disposition carrying the returned state, or the peer's own state
    /// when `None` is returned. A broker returns `Some(Rejected{error})` to say "I could not
    /// apply that", which is how Service Bus reports a lost message lock.
    fn on_disposition(&mut self, _link: LinkId, _tag: &[u8], _state: Option<DeliveryState>, _settled: bool) -> Option<DeliveryState> {
        None
    }

    /// The peer changed the credit on a sender link (informational: `pull` does the work).
    fn on_flow(&mut self, _link: LinkId, _credit: u32, _drain: bool) {}

    /// Next message for a sender link that has credit and window. `None` when nothing is ready;
    /// the service then calls [`LinkHandle::wake`] when something arrives.
    fn pull(&mut self, link: LinkId) -> Option<Outgoing>;

    /// The link is gone. Called exactly once per attached link.
    fn on_detach(&mut self, link: LinkId, reason: DetachReason);

    /// The connection ended (close, error or socket drop), after every link's `on_detach`.
    fn on_close(&mut self) {}
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum Command {
    Wake(LinkId),
    Detach(LinkId, Option<Error>),
    Settle { link: LinkId, delivery_id: u32, state: DeliveryState },
    CompleteAttach(LinkId, Result<AttachAccept, Error>),
    Close(Option<Error>),
}

/// Post work to one link of a connection from any task. Cheap to clone; never blocks.
#[derive(Clone)]
pub struct LinkHandle {
    pub(crate) link: LinkId,
    pub(crate) tx: UnboundedSender<Command>,
}

impl LinkHandle {
    pub fn id(&self) -> LinkId {
        self.link
    }
    /// A sender link may have something to send: the connection task will call `pull`.
    /// Returns false when the connection is gone.
    pub fn wake(&self) -> bool {
        self.tx.send(Command::Wake(self.link)).is_ok()
    }
    /// Detach and close the link, with an error if given.
    pub fn detach(&self, error: Option<Error>) {
        let _ = self.tx.send(Command::Detach(self.link, error));
    }
    /// Settle an incoming delivery that `on_transfer` deferred.
    pub fn settle(&self, delivery_id: u32, state: DeliveryState) {
        let _ = self.tx.send(Command::Settle { link: self.link, delivery_id, state });
    }
    /// Answer a deferred attach.
    pub fn complete_attach(&self, result: Result<AttachAccept, Error>) {
        let _ = self.tx.send(Command::CompleteAttach(self.link, result));
    }
    pub fn is_connected(&self) -> bool {
        !self.tx.is_closed()
    }
}

/// Post work to the connection.
#[derive(Clone)]
pub struct ConnHandle {
    pub(crate) tx: UnboundedSender<Command>,
}

impl ConnHandle {
    /// Close the connection (with an error if given).
    pub fn close(&self, error: Option<Error>) {
        let _ = self.tx.send(Command::Close(error));
    }
    pub fn link(&self, link: LinkId) -> LinkHandle {
        LinkHandle { link, tx: self.tx.clone() }
    }
}

pub type SharedFactory = Arc<dyn HandlerFactory>;
