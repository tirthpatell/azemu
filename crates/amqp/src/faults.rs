//! Fault injection for an AMQP service (`docs/faults.md`): a [`ConnectionHandler`] that asks the emulator's fault rules before
//! it passes a call on to the service's own handler.
//!
//! What is asked, and what a rule can do:
//!
//! | operation | asked when | `throttle` / `error` | `latency` | `drop` / `timeout` |
//! |---|---|---|---|---|
//! | `connect` | the peer's `open` arrives | the connection is closed with the error | `open` is answered late | socket closed / held |
//! | `send` | a message arrives on a link to an entity | the transfer is settled `rejected` with the error | the settlement is late | |
//! | `management` | a request arrives on `<entity>/$management` | as `send` | as `send` | |
//! | `receive` | the peer attaches a link to receive from an entity | the attach is refused with the error | the attach is answered late | socket closed / held |
//!
//! The `$cbs` node (the token exchange) and transaction coordinator links are never faulted.
//!
//! `receive` is asked when a receiving link is attached, not for each credit grant: a client that has a link open keeps it. A
//! refused attach is what the Python Service Bus client retries; a link that is detached with the error after it was attached is
//! kept by that client in a broken state and the error comes back until its retries are used up (see `docs/faults.md`).
//!
//! Throttling is the condition `com.microsoft:server-busy`: the Azure SDKs retry it (see `docs/faults.md` for what each one did).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use azemu_core::faults::{Action, FaultAnswer, Faults, Operation, Outcome};

use crate::api::*;
use crate::error::{condition, Error};
use crate::performatives::{Open, Role};
use crate::terminus::{DeliveryState, TargetKind};

/// `com.microsoft:server-busy`, the condition both Service Bus and Event Hubs use for a throttled request.
pub const SERVER_BUSY: &str = "com.microsoft:server-busy";
const TIMEOUT: &str = "com.microsoft:timeout";

/// How long a `timeout` rule with no `ms` holds a connection.
const HOLD_LIMIT: Duration = Duration::from_secs(3600);

/// What the wrapper needs from the service that uses it.
#[derive(Clone)]
pub struct FaultSetup {
    pub faults: Faults,
    /// `servicebus` or `eventhubs`: the `service` of a rule.
    pub service: &'static str,
    /// The namespace (the `account` of a rule) from the host name in the `open` frame.
    pub namespace_of: fn(Option<&str>) -> String,
    /// Shared by the handlers of one connection (a multiplexer makes one per service), so that a connection is asked about once.
    pub connect_asked: Arc<AtomicBool>,
}

impl FaultSetup {
    pub fn new(faults: Faults, service: &'static str, namespace_of: fn(Option<&str>) -> String) -> FaultSetup {
        FaultSetup { faults, service, namespace_of, connect_asked: Arc::new(AtomicBool::new(false)) }
    }
}

/// The same setup for another service on the same connection.
impl FaultSetup {
    pub fn for_service(&self, service: &'static str) -> FaultSetup {
        FaultSetup { service, ..self.clone() }
    }
}

/// A factory whose handlers are wrapped.
pub struct FaultFactory {
    pub inner: SharedFactory,
    pub faults: Faults,
    pub service: &'static str,
    pub namespace_of: fn(Option<&str>) -> String,
}

impl HandlerFactory for FaultFactory {
    fn new_connection(&self, info: &ConnectionInfo, conn: ConnHandle) -> Box<dyn ConnectionHandler> {
        let inner = self.inner.new_connection(info, conn.clone());
        wrap(inner, conn, FaultSetup::new(self.faults.clone(), self.service, self.namespace_of))
    }
}

/// Wrap a handler. With no rule the wrapper costs one atomic read per call.
pub fn wrap(inner: Box<dyn ConnectionHandler>, conn: ConnHandle, setup: FaultSetup) -> Box<dyn ConnectionHandler> {
    Box::new(FaultHandler { inner, conn, setup, namespace: String::new(), links: HashMap::new() })
}

enum Role2 {
    /// The peer sends to this address (we receive).
    Inbound,
    /// The peer receives from this address (we send).
    Outbound,
}

struct LinkMeta {
    role: Role2,
    /// The entity or node as the client named it, relative (`orders`, `hub/Partitions/0`); empty for a coordinator.
    address: String,
    /// `$management` request link, or the response link of one.
    management: bool,
    /// Never faulted (`$cbs`, a transaction coordinator).
    exempt: bool,
}

struct FaultHandler {
    inner: Box<dyn ConnectionHandler>,
    conn: ConnHandle,
    setup: FaultSetup,
    namespace: String,
    links: HashMap<LinkId, LinkMeta>,
}

/// What the rules decided, in terms of one call.
enum Verdict {
    Pass,
    Reject(Error),
    /// The connection was dropped or held: the call goes no further.
    Gone,
}

/// The address a link names, relative to the namespace: `amqps://ns.servicebus.windows.net/orders` and `/orders` are `orders`.
fn relative(address: &str) -> String {
    let rest = match address.split_once("://") {
        Some((_, after)) => after.split_once('/').map(|(_, p)| p).unwrap_or(""),
        None => address,
    };
    rest.trim_start_matches('/').to_string()
}

fn error_for(a: &FaultAnswer) -> Error {
    let status = a.status.unwrap_or(503);
    let condition = match (&a.code, a.throttle) {
        (Some(c), _) => c.clone(),
        (None, true) => SERVER_BUSY.to_string(),
        (None, false) => match status {
            404 => condition::NOT_FOUND.to_string(),
            401 | 403 => condition::UNAUTHORIZED_ACCESS.to_string(),
            408 => TIMEOUT.to_string(),
            429 | 503 => SERVER_BUSY.to_string(),
            400 => condition::INVALID_FIELD.to_string(),
            _ => condition::INTERNAL_ERROR.to_string(),
        },
    };
    let wait = a.retry_after_secs().map(|s| format!(" Please wait {s} seconds and try again.")).unwrap_or_default();
    let default = if a.throttle {
        format!("The request was terminated because the entity is being throttled.{wait}")
    } else {
        FaultAnswer::DEFAULT_MESSAGE.to_string()
    };
    Error::new(condition, a.message.clone().unwrap_or(default))
}

impl FaultHandler {
    fn active(&self) -> bool {
        self.setup.faults.is_active()
    }

    /// Ask the rules and carry out what is not an answer; log what fired.
    fn ask(&self, entity: &str, op: Operation) -> Verdict {
        let out: Outcome = self.setup.faults.messaging(self.setup.service, &self.namespace, entity, op);
        if out.fired.is_empty() {
            return Verdict::Pass;
        }
        tracing::info!(
            service = self.setup.service,
            account = %self.namespace,
            entity = %entity,
            operation = op.name(),
            fault = %out.kinds(),
            rule = %out.rules(),
            "amqp fault"
        );
        if !out.delay.is_zero() {
            self.conn.delay(out.delay);
        }
        match out.action {
            None => Verdict::Pass,
            Some(Action::Answer(a)) => Verdict::Reject(error_for(&a)),
            Some(Action::Drop) => {
                self.conn.abort();
                Verdict::Gone
            }
            Some(Action::Timeout(d)) => {
                self.conn.hold(d.unwrap_or(HOLD_LIMIT));
                Verdict::Gone
            }
        }
    }
}

impl ConnectionHandler for FaultHandler {
    fn on_open(&mut self, open: &Open) -> Result<(), Error> {
        self.namespace = (self.setup.namespace_of)(open.hostname.as_deref());
        if self.active() && !self.setup.connect_asked.swap(true, Ordering::SeqCst) {
            match self.ask("", Operation::Connect) {
                Verdict::Pass => {}
                Verdict::Reject(e) => return Err(e),
                // the connection ends at the next turn of its task; nothing more to do with it
                Verdict::Gone => return Ok(()),
            }
        }
        self.inner.on_open(open)
    }

    fn open_settings(&self) -> Option<OpenSettings> {
        self.inner.open_settings()
    }

    fn on_attach(&mut self, req: AttachRequest) -> AttachDecision {
        let role = req.local_role();
        let (address, exempt) = match (role, &req.attach.target, &req.attach.source) {
            (Role::Receiver, Some(TargetKind::Target(t)), _) => (t.address.clone().unwrap_or_default(), false),
            (Role::Receiver, Some(TargetKind::Coordinator(_)), _) => (String::new(), true),
            (Role::Sender, _, Some(s)) => (s.address.clone().unwrap_or_default(), false),
            _ => (String::new(), true),
        };
        let rel = relative(&address);
        let last = rel.rsplit('/').next().unwrap_or("");
        let cbs = last == "$cbs";
        let management = last == "$management";
        let address = if management { rel.trim_end_matches("$management").trim_end_matches('/').to_string() } else { rel };
        if self.active() && role == Role::Sender && !exempt && !cbs && !management {
            match self.ask(&address, Operation::Receive) {
                Verdict::Pass => {}
                // the peer's attach is answered with a detach that carries the error
                Verdict::Reject(e) => return AttachDecision::Refuse(e),
                // the connection ends at the next turn of its task
                Verdict::Gone => {}
            }
        }
        self.links.insert(
            req.link,
            LinkMeta {
                role: if role == Role::Receiver { Role2::Inbound } else { Role2::Outbound },
                address,
                management,
                exempt: exempt || cbs,
            },
        );
        self.inner.on_attach(req)
    }

    fn on_transfer(&mut self, link: LinkId, delivery: IncomingDelivery) -> TransferAction {
        if self.active() {
            if let Some(m) = self.links.get(&link).filter(|m| !m.exempt && matches!(m.role, Role2::Inbound)) {
                let op = if m.management { Operation::Management } else { Operation::Send };
                let address = m.address.clone();
                match self.ask(&address, op) {
                    Verdict::Pass => {}
                    // a message the service never saw: refused as `rejected`, which is what a client treats as not sent
                    Verdict::Reject(e) => return TransferAction::Settle(DeliveryState::Rejected { error: Some(e) }),
                    Verdict::Gone => return TransferAction::Defer,
                }
            }
        }
        self.inner.on_transfer(link, delivery)
    }

    fn on_disposition(&mut self, link: LinkId, tag: &[u8], state: Option<DeliveryState>, settled: bool) -> Option<DeliveryState> {
        self.inner.on_disposition(link, tag, state, settled)
    }

    fn on_flow(&mut self, link: LinkId, credit: u32, drain: bool) {
        self.inner.on_flow(link, credit, drain);
    }

    fn pull(&mut self, link: LinkId) -> Option<Outgoing> {
        self.inner.pull(link)
    }

    fn on_detach(&mut self, link: LinkId, reason: DetachReason) {
        self.links.remove(&link);
        self.inner.on_detach(link, reason);
    }

    fn on_close(&mut self) {
        self.inner.on_close();
    }
}
