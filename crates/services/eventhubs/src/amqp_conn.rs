//! The AMQP side of Event Hubs: one `EhConn` per connection implements the connection handler
//! of `azemu-amqp` on top of the shared state.
//!
//! Links: `$cbs` and `$management` request/response pairs, sender links to an event hub or one
//! of its partitions (the client sends events), receiver links from a partition of a consumer
//! group (the client reads events).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use azemu_amqp::api::*;
use azemu_amqp::message::{Message, BATCH_MESSAGE_FORMAT};
use azemu_amqp::performatives::{Open, Role};
use azemu_amqp::terminus::{DeliveryState, TargetKind};
use azemu_amqp::types::Value;
use azemu_amqp::{condition, Error as AmqpError};

use crate::address::Address;
use crate::errors::EhError;
use crate::model::*;
use crate::selector::{self, StartPosition};
use crate::state::*;

/// Largest request on `$cbs` or `$management`.
pub const MAX_REQUEST_SIZE: u64 = 1024 * 1024;
pub const SELECTOR_FILTER: &str = "apache.org:selector-filter:string";
pub const EPOCH_PROPERTY: &str = "com.microsoft:epoch";
pub const RUNTIME_METRIC_CAPABILITY: &str = "com.microsoft:enable-receiver-runtime-metric";

pub struct EhFactory {
    pub shared: Arc<Shared>,
}

impl HandlerFactory for EhFactory {
    fn new_connection(&self, _info: &ConnectionInfo, conn: ConnHandle) -> Box<dyn ConnectionHandler> {
        Box::new(EhConn {
            shared: self.shared.clone(),
            conn_id: self.shared.next_conn_id(),
            conn,
            ns: DEFAULT_NAMESPACE.to_string(),
            links: HashMap::new(),
            pairing: HashMap::new(),
        })
    }
}

/// The namespace an `open` frame's hostname selects: the first DNS label, except for
/// `localhost`, IP addresses, an empty hostname and the bare `eventhub(s)` / `servicebus` labels.
pub fn namespace_from_hostname(host: Option<&str>) -> String {
    let Some(h) = host.map(str::trim).filter(|h| !h.is_empty()) else { return DEFAULT_NAMESPACE.to_string() };
    if h.starts_with('[') {
        return DEFAULT_NAMESPACE.to_string();
    }
    let h = h.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map(|(a, _)| a).unwrap_or(h);
    if h.parse::<std::net::IpAddr>().is_ok() {
        return DEFAULT_NAMESPACE.to_string();
    }
    let first = h.split('.').next().unwrap_or("").to_lowercase();
    if matches!(first.as_str(), "" | "localhost" | "servicebus" | "eventhub" | "eventhubs") {
        DEFAULT_NAMESPACE.to_string()
    } else {
        first
    }
}

/// Does the hostname of the `open` frame say "Event Hubs"? Its first label is `eventhub(s)` or
/// it contains an `eventhub(s)` label after the first (`ns.eventhub.localhost`).
pub fn hostname_says_eventhubs(host: Option<&str>) -> bool {
    let Some(h) = host.map(|h| h.trim().to_lowercase()).filter(|h| !h.is_empty()) else { return false };
    let h = h.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map(|(a, _)| a).unwrap_or(&h).to_string();
    h.split('.').any(|l| l == "eventhub" || l == "eventhubs")
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Node {
    Cbs,
    Management,
}

#[derive(Clone)]
pub(crate) struct SendLink {
    hub: String,
    target: SendTarget,
}

pub(crate) struct RecvLink {
    hub: String,
    hub_id: u64,
    pid: usize,
    /// Sequence number of the next event to send.
    cursor: i64,
    metrics: bool,
    handle: LinkHandle,
}

pub(crate) enum EhLink {
    /// We receive requests from the client on `$cbs` or `$management`.
    RequestReceiver { node: Node, session: u16 },
    /// We send responses on the response link of a request/response pair. `session` and
    /// `reply_address` (the target of the client's receiving end) say which requests it answers.
    ResponseSender { node: Node, session: u16, reply_address: Option<String>, outbox: VecDeque<Outgoing> },
    /// The client sends events.
    Sender(SendLink),
    /// The client reads events.
    Receiver(RecvLink),
}

pub struct EhConn {
    pub(crate) shared: Arc<Shared>,
    pub(crate) conn_id: u64,
    pub(crate) conn: ConnHandle,
    pub(crate) ns: String,
    pub(crate) links: HashMap<LinkId, EhLink>,
    /// request and response links of each node, paired by attach order within a session
    pairing: HashMap<Node, azemu_amqp::reply::Pairing>,
}

fn refuse(e: EhError) -> AttachDecision {
    AttachDecision::Refuse(e.to_amqp())
}

fn rejected(e: EhError) -> TransferAction {
    TransferAction::Settle(DeliveryState::rejected(e.to_amqp()))
}

impl EhConn {
    fn attach_sender(&mut self, req: &AttachRequest, hub: String, partition: Option<String>) -> AttachDecision {
        let now = now_ms();
        let mut st = self.shared.state.lock();
        if let Err(e) = st.ensure_hub(&self.ns, &hub, self.shared.autocreate, now) {
            return refuse(e);
        }
        let target = match partition {
            None => SendTarget::Any,
            Some(pid) => match st.hub(&self.ns, &hub).map(|h| h.parse_pid(&pid)) {
                Some(Ok(p)) => SendTarget::Partition(p),
                Some(Err(e)) => return refuse(e),
                None => return refuse(EhError::entity_not_found(&self.ns, &hub)),
            },
        };
        drop(st);
        self.links.insert(req.link, EhLink::Sender(SendLink { hub, target }));
        AttachDecision::Accept(AttachAccept { max_message_size: Some(MAX_MESSAGE_SIZE), ..Default::default() })
    }

    fn attach_receiver(&mut self, req: &AttachRequest, hub: String, cg: String, pid: String) -> AttachDecision {
        let now = now_ms();
        let source = req.attach.source.clone().unwrap_or_default();
        let position = match source.filter_get(SELECTOR_FILTER) {
            None => StartPosition::Beginning,
            Some(v) => {
                let text = match v.undescribed() {
                    Value::Binary(b) => String::from_utf8_lossy(b).into_owned(),
                    other => other.as_str().unwrap_or_default().to_string(),
                };
                match selector::parse(&text) {
                    Ok(p) => p,
                    Err(e) => return refuse(e),
                }
            }
        };
        let epoch = req.attach.properties.as_ref().and_then(|p| p.map_get(EPOCH_PROPERTY)).and_then(|v| v.as_i64());
        let metrics = req.attach.desired_capabilities.iter().any(|c| c == RUNTIME_METRIC_CAPABILITY);
        let mut st = self.shared.state.lock();
        if let Err(e) = st.ensure_hub(&self.ns, &hub, self.shared.autocreate, now) {
            return refuse(e);
        }
        let nsk = st.resolve(&self.ns);
        let autocreate = self.shared.autocreate;
        let Some(h) = st.hub_mut(&self.ns, &hub) else { return refuse(EhError::entity_not_found(&nsk, &hub)) };
        let p = match h.parse_pid(&pid) {
            Ok(p) => p,
            Err(e) => return refuse(e),
        };
        if !h.has_consumer_group(&cg) {
            if !autocreate {
                return refuse(EhError::entity_not_found(&nsk, &format!("{}/ConsumerGroups/{cg}", h.name)));
            }
            if h.consumer_groups.len() >= MAX_CONSUMER_GROUPS {
                return refuse(EhError::new(
                    condition::RESOURCE_LIMIT_EXCEEDED,
                    403,
                    format!("The maximum number of consumer groups of an event hub ({MAX_CONSUMER_GROUPS}) has been reached."),
                ));
            }
            h.add_consumer_group(&cg);
        }
        h.partitions[p].trim(now, h.retention_ms);
        let cursor = match h.partitions[p].cursor_for(position) {
            Ok(c) => c,
            Err(e) => return refuse(e),
        };
        let hub_id = h.id;
        let reg = RecvReg { conn: self.conn_id, link: req.link, pid: p, cg: key(&cg), epoch, handle: req.handle.clone() };
        if let Err(e) = st.register_receiver(&self.ns, &hub, reg) {
            return refuse(e);
        }
        drop(st);
        self.links.insert(req.link, EhLink::Receiver(RecvLink { hub, hub_id, pid: p, cursor, metrics, handle: req.handle.clone() }));
        AttachDecision::Accept(AttachAccept {
            max_message_size: Some(MAX_MESSAGE_SIZE),
            offered_capabilities: if metrics { vec![RUNTIME_METRIC_CAPABILITY.to_string()] } else { vec![] },
            ..Default::default()
        })
    }

    fn register_request(&mut self, req: &AttachRequest, node: Node) {
        self.links.insert(req.link, EhLink::RequestReceiver { node, session: req.session });
        self.pairing.entry(node).or_default().add_request(req.session, req.link);
    }

    fn register_responder(&mut self, req: &AttachRequest, node: Node) {
        let reply_address = req.attach.target.as_ref().and_then(|t| t.address()).map(str::to_string);
        self.links.insert(req.link, EhLink::ResponseSender { node, session: req.session, reply_address, outbox: VecDeque::new() });
        self.pairing.entry(node).or_default().add_response(req.session, req.link);
    }

    /// Queue a response on the response link that answers this request and wake it: the link
    /// whose address is the request's `reply-to`, else the one on the request's session (the
    /// pair the SDKs open on one session), else every response link of the node (see
    /// `azemu_amqp::reply`).
    pub(crate) fn respond(&mut self, request_link: LinkId, request_session: u16, node: Node, reply_to: Option<&str>, response: Message) {
        let candidates: Vec<azemu_amqp::reply::ResponseLink> = self
            .links
            .iter()
            .filter_map(|(id, l)| match l {
                EhLink::ResponseSender { node: n, session, reply_address, .. } if *n == node => {
                    Some(azemu_amqp::reply::ResponseLink { link: *id, session: *session, address: reply_address.clone() })
                }
                _ => None,
            })
            .collect();
        let paired = self.pairing.get(&node).and_then(|p| p.response_for(request_link));
        let mut ids = azemu_amqp::reply::pick(request_session, reply_to, paired, &candidates);
        ids.sort_unstable();
        let payload = response.encode();
        for id in ids {
            if let Some(EhLink::ResponseSender { outbox, .. }) = self.links.get_mut(&id) {
                outbox.push_back(Outgoing {
                    tag: uuid::Uuid::new_v4().as_bytes().to_vec(),
                    message_format: 0,
                    settled: true,
                    payload: payload.clone(),
                });
            }
            self.conn.link(id).wake();
        }
    }

    fn handle_send(&mut self, link: &SendLink, d: &IncomingDelivery) -> Result<(), EhError> {
        let batch = d.message_format == BATCH_MESSAGE_FORMAT;
        let msgs: Vec<Message> = if batch {
            decode_batch_limited(&d.payload)?
        } else {
            vec![Message::decode(&d.payload).map_err(|e| EhError::new(condition::DECODE_ERROR, 400, e.to_string()))?]
        };
        if msgs.is_empty() {
            return Ok(());
        }
        // measuring and stripping the events costs time in proportion to their size: before the lock
        let prepared = msgs.into_iter().map(prepare).collect();
        self.shared.state.lock().store_prepared(&self.ns, &link.hub, link.target, prepared, batch, now_ms())
    }

    fn pull_receiver(&mut self, link: LinkId) -> Option<Outgoing> {
        let Some(EhLink::Receiver(r)) = self.links.get_mut(&link) else { return None };
        let now = now_ms();
        let mut st = self.shared.state.lock();
        let ns = st.resolve(&self.ns);
        let Some(h) = st.hub_mut(&ns, &r.hub).filter(|h| h.id == r.hub_id) else {
            drop(st);
            r.handle.detach(Some(EhError::entity_not_found(&ns, &r.hub).to_amqp()));
            return None;
        };
        let retention = h.retention_ms;
        let p = &mut h.partitions[r.pid];
        p.trim(now, retention);
        r.cursor = r.cursor.max(p.first_seq());
        let ev = p.get(r.cursor)?;
        r.cursor += 1;
        let metrics = r.metrics.then(|| (p.last_seq(), p.last_offset_string(), p.last_ms.unwrap_or(now)));
        drop(st);
        Some(Outgoing {
            tag: ev.seq.to_be_bytes().to_vec(),
            message_format: 0,
            settled: true,
            payload: render(&ev, metrics.map(|m| (m, now))),
        })
    }
}

/// [`Message::decode_batch`] with a limit on the number of events: the envelope is read first (its `data` sections are
/// bytes, a few per event) and only a batch within [`MAX_EVENTS_PER_BATCH`] is decoded into messages. A batch of a
/// million empty events fits in the 1 MiB message limit and would otherwise cost hundreds of MB while it is decoded.
fn decode_batch_limited(payload: &[u8]) -> Result<Vec<Message>, EhError> {
    let bad = |e: azemu_amqp::codec::DecodeError| EhError::new(condition::DECODE_ERROR, 400, e.to_string());
    let envelope = Message::decode(payload).map_err(bad)?;
    match envelope.body {
        azemu_amqp::message::Body::Data(parts) => {
            if parts.len() > MAX_EVENTS_PER_BATCH {
                return Err(EhError::new(
                    condition::LINK_MESSAGE_SIZE_EXCEEDED,
                    400,
                    format!("The batch holds {} events; at most {MAX_EVENTS_PER_BATCH} events can be sent in one batch.", parts.len()),
                ));
            }
            parts.iter().map(|p| Message::decode(p).map_err(bad)).collect()
        }
        azemu_amqp::message::Body::Empty => Ok(Vec::new()),
        _ => Err(EhError::new(condition::DECODE_ERROR, 400, "batch envelope body is not data sections")),
    }
}

type Metrics = (i64, String, i64);

/// The message as a receiver sees it: the stored event plus the annotations only the broker sets.
fn render(ev: &Event, metrics: Option<(Metrics, i64)>) -> Vec<u8> {
    let mut m = ev.message.clone();
    let mut ann = vec![
        (Value::sym("x-opt-sequence-number"), Value::Long(ev.seq)),
        (Value::sym("x-opt-offset"), Value::String(ev.offset.to_string())),
        (Value::sym("x-opt-enqueued-time"), Value::Timestamp(ev.enqueued_ms)),
    ];
    ann.extend(m.message_annotations.take().unwrap_or_default());
    m.message_annotations = Some(ann);
    if let Some(((seq, offset, time), retrieved)) = metrics {
        // what the sender put in the delivery annotations is kept (Microsoft's live tests read it back)
        let mut da = m.delivery_annotations.take().unwrap_or_default();
        da.extend([
            (Value::sym("last_enqueued_sequence_number"), Value::Long(seq)),
            (Value::sym("last_enqueued_offset"), Value::String(offset)),
            (Value::sym("last_enqueued_time_utc"), Value::Timestamp(time)),
            (Value::sym("runtime_info_retrieval_time_utc"), Value::Timestamp(retrieved)),
        ]);
        m.delivery_annotations = Some(da);
    }
    m.encode()
}

impl ConnectionHandler for EhConn {
    fn on_open(&mut self, open: &Open) -> Result<(), AmqpError> {
        self.ns = namespace_from_hostname(open.hostname.as_deref());
        Ok(())
    }

    fn on_attach(&mut self, req: AttachRequest) -> AttachDecision {
        self.decide(&req)
    }

    fn on_transfer(&mut self, link: LinkId, d: IncomingDelivery) -> TransferAction {
        enum Kind {
            Request(Node, u16),
            Send(SendLink),
            None,
        }
        let kind = match self.links.get(&link) {
            Some(EhLink::RequestReceiver { node, session }) => Kind::Request(*node, *session),
            Some(EhLink::Sender(s)) => Kind::Send(s.clone()),
            _ => Kind::None,
        };
        match kind {
            Kind::Request(node, session) => match Message::decode(&d.payload) {
                Ok(m) => {
                    self.handle_request(link, session, node, m);
                    TransferAction::Settle(DeliveryState::Accepted)
                }
                Err(e) => TransferAction::Settle(DeliveryState::rejected(AmqpError::new(condition::DECODE_ERROR, e.to_string()))),
            },
            Kind::Send(s) => match self.handle_send(&s, &d) {
                Ok(()) => TransferAction::Settle(DeliveryState::Accepted),
                Err(e) => rejected(e),
            },
            Kind::None => TransferAction::Settle(DeliveryState::rejected(AmqpError::new(condition::ILLEGAL_STATE, "unexpected transfer"))),
        }
    }

    fn pull(&mut self, link: LinkId) -> Option<Outgoing> {
        match self.links.get_mut(&link)? {
            EhLink::ResponseSender { outbox, .. } => outbox.pop_front(),
            EhLink::Receiver(_) => self.pull_receiver(link),
            _ => None,
        }
    }

    fn on_detach(&mut self, link: LinkId, _reason: DetachReason) {
        for p in self.pairing.values_mut() {
            p.remove(link);
        }
        if let Some(EhLink::Receiver(r)) = self.links.remove(&link) {
            self.shared.state.lock().unregister_receiver(&self.ns, &r.hub, self.conn_id, link);
        }
    }
}

impl EhConn {
    fn decide(&mut self, req: &AttachRequest) -> AttachDecision {
        if let Some(TargetKind::Coordinator(_)) = &req.attach.target {
            return AttachDecision::Refuse(AmqpError::new(
                condition::NOT_IMPLEMENTED,
                "eventhubs: transactions are not implemented by azemu",
            ));
        }
        let role = req.local_role();
        let raw = match role {
            Role::Receiver => req.attach.target.as_ref().and_then(|t| t.address()).unwrap_or_default().to_string(),
            Role::Sender => req.attach.source.as_ref().and_then(|s| s.address.clone()).unwrap_or_default(),
        };
        let Some(addr) = Address::parse(&raw) else {
            return refuse(EhError::new(condition::INVALID_FIELD, 400, format!("The address '{raw}' is not a valid Event Hubs address.")));
        };
        match (addr, role) {
            (Address::Cbs, Role::Receiver) => self.accept_request(req, Node::Cbs),
            (Address::Cbs, Role::Sender) => self.accept_responder(req, Node::Cbs),
            (Address::Management | Address::HubManagement(_), Role::Receiver) => self.accept_request(req, Node::Management),
            (Address::Management | Address::HubManagement(_), Role::Sender) => self.accept_responder(req, Node::Management),
            (Address::Hub(hub), Role::Receiver) => self.attach_sender(req, hub, None),
            (Address::Partition { hub, pid }, Role::Receiver) => self.attach_sender(req, hub, Some(pid)),
            (Address::Consumer { hub, cg, pid }, Role::Sender) => self.attach_receiver(req, hub, cg, pid),
            (Address::Hub(_) | Address::Partition { .. }, Role::Sender) => {
                refuse(EhError::not_allowed("Receiving needs a consumer group: use '{hub}/ConsumerGroups/{group}/Partitions/{id}'."))
            }
            (Address::Consumer { .. }, Role::Receiver) => {
                refuse(EhError::not_allowed("Events cannot be sent to a consumer group address."))
            }
        }
    }
}

impl EhConn {
    fn accept_request(&mut self, req: &AttachRequest, node: Node) -> AttachDecision {
        self.register_request(req, node);
        // requests are small (`$cbs` tokens, `$management` reads)
        AttachDecision::Accept(AttachAccept { max_message_size: Some(MAX_REQUEST_SIZE), ..Default::default() })
    }

    fn accept_responder(&mut self, req: &AttachRequest, node: Node) -> AttachDecision {
        self.register_responder(req, node);
        AttachDecision::Accept(AttachAccept::default())
    }
}
