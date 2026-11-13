//! The AMQP side of Service Bus: one `SbConn` per connection implements the connection
//! handler of `azemu-amqp` on top of the shared state.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use azemu_amqp::api::*;
use azemu_amqp::message::{Body, Message, BATCH_MESSAGE_FORMAT};
use azemu_amqp::performatives::{Role, SndSettleMode};
use azemu_amqp::terminus::{DeliveryState, Source, TargetKind};
use azemu_amqp::types::Value;
use azemu_amqp::{condition, Error as AmqpError};

use crate::core::*;
use crate::errors::{self, SbError};
use crate::model::*;
use crate::rules::Rule;
use crate::state::*;
use crate::store::*;

pub const SESSION_FILTER: &str = "com.microsoft:session-filter";
pub const LOCKED_UNTIL_UTC: &str = "com.microsoft:locked-until-utc";
pub const OPERATION_TIMEOUT: &str = "com.microsoft:timeout";
/// The link property of a send-via sender: the entity the messages are really for.
pub const TRANSFER_DESTINATION: &str = "com.microsoft:transfer-destination-address";
/// 100 ns ticks between 0001-01-01 and 1970-01-01.
const DOTNET_EPOCH_TICKS: i64 = 621_355_968_000_000_000;
const DEFAULT_SESSION_WAIT: Duration = Duration::from_secs(65);
/// Largest request a client may send on `$cbs` (a token) and on `<entity>/$management` (the
/// biggest request is `schedule-message` with a batch of messages).
pub const CBS_MAX_MESSAGE_SIZE: u64 = 1024 * 1024;
pub const MGMT_MAX_MESSAGE_SIZE: u64 = 16 * 1024 * 1024;

pub fn ms_to_ticks(ms: i64) -> i64 {
    ms * 10_000 + DOTNET_EPOCH_TICKS
}

pub struct SbFactory {
    pub shared: Arc<Shared>,
}

impl HandlerFactory for SbFactory {
    fn new_connection(&self, _info: &ConnectionInfo, conn: ConnHandle) -> Box<dyn ConnectionHandler> {
        self.shared.ensure_ticker();
        Box::new(SbConn {
            shared: self.shared.clone(),
            conn_id: self.shared.next_conn_id(),
            _conn: conn,
            ns: DEFAULT_NAMESPACE.to_string(),
            links: HashMap::new(),
            responders: HashMap::new(),
            pairing: HashMap::new(),
            txns: crate::txn::TxnTable::new(self.shared.txn_budget.clone()),
        })
    }
}

/// The namespace an `open` frame's hostname selects: the first DNS label, except for
/// `localhost`, IP addresses, an empty hostname and the bare `servicebus` label.
pub fn namespace_from_hostname(host: Option<&str>) -> String {
    let Some(h) = host.map(str::trim).filter(|h| !h.is_empty()) else { return DEFAULT_NAMESPACE.to_string() };
    let h = if let Some(rest) = h.strip_prefix('[') {
        // [::1]:5672
        let _ = rest;
        return DEFAULT_NAMESPACE.to_string();
    } else {
        h.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map(|(a, _)| a).unwrap_or(h)
    };
    if h.parse::<std::net::IpAddr>().is_ok() {
        return DEFAULT_NAMESPACE.to_string();
    }
    let first = h.split('.').next().unwrap_or("").to_lowercase();
    if first.is_empty() || first == "localhost" || first == "servicebus" {
        DEFAULT_NAMESPACE.to_string()
    } else {
        first
    }
}

/// What a link carries.
pub(crate) enum SbLink {
    /// We receive requests from the client on `$cbs` or `<entity>/$management`.
    RequestReceiver { node: Node, session: u16 },
    /// We send responses on the response link of a request/response pair. `session` and
    /// `reply_address` (the target of the client's receiving end) say which requests it answers.
    ResponseSender { node_key: String, session: u16, reply_address: Option<String>, outbox: VecDeque<Outgoing> },
    /// The client sends messages to a queue or topic.
    EntitySender { target: Address, session: u16, via: bool },
    /// The client receives from a queue, subscription or dead-letter queue.
    EntityReceiver(EntityRecv),
    /// The client's transaction controller sends `declare` and `discharge` here (`amqp:coordinator`).
    Coordinator,
}

#[derive(Clone)]
pub(crate) enum Node {
    Cbs,
    Management(String),
}

impl Node {
    fn key(&self) -> String {
        match self {
            Node::Cbs => "$cbs".to_string(),
            Node::Management(p) => format!("{}/$management", p.to_lowercase()),
        }
    }
}

pub(crate) struct EntityRecv {
    pub addr: Address,
    pub mode: ReceiveMode,
    pub session: Option<String>,
    pub handle: LinkHandle,
    /// Set while the receiver waits for "the next available session", and kept after the wait
    /// succeeded so the session it was given is known.
    pub wait: Option<Arc<SessionWait>>,
    /// The AMQP session (channel) the link is on.
    pub channel: u16,
}

/// Shared between a link and the task that waits for a session for it. It is only changed while
/// the service state lock is held, so "the link ended" and "a session was locked for it" cannot
/// both be missed.
pub(crate) struct SessionWait {
    state: parking_lot::Mutex<WaitState>,
}

enum WaitState {
    Waiting,
    /// The link, its session or the connection ended: nothing may be locked for it any more.
    Cancelled,
    Granted(String),
}

pub struct SbConn {
    pub(crate) shared: Arc<Shared>,
    pub(crate) conn_id: u64,
    _conn: ConnHandle,
    pub(crate) ns: String,
    pub(crate) links: HashMap<LinkId, SbLink>,
    pub(crate) responders: HashMap<String, Vec<LinkId>>,
    /// request and response links of each node, paired by attach order within a session
    pairing: HashMap<String, azemu_amqp::reply::Pairing>,
    /// transactions declared on this connection
    pub(crate) txns: crate::txn::TxnTable,
}

fn refuse(e: SbError) -> AttachDecision {
    AttachDecision::Refuse(e.to_amqp())
}

fn session_filter_of(src: &Source) -> Option<Option<String>> {
    let f = src.filter_get(SESSION_FILTER)?;
    Some(f.undescribed().as_str().map(str::to_string))
}

impl SbConn {
    pub(crate) fn owner(&self, link: LinkId) -> Owner {
        Owner { conn: self.conn_id, link }
    }

    fn waiter_key(&self, link: LinkId) -> (u64, u64) {
        (self.conn_id, link)
    }

    /// Make sure the entity named by `addr` exists, creating it when auto-creation is on.
    fn ensure_entity(&self, st: &mut State, addr: &Address, session: bool, send: bool, now: i64) -> Result<(), SbError> {
        let ns_name = self.ns.clone();
        let autocreate = self.shared.autocreate;
        let ns = st.ns(&ns_name);
        match addr {
            Address::Queue { name, .. } => {
                if ns.queue(name).is_some() || (send && ns.topic(name).is_some()) {
                    return Ok(());
                }
                if ns.topic(name).is_some() {
                    return Err(SbError::not_allowed(format!("'{name}' is a topic: receive from one of its subscriptions.")));
                }
                if !autocreate {
                    return Err(SbError::entity_not_found(name, &ns_name));
                }
                let mut s = Settings::queue_default(now);
                s.requires_session = session;
                ns.create_queue(name, s)?;
                if let Some(q) = ns.queue_mut(name) {
                    q.core.autocreated = true;
                    next_revision();
                }
                Ok(())
            }
            Address::Subscription { topic, name, .. } => {
                if ns.topic(topic).map(|t| t.subs.contains_key(&key(name))).unwrap_or(false) {
                    return Ok(());
                }
                if !autocreate {
                    return Err(SbError::entity_not_found(&format!("{topic}/Subscriptions/{name}"), &ns_name));
                }
                // A sender that attached first created a queue under this name; the subscription
                // shows it was meant to be a topic.
                if ns.queue(topic).map(|q| q.core.autocreated).unwrap_or(false) {
                    ns.queues.remove(&key(topic));
                    next_revision();
                }
                if ns.topic(topic).is_none() {
                    ns.create_topic(topic, Settings::topic_default(now))?;
                }
                let mut s = Settings::subscription_default(now);
                s.requires_session = session;
                ns.create_subscription(topic, name, s, vec![Rule::default_rule(now)])?;
                if let Some(sub) = ns.topic_mut(topic).and_then(|t| t.subs.get_mut(&key(name))) {
                    sub.core.autocreated = true;
                    next_revision();
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The entity a sender link to `addr` sends to: a queue, or a topic (a name that is a topic).
    fn send_target(&self, st: &mut State, addr: &Address, now: i64) -> Result<Address, SbError> {
        Ok(match addr {
            Address::Queue { name, sub: SubQueue::Main } => {
                self.ensure_entity(st, addr, false, true, now)?;
                if st.ns(&self.ns).topic(name).is_some() {
                    Address::Topic { name: name.clone() }
                } else {
                    addr.clone()
                }
            }
            Address::Queue { .. } | Address::Subscription { .. } => {
                return Err(SbError::not_allowed("messages cannot be sent to a subscription or a dead-letter queue"));
            }
            other => other.clone(),
        })
    }

    /// A sender link. With `dest` the link is a send-via link: `addr` is the via entity (it must exist and
    /// take sends) and `dest` the entity the messages are for.
    fn attach_entity_sender(&mut self, req: &AttachRequest, addr: Address, dest: Option<Address>) -> AttachDecision {
        let now = now_ms();
        let mut st = self.shared.state.lock();
        let target = match &dest {
            None => self.send_target(&mut st, &addr, now),
            Some(d) => self.send_target(&mut st, &addr, now).and_then(|_| self.send_target(&mut st, d, now)),
        };
        let target = match target {
            Ok(t) => t,
            Err(e) => return refuse(e),
        };
        let max = match &target {
            Address::Topic { name } => st.ns(&self.ns).topic(name).map(|t| t.settings.max_message_size_bytes()),
            _ => st.ns(&self.ns).core(&target).map(|c| c.settings.max_message_size_bytes()),
        };
        drop(st);
        self.links.insert(req.link, SbLink::EntitySender { target, session: req.session, via: dest.is_some() });
        let properties = dest.is_some().then(|| req.attach.properties.clone()).flatten();
        AttachDecision::Accept(AttachAccept { max_message_size: max, properties, ..Default::default() })
    }

    fn attach_entity_receiver(&mut self, req: &AttachRequest, addr: Address) -> AttachDecision {
        let now = now_ms();
        let source = req.attach.source.clone().unwrap_or_default();
        let session_req = session_filter_of(&source);
        let mode = if req.attach.snd_settle_mode == SndSettleMode::Settled { ReceiveMode::ReceiveAndDelete } else { ReceiveMode::PeekLock };
        let mut st = self.shared.state.lock();
        if matches!(addr, Address::Topic { .. }) {
            return refuse(SbError::not_allowed("cannot receive from a topic: receive from one of its subscriptions"));
        }
        if let Err(e) = self.ensure_entity(&mut st, &addr, session_req.is_some(), false, now) {
            return refuse(e);
        }
        let ns_name = self.ns.clone();
        let Some(core) = st.ns(&ns_name).core_mut(&addr) else { return refuse(SbError::entity_not_found(&addr_path(&addr), &ns_name)) };
        if core.settings.status == "Disabled" || core.settings.status == "ReceiveDisabled" {
            return refuse(SbError::new(
                errors::ENTITY_DISABLED,
                400,
                format!("The messaging entity '{}' is disabled for receiving.", core.path),
            ));
        }
        let owner = self.owner(req.link);
        let max = core.settings.max_message_size_bytes();
        if session_req.is_some() && !core.settings.requires_session && core.autocreated && self.shared.autocreate {
            core.settings.requires_session = true;
            next_revision();
        }
        let sub_queue = match &addr {
            Address::Queue { sub, .. } | Address::Subscription { sub, .. } => *sub,
            _ => SubQueue::Main,
        };
        match (&session_req, core.settings.requires_session && sub_queue == SubQueue::Main) {
            (None, true) => {
                return refuse(SbError::not_allowed(
                    "It is not possible for an entity that requires sessions to create a non-sessionful message receiver.",
                ));
            }
            (Some(_), false) => {
                return refuse(SbError::not_allowed(
                    "It is not possible to create a session receiver for an entity that does not require sessions.",
                ));
            }
            _ => {}
        }
        let mut accept = AttachAccept { max_message_size: Some(max), ..Default::default() };
        let mut session = None;
        if let Some(wanted) = session_req {
            match wanted {
                Some(sid) => match core.accept_session(&sid, owner, now) {
                    Ok(until) => {
                        accept = session_accept(&source, &sid, until, max);
                        session = Some(sid);
                    }
                    Err(e) => return refuse(e),
                },
                None => match core.accept_next_session(owner, now) {
                    Some((sid, until)) => {
                        accept = session_accept(&source, &sid, until, max);
                        session = Some(sid);
                    }
                    None => {
                        drop(st);
                        let wait = Arc::new(SessionWait { state: parking_lot::Mutex::new(WaitState::Waiting) });
                        self.links.insert(
                            req.link,
                            SbLink::EntityReceiver(EntityRecv {
                                addr: addr.clone(),
                                mode,
                                session: None,
                                handle: req.handle.clone(),
                                wait: Some(wait.clone()),
                                channel: req.session,
                            }),
                        );
                        self.spawn_session_wait(req, addr, source, wait);
                        return AttachDecision::Defer;
                    }
                },
            }
        }
        drop(st);
        self.links.insert(
            req.link,
            SbLink::EntityReceiver(EntityRecv { addr, mode, session, handle: req.handle.clone(), wait: None, channel: req.session }),
        );
        AttachDecision::Accept(accept)
    }

    /// A receiver asked for "the next available session" and there is none yet: wait for one in
    /// a task, then complete the attach (or time out the way Service Bus does).
    fn spawn_session_wait(&self, req: &AttachRequest, addr: Address, source: Source, wait_state: Arc<SessionWait>) {
        let shared = self.shared.clone();
        let ns = self.ns.clone();
        let handle = req.handle.clone();
        let owner = self.owner(req.link);
        let key = self.waiter_key(req.link);
        let wait = req
            .attach
            .properties
            .as_ref()
            .and_then(|p| p.map_get(OPERATION_TIMEOUT))
            .and_then(|v| v.as_u64())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_SESSION_WAIT);
        let deadline = tokio::time::Instant::now() + wait;
        tokio::spawn(async move {
            loop {
                if !handle.is_connected() {
                    return;
                }
                let now = now_ms();
                let found = {
                    let mut st = shared.state.lock();
                    let mut ws = wait_state.state.lock();
                    if matches!(*ws, WaitState::Cancelled) {
                        // the receiver is gone; the link's detach already removed its waiter
                        return;
                    }
                    match st.ns(&ns).core_mut(&addr) {
                        Some(core) => {
                            let r = core.accept_next_session(owner, now);
                            match &r {
                                Some((sid, _)) => *ws = WaitState::Granted(sid.clone()),
                                None => core.add_session_waiter(key, handle.clone()),
                            }
                            Ok(r.map(|(sid, until)| (sid, until, core.settings.max_message_size_bytes())))
                        }
                        None => Err(SbError::entity_not_found(&addr_path(&addr), &ns)),
                    }
                };
                match found {
                    Ok(Some((sid, until, max))) => {
                        handle.complete_attach(Ok(session_accept(&source, &sid, until, max)));
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        handle.complete_attach(Err(e.to_amqp()));
                        return;
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    let secs = wait.as_secs();
                    let err = SbError::new(
                        errors::TIMEOUT,
                        408,
                        format!(
                            "The operation did not complete within the allocated time 00:00:{secs:02} for object {}.",
                            addr_path(&addr)
                        ),
                    );
                    handle.complete_attach(Err(err.to_amqp()));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
    }
}

fn addr_path(a: &Address) -> String {
    match a {
        Address::Queue { name, .. } => name.clone(),
        Address::Topic { name } => name.clone(),
        Address::Subscription { topic, name, .. } => format!("{topic}/Subscriptions/{name}"),
        Address::Management(p) => format!("{p}/$management"),
        Address::Cbs => "$cbs".into(),
    }
}

fn session_accept(source: &Source, sid: &str, until_ms: i64, max: u64) -> AttachAccept {
    let mut src = source.clone();
    // The answer repeats the form of the request. go-amqp types a filter value as a described type
    // (AMQP 1.0 part 3.5.1, filter-set) and drops the connection on a bare string; rhea (JavaScript)
    // sends a bare string and compares what comes back with `===`, so a described answer (which it
    // decodes to a wrapper object) fails there. Python reads neither. A described request keeps its
    // descriptor (Service Bus's own is 0x00000013_7000000C).
    let value = Value::String(sid.to_string());
    let value = match source.filter_get(SESSION_FILTER).and_then(|v| v.as_described()) {
        Some(d) => Value::described(d.descriptor.clone(), value),
        None => value,
    };
    src.filter = Some(Value::Map(vec![(Value::sym(SESSION_FILTER), value)]));
    AttachAccept {
        source: Some(src),
        max_message_size: Some(max),
        properties: Some(Value::Map(vec![(Value::sym(LOCKED_UNTIL_UTC), Value::Long(ms_to_ticks(until_ms)))])),
        ..Default::default()
    }
}

impl ConnectionHandler for SbConn {
    fn on_open(&mut self, open: &azemu_amqp::performatives::Open) -> Result<(), AmqpError> {
        self.ns = namespace_from_hostname(open.hostname.as_deref());
        Ok(())
    }

    fn on_attach(&mut self, req: AttachRequest) -> AttachDecision {
        let role = req.local_role();
        // The coordinator target (transactions).
        if let Some(TargetKind::Coordinator(_)) = &req.attach.target {
            if role != Role::Receiver {
                return AttachDecision::Refuse(AmqpError::new(
                    condition::NOT_ALLOWED,
                    "The coordinator takes messages from the client: attach as the sender.",
                ));
            }
            // the link is outside any entity: no `$cbs` token is needed and one link serves every entity
            self.links.insert(req.link, SbLink::Coordinator);
            return AttachDecision::Accept(AttachAccept::default());
        }
        let raw = match role {
            Role::Receiver => req.attach.target.as_ref().and_then(|t| t.address()).unwrap_or_default().to_string(),
            Role::Sender => req.attach.source.as_ref().and_then(|s| s.address.clone()).unwrap_or_default(),
        };
        let Some(addr) = Address::parse(&raw) else {
            return refuse(SbError::new(
                condition::INVALID_FIELD,
                400,
                format!("The address '{raw}' is not a valid Service Bus entity address."),
            ));
        };
        match (&addr, role) {
            (Address::Cbs, Role::Receiver) => {
                self.links.insert(req.link, SbLink::RequestReceiver { node: Node::Cbs, session: req.session });
                self.pairing.entry(Node::Cbs.key()).or_default().add_request(req.session, req.link);
                AttachDecision::Accept(AttachAccept { max_message_size: Some(CBS_MAX_MESSAGE_SIZE), ..Default::default() })
            }
            (Address::Cbs, Role::Sender) => {
                self.register_responder(&req, Node::Cbs);
                AttachDecision::Accept(AttachAccept::default())
            }
            (Address::Management(path), role) => {
                let node = Node::Management(path.clone());
                let target = Address::parse(path);
                let now = now_ms();
                {
                    let mut st = self.shared.state.lock();
                    match &target {
                        Some(a @ (Address::Queue { .. } | Address::Subscription { .. })) => {
                            if let Err(e) = self.ensure_entity(&mut st, a, false, true, now) {
                                return refuse(e);
                            }
                        }
                        _ => return refuse(SbError::entity_not_found(path, &self.ns)),
                    }
                }
                if role == Role::Receiver {
                    self.pairing.entry(node.key()).or_default().add_request(req.session, req.link);
                    self.links.insert(req.link, SbLink::RequestReceiver { node, session: req.session });
                } else {
                    self.register_responder(&req, node);
                }
                AttachDecision::Accept(AttachAccept {
                    max_message_size: (role == Role::Receiver).then_some(MGMT_MAX_MESSAGE_SIZE),
                    ..Default::default()
                })
            }
            (_, Role::Receiver) => {
                // send-via: the link is attached to the via entity and names the real destination
                // (`com.microsoft:transfer-destination-address`); what is sent goes to the destination
                let dest = req
                    .attach
                    .properties
                    .as_ref()
                    .and_then(|p| p.map_get(TRANSFER_DESTINATION))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                match dest {
                    None => self.attach_entity_sender(&req, addr, None),
                    Some(d) => {
                        let Some(dest_addr) = Address::parse(&d) else {
                            return refuse(SbError::new(
                                condition::INVALID_FIELD,
                                400,
                                format!("The address '{d}' is not a valid Service Bus entity address."),
                            ));
                        };
                        self.attach_entity_sender(&req, addr, Some(dest_addr))
                    }
                }
            }
            (_, Role::Sender) => self.attach_entity_receiver(&req, addr),
        }
    }

    fn on_transfer(&mut self, link: LinkId, d: IncomingDelivery) -> TransferAction {
        enum Kind {
            Request(Node, u16),
            Send(Address, crate::txn::OpContext),
            Coordinator,
            None,
        }
        let kind = match self.links.get(&link) {
            Some(SbLink::RequestReceiver { node, session }) => Kind::Request(node.clone(), *session),
            Some(SbLink::EntitySender { target, session, via }) => {
                Kind::Send(target.clone(), crate::txn::OpContext { session: *session, via: *via })
            }
            Some(SbLink::Coordinator) => Kind::Coordinator,
            _ => Kind::None,
        };
        // the transaction a transfer belongs to (`transactional-state` as its delivery state)
        let txn = crate::txn::txn_id_of(&d.state);
        match kind {
            Kind::Request(node, session) => {
                self.txns.set_context(crate::txn::OpContext { session, via: false });
                match Message::decode(&d.payload) {
                    Ok(m) => self.handle_request(node, link, session, m, txn.clone()),
                    Err(e) => {
                        return TransferAction::Settle(DeliveryState::rejected(AmqpError::new(condition::DECODE_ERROR, e.to_string())))
                    }
                }
                TransferAction::Settle(crate::txn::in_txn(&txn, DeliveryState::Accepted))
            }
            Kind::Send(target, ctx) => {
                self.txns.set_context(ctx);
                let sent = self.handle_send(&target, &d, txn.as_deref());
                match sent {
                    Ok(()) => TransferAction::Settle(crate::txn::in_txn(&txn, DeliveryState::Accepted)),
                    Err(e) => TransferAction::Settle(crate::txn::in_txn(&txn, DeliveryState::rejected(e.to_amqp()))),
                }
            }
            Kind::Coordinator => self.on_coordinator_transfer(link, &d),
            Kind::None => TransferAction::Settle(DeliveryState::rejected(AmqpError::new(condition::ILLEGAL_STATE, "unexpected transfer"))),
        }
    }

    fn on_disposition(&mut self, link: LinkId, tag: &[u8], state: Option<DeliveryState>, _settled: bool) -> Option<DeliveryState> {
        // The link is gone (the broker detached it, for example when its session lock lapsed):
        // the settlement cannot be applied.
        let Some(SbLink::EntityReceiver(r)) = self.links.get(&link) else {
            return Some(DeliveryState::rejected(SbError::message_lock_lost().to_amqp()));
        };
        let addr = r.addr.clone();
        let channel = r.channel;
        // a receiver bound to a session (by id, or the one it was given): its settlements that fail because the
        // lock is gone fail with the session's error, which is what the SDKs turn into `SessionLockLost`
        let session_receiver =
            r.session.is_some() || r.wait.as_ref().map(|w| matches!(&*w.state.lock(), WaitState::Granted(_))).unwrap_or(false);
        let token = tag_to_token(tag)?;
        // inside a transaction the outcome is wrapped in a `transactional-state`
        let (txn, state) = match state {
            Some(DeliveryState::Transactional { txn_id, outcome }) => match outcome {
                Some(o) => (Some(txn_id), Some(*o)),
                None => {
                    let err = AmqpError::new(condition::INVALID_FIELD, "A transactional disposition needs an outcome.");
                    return Some(crate::txn::in_txn(&Some(txn_id), DeliveryState::rejected(err)));
                }
            },
            other => (None, other),
        };
        let (outcome, reply_ok) = match &state {
            Some(DeliveryState::Accepted) => (Outcome::Complete, None),
            Some(DeliveryState::Released) | None => (Outcome::Release, None),
            Some(DeliveryState::Modified { undeliverable_here, message_annotations, .. }) => {
                // `modified` is how the SDKs abandon (`delivery-failed` is true in Python, absent in
                // JavaScript and Go) and defer (`undeliverable-here`). Abandon always counts as a
                // delivery attempt, so `delivery-failed` does not decide anything.
                let modify = message_annotations.as_ref().and_then(|v| v.as_map().map(|m| m.to_vec()));
                if *undeliverable_here {
                    (Outcome::Defer { modify }, None)
                } else {
                    (Outcome::Abandon { modify }, None)
                }
            }
            Some(DeliveryState::Rejected { error }) => {
                let info = error.as_ref().and_then(|e| e.info.clone()).unwrap_or_default();
                let find = |k: &str| info.iter().find(|(key, _)| key.as_str() == Some(k)).and_then(|(_, v)| v.as_str().map(str::to_string));
                let reason = find(PROP_DEAD_LETTER_REASON).or_else(|| error.as_ref().map(|e| e.condition.clone()));
                let description = find(PROP_DEAD_LETTER_DESCRIPTION).or_else(|| error.as_ref().and_then(|e| e.description.clone()));
                // every other entry is a property to modify (`propertiesToModify` of the SDKs)
                let modify: Vec<(Value, Value)> = info
                    .iter()
                    .filter(|(key, _)| !matches!(key.as_str(), Some(PROP_DEAD_LETTER_REASON) | Some(PROP_DEAD_LETTER_DESCRIPTION)))
                    .cloned()
                    .collect();
                let modify = (!modify.is_empty()).then_some(modify);
                (Outcome::DeadLetter { reason, description, modify }, Some(DeliveryState::Rejected { error: None }))
            }
            Some(DeliveryState::Received { .. }) | Some(_) => return None,
        };
        if let Some(txn_id) = txn {
            self.txns.set_context(crate::txn::OpContext { session: channel, via: false });
            // applied when the transaction commits; the answer says what will be applied
            let answer = reply_ok.or(state).unwrap_or(DeliveryState::Released);
            let txn = Some(txn_id);
            return Some(match self.txn_settle(txn.as_deref().unwrap_or_default(), &addr, token, outcome) {
                Ok(()) => crate::txn::in_txn(&txn, answer),
                Err(e) => crate::txn::in_txn(&txn, DeliveryState::rejected(if session_receiver { e.in_session() } else { e }.to_amqp())),
            });
        }
        let now = now_ms();
        let mut st = self.shared.state.lock();
        let ns = self.ns.clone();
        let result = match st.ns(&ns).core_mut(&addr) {
            Some(core) => core.settle(&token, outcome, now),
            None => Err(SbError::entity_not_found(&addr_path(&addr), &ns)),
        };
        match result {
            Ok(fwd) => {
                st.forward_all(&ns, fwd, now);
                reply_ok
            }
            Err(e) => Some(DeliveryState::rejected(if session_receiver { e.in_session() } else { e }.to_amqp())),
        }
    }

    fn pull(&mut self, link: LinkId) -> Option<Outgoing> {
        match self.links.get_mut(&link)? {
            SbLink::ResponseSender { outbox, .. } => outbox.pop_front(),
            SbLink::EntityReceiver(_) => self.pull_entity(link),
            _ => None,
        }
    }

    fn on_detach(&mut self, link: LinkId, _reason: DetachReason) {
        let Some(l) = self.links.remove(&link) else { return };
        if matches!(l, SbLink::Coordinator) {
            // the controller is gone: its open transactions are rolled back
            self.txns.drop_controller(link);
        }
        if matches!(l, SbLink::RequestReceiver { .. } | SbLink::ResponseSender { .. }) {
            for p in self.pairing.values_mut() {
                p.remove(link);
            }
        }
        match l {
            SbLink::ResponseSender { node_key, .. } => {
                if let Some(v) = self.responders.get_mut(&node_key) {
                    v.retain(|id| *id != link);
                }
            }
            SbLink::EntityReceiver(r) => {
                let ns = self.ns.clone();
                let owner = self.owner(link);
                let key = self.waiter_key(link);
                let now = now_ms();
                let mut st = self.shared.state.lock();
                // A receiver that waited for a session: stop the wait (under the state lock, so
                // the task cannot lock a session afterwards) and find out whether it got one.
                let mut granted = None;
                if let Some(w) = &r.wait {
                    let mut ws = w.state.lock();
                    if let WaitState::Granted(sid) = &*ws {
                        granted = Some(sid.clone());
                    }
                    *ws = WaitState::Cancelled;
                }
                let mut fwd = Vec::new();
                if let Some(core) = st.ns(&ns).core_mut(&r.addr) {
                    core.remove_waiter(key);
                    if let Some(sid) = r.session.as_ref().or(granted.as_ref()) {
                        fwd = core.release_session(sid, owner);
                    }
                }
                st.forward_all(&ns, fwd, now);
            }
            _ => {}
        }
    }
}

impl SbConn {
    fn register_responder(&mut self, req: &AttachRequest, node: Node) {
        let key = node.key();
        let reply_address = req.attach.target.as_ref().and_then(|t| t.address()).map(str::to_string);
        self.links.insert(
            req.link,
            SbLink::ResponseSender { node_key: key.clone(), session: req.session, reply_address, outbox: VecDeque::new() },
        );
        self.pairing.entry(key.clone()).or_default().add_response(req.session, req.link);
        self.responders.entry(key).or_default().push(req.link);
    }

    /// Queue a response on the response link that answers this request and wake it: the link
    /// whose address is the request's `reply-to`, else the one on the request's session (see
    /// `azemu_amqp::reply`).
    pub(crate) fn respond(&mut self, node: &Node, request_link: LinkId, request_session: u16, reply_to: Option<&str>, response: Message) {
        let key = node.key();
        let candidates: Vec<azemu_amqp::reply::ResponseLink> = self
            .responders
            .get(&key)
            .into_iter()
            .flatten()
            .filter_map(|id| match self.links.get(id) {
                Some(SbLink::ResponseSender { session, reply_address, .. }) => {
                    Some(azemu_amqp::reply::ResponseLink { link: *id, session: *session, address: reply_address.clone() })
                }
                _ => None,
            })
            .collect();
        let paired = self.pairing.get(&key).and_then(|p| p.response_for(request_link));
        let ids = azemu_amqp::reply::pick(request_session, reply_to, paired, &candidates);
        let payload = response.encode();
        for id in ids {
            if let Some(SbLink::ResponseSender { outbox, .. }) = self.links.get_mut(&id) {
                outbox.push_back(Outgoing {
                    tag: uuid::Uuid::new_v4().as_bytes().to_vec(),
                    message_format: 0,
                    settled: true,
                    payload: payload.clone(),
                });
            }
            self._conn.link(id).wake();
        }
    }

    fn handle_send(&mut self, target: &Address, d: &IncomingDelivery, txn: Option<&[u8]>) -> Result<(), SbError> {
        let now = now_ms();
        let messages: Vec<(Message, usize)> = if d.message_format == BATCH_MESSAGE_FORMAT {
            decode_batch_limited(&d.payload)?
                .into_iter()
                .map(|m| {
                    let size = m.encode().len();
                    (m, size)
                })
                .collect()
        } else {
            vec![(Message::decode(&d.payload).map_err(|e| SbError::new(condition::DECODE_ERROR, 400, e.to_string()))?, d.payload.len())]
        };
        if let Some(txn) = txn {
            // not stored yet: checked and kept until the transaction commits
            return self.txn_send(txn, target, messages);
        }
        let ns = self.ns.clone();
        let mut st = self.shared.state.lock();
        // validate every message first so a batch is accepted or rejected as a whole
        {
            let nsr = st.ns(&ns);
            match target {
                Address::Topic { name } => {
                    let t = nsr.topic(name).ok_or_else(|| SbError::entity_not_found(name, &ns))?;
                    for (_, size) in &messages {
                        t.check_send(*size)?;
                    }
                }
                other => {
                    let core = nsr.core(other).ok_or_else(|| SbError::entity_not_found(&addr_path(other), &ns))?;
                    for (_, size) in &messages {
                        core.check_send_limits(*size)?;
                    }
                    // the whole batch must fit, so that it is stored whole or not at all
                    core.check_room_for(messages.iter().map(|(_, size)| charged(*size)).sum())?;
                }
            }
        }
        for (m, size) in messages {
            st.send(&ns, target, Incoming::new(m, size), now)?;
        }
        Ok(())
    }

    fn pull_entity(&mut self, link: LinkId) -> Option<Outgoing> {
        let Some(SbLink::EntityReceiver(r)) = self.links.get_mut(&link) else { return None };
        if r.session.is_none() {
            if let Some(w) = &r.wait {
                if let WaitState::Granted(sid) = &*w.state.lock() {
                    r.session = Some(sid.clone());
                }
            }
        }
        let (addr, mode, session, handle) = (r.addr.clone(), r.mode, r.session.clone(), r.handle.clone());
        let sq = match &addr {
            Address::Queue { sub, .. } | Address::Subscription { sub, .. } => *sub,
            _ => SubQueue::Main,
        };
        let owner = self.owner(link);
        let key = self.waiter_key(link);
        let ns = self.ns.clone();
        let now = now_ms();
        let mut st = self.shared.state.lock();
        let res = match st.ns(&ns).core_mut(&addr) {
            Some(core) => core.pull(sq, session.as_deref(), owner, mode, now, Some((key, handle.clone()))),
            None => {
                drop(st);
                handle.detach(Some(SbError::entity_not_found(&addr_path(&addr), &ns).to_amqp()));
                return None;
            }
        };
        match res {
            Ok((delivered, fwd)) => {
                if !fwd.is_empty() {
                    st.forward_all(&ns, fwd, now);
                }
                let d = delivered?;
                drop(st);
                let msg = render(&d.msg, None);
                let (tag, settled) = match d.token {
                    Some(t) => (token_to_tag(&t), false),
                    None => (d.msg.seq.to_be_bytes().to_vec(), true),
                };
                Some(Outgoing { tag, message_format: 0, settled, payload: msg.encode() })
            }
            Err(e) => {
                drop(st);
                handle.detach(Some(e.to_amqp()));
                None
            }
        }
    }
}

/// [`Message::decode_batch`] with a limit on the number of messages: the envelope is read first (its `data` sections
/// are bytes, a few per message) and only a batch within [`MAX_MESSAGES_PER_BATCH`] is decoded into messages. A batch
/// of a million empty messages fits in the 1 MiB batch limit and would otherwise cost hundreds of MB while it is decoded.
fn decode_batch_limited(payload: &[u8]) -> Result<Vec<Message>, SbError> {
    let bad = |e: azemu_amqp::codec::DecodeError| SbError::new(condition::DECODE_ERROR, 400, e.to_string());
    let envelope = Message::decode(payload).map_err(bad)?;
    match envelope.body {
        Body::Data(parts) => {
            if parts.len() > MAX_MESSAGES_PER_BATCH {
                return Err(SbError::new(
                    condition::LINK_MESSAGE_SIZE_EXCEEDED,
                    400,
                    format!(
                        "The batch holds {} messages; at most {MAX_MESSAGES_PER_BATCH} messages can be sent in one batch.",
                        parts.len()
                    ),
                ));
            }
            parts.iter().map(|p| Message::decode(p).map_err(bad)).collect()
        }
        Body::Empty => Ok(Vec::new()),
        _ => Err(SbError::new(condition::DECODE_ERROR, 400, "batch envelope body is not data sections")),
    }
}

/// Body helpers shared with the management side.
pub(crate) fn map_body(m: &Message) -> Option<&[(Value, Value)]> {
    match &m.body {
        Body::Value(Value::Map(v)) => Some(v),
        _ => None,
    }
}
