//! `$cbs` (claims-based security) and the `$management` request/response node of every entity.
//!
//! Shapes follow the Python SDK (`_common/mgmt_handlers.py`, `_common/constants.py`): requests
//! carry `operation` / `type` application properties and an amqp-value map body; responses carry
//! `statusCode` / `statusDescription` (`status-code` / `status-description` on `$cbs`) and
//! `errorCondition` application properties and the correlation id of the request.

use azemu_amqp::message::{Body, Message, Properties};
use azemu_amqp::types::Value;
use azemu_servicebus_sql::Params;

use crate::amqp_conn::*;
use crate::core::*;
use crate::errors::{self, SbError};
use crate::model::*;
use crate::rules::{CorrelationFilter, Rule, RuleFilter};
use crate::state::*;
use crate::store::*;

/// Most messages one `peek-message` returns (Azure: 250).
const MAX_PEEK_COUNT: i64 = 250;
/// Peeked messages are added up to about this many bytes; the rest is for the next peek.
const MAX_PEEK_BYTES: usize = 8 * 1024 * 1024;

fn get<'a>(body: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    body.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
}

fn app_prop<'a>(m: &'a Message, key: &str) -> Option<&'a Value> {
    m.app_property(key)
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (Value::str(k), v)).collect())
}

/// The messages of a `schedule-message` request, ready to store.
fn decode_scheduled(body: &[(Value, Value)], now: i64) -> Result<Vec<Incoming>, SbError> {
    let items = get(body, "messages").and_then(|v| v.as_list()).ok_or_else(|| SbError::argument("schedule-message: no messages"))?;
    if items.len() > crate::state::MAX_MESSAGES_PER_BATCH {
        return Err(SbError::argument(format!(
            "schedule-message: {} messages; at most {} can be scheduled in one request",
            items.len(),
            crate::state::MAX_MESSAGES_PER_BATCH
        )));
    }
    let mut incoming = Vec::new();
    for item in items {
        let bin =
            item.map_get("message").and_then(|v| v.as_binary()).ok_or_else(|| SbError::argument("schedule-message: message missing"))?;
        let m = Message::decode(bin).map_err(|e| SbError::argument(e.to_string()))?;
        let mut inc = Incoming::new(m, bin.len());
        // an immediate schedule time still counts as scheduled for the sequence number
        if inc.scheduled_ms.is_none() {
            inc.scheduled_ms = Some(now);
        }
        incoming.push(inc);
    }
    Ok(incoming)
}

fn uuid_list(v: Option<&Value>) -> Vec<Token> {
    v.and_then(|v| v.as_list())
        .map(|l| l.iter().filter_map(|x| if let Value::Uuid(u) = x { Some(*u) } else { None }).collect())
        .unwrap_or_default()
}

fn long_list(v: Option<&Value>) -> Vec<i64> {
    v.and_then(|v| v.as_list()).map(|l| l.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default()
}

#[derive(Clone)]
pub(crate) enum Target {
    Core(Address),
    Topic(String),
}

struct Response {
    status: i32,
    description: String,
    condition: Option<&'static str>,
    body: Option<Value>,
}

impl Response {
    fn ok(body: Option<Value>) -> Response {
        Response { status: 200, description: "OK".into(), condition: None, body }
    }
    fn no_content(description: &str) -> Response {
        Response { status: 204, description: description.into(), condition: None, body: None }
    }
    fn err(e: SbError) -> Response {
        Response { status: e.status, description: e.description, condition: Some(e.condition), body: None }
    }
}

impl SbConn {
    pub(crate) fn handle_request(&mut self, node: Node, link: azemu_amqp::api::LinkId, session: u16, req: Message, txn: Option<Vec<u8>>) {
        let correlation = req.properties.as_ref().and_then(|p| p.message_id.clone());
        let cbs = matches!(node, Node::Cbs);
        let resp = match &node {
            Node::Cbs => self.cbs_request(&req),
            Node::Management(path) => self.mgmt_request(path, &req, txn.as_deref()),
        };
        tracing::debug!("management {} -> {} {}", node_name(&node), resp.status, resp.description);
        let (sk, dk) = if cbs { ("status-code", "status-description") } else { ("statusCode", "statusDescription") };
        let mut props = vec![(Value::str(sk), Value::Int(resp.status)), (Value::str(dk), Value::String(resp.description))];
        if let Some(c) = resp.condition {
            props.push((Value::str("errorCondition"), Value::Symbol(c.to_string())));
        }
        let msg = Message {
            properties: Some(Properties { correlation_id: correlation, ..Default::default() }),
            application_properties: Some(props),
            body: match resp.body {
                Some(b) => Body::Value(b),
                None => Body::Value(Value::Null),
            },
            ..Default::default()
        };
        let reply_to = req.properties.as_ref().and_then(|p| p.reply_to.clone());
        self.respond(&node, link, session, reply_to.as_deref(), msg);
    }

    fn cbs_request(&self, req: &Message) -> Response {
        let op = app_prop(req, "operation").and_then(|v| v.as_str()).unwrap_or("");
        if op.eq_ignore_ascii_case("put-token") {
            // Every credential is accepted: SAS tokens and JWTs alike.
            Response { status: 202, description: "Accepted".into(), condition: None, body: None }
        } else {
            Response::err(SbError::new(
                azemu_amqp::condition::NOT_IMPLEMENTED,
                501,
                format!("servicebus: $cbs operation '{op}' is not implemented by azemu"),
            ))
        }
    }

    fn resolve_target(&self, st: &mut State, path: &str) -> Result<Target, SbError> {
        let ns = self.ns.clone();
        let addr = Address::parse(path).ok_or_else(|| SbError::entity_not_found(path, &ns))?;
        let nsr = st.ns(&ns);
        match &addr {
            Address::Queue { name, sub: SubQueue::Main } if nsr.topic(name).is_some() => Ok(Target::Topic(name.clone())),
            Address::Queue { .. } | Address::Subscription { .. } => {
                if nsr.exists(&addr) {
                    Ok(Target::Core(addr))
                } else {
                    Err(SbError::entity_not_found(path, &ns))
                }
            }
            _ => Err(SbError::entity_not_found(path, &ns)),
        }
    }

    fn mgmt_request(&mut self, path: &str, req: &Message, txn: Option<&[u8]>) -> Response {
        let op = app_prop(req, "operation").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let empty: Vec<(Value, Value)> = Vec::new();
        let body: &[(Value, Value)] = crate::amqp_conn::map_body(req).unwrap_or(&empty);
        let now = now_ms();
        let shared = self.shared.clone();
        // Decoding the messages of a `schedule-message` costs time in proportion to their size:
        // do it before taking the lock that every connection shares.
        let decoded = (op == "com.microsoft:schedule-message").then(|| decode_scheduled(body, now));
        let mut st = shared.state.lock();
        let target = match self.resolve_target(&mut st, path) {
            Ok(t) => t,
            Err(e) => return Response::err(e),
        };
        let ns = self.ns.clone();
        let result: Result<Response, SbError> = match (op.as_str(), &target) {
            ("com.microsoft:schedule-message", _) => match decoded {
                Some(Ok(incoming)) => match txn {
                    Some(t) => self.txn_schedule(&mut st, &target, incoming, t),
                    None => self.op_schedule(&mut st, &target, incoming, now),
                },
                Some(Err(e)) => Err(e),
                None => Err(SbError::argument("schedule-message: no messages")),
            },
            ("com.microsoft:cancel-scheduled-message", _) => match txn {
                Some(t) => self.txn_cancel(&mut st, &target, body, t),
                None => op_cancel(&mut st, &ns, &target, body),
            },
            (_, Target::Topic(_)) => Err(SbError::not_implemented(&format!("management operation '{op}' on a topic"))),
            ("com.microsoft:update-disposition", Target::Core(addr)) if txn.is_some() => {
                self.txn_update_disposition(&mut st, addr, body, txn.unwrap_or_default())
            }
            ("com.microsoft:set-session-state", Target::Core(addr)) if txn.is_some() => {
                self.txn_set_session_state(&mut st, addr, body, txn.unwrap_or_default())
            }
            (op, Target::Core(addr)) => self.core_op(&mut st, addr, op, body, now),
        };
        match result {
            Ok(r) => r,
            Err(e) => Response::err(e),
        }
    }

    fn op_schedule(&mut self, st: &mut State, target: &Target, incoming: Vec<Incoming>, now: i64) -> Result<Response, SbError> {
        let ns = self.ns.clone();
        let addr = match target {
            Target::Topic(n) => Address::Topic { name: n.clone() },
            Target::Core(a) => a.clone(),
        };
        match &addr {
            Address::Topic { name } => {
                let t = st.ns(&ns).topic(name).ok_or_else(|| SbError::entity_not_found(name, &ns))?;
                for i in &incoming {
                    t.check_send(i.size)?;
                }
            }
            other => {
                let core = st.ns(&ns).core(other).ok_or_else(|| SbError::entity_not_found(&path_of(other), &ns))?;
                for i in &incoming {
                    core.check_send(i.size)?;
                }
            }
        }
        let mut seqs = Vec::new();
        for inc in incoming {
            seqs.push(Value::Long(st.send(&ns, &addr, inc, now)? as i64));
        }
        Ok(Response::ok(Some(map(vec![("sequence-numbers", Value::Array(seqs))]))))
    }

    /// `schedule-message` inside a transaction: the sequence numbers are answered now, the messages are
    /// stored under them when the transaction commits.
    fn txn_schedule(&mut self, st: &mut State, target: &Target, incoming: Vec<Incoming>, txn: &[u8]) -> Result<Response, SbError> {
        let ns = self.ns.clone();
        let now = std::time::Instant::now();
        self.txns.check_open(txn, now)?;
        let addr = match target {
            Target::Topic(n) => Address::Topic { name: n.clone() },
            Target::Core(a) => a.clone(),
        };
        for i in &incoming {
            crate::txn::target_exists(st, &ns, &addr, i, true)?;
        }
        let mut items = Vec::new();
        let mut seqs = Vec::new();
        for inc in incoming {
            let seq = st.reserve_seq(&ns, &addr)?;
            seqs.push(Value::Long(seq as i64));
            items.push((inc, seq));
        }
        let weight = items.len();
        self.txns.add(txn, crate::txn::TxOp::Schedule { target: addr, items }, weight, now)?;
        Ok(Response::ok(Some(map(vec![("sequence-numbers", Value::Array(seqs))]))))
    }

    fn txn_cancel(&mut self, st: &mut State, target: &Target, body: &[(Value, Value)], txn: &[u8]) -> Result<Response, SbError> {
        let ns = self.ns.clone();
        let now = std::time::Instant::now();
        self.txns.check_open(txn, now)?;
        let seqs = long_list(get(body, "sequence-numbers"));
        for s in &seqs {
            let nsr = st.ns(&ns);
            let found = match target {
                Target::Topic(name) => nsr.topic(name).map(|t| t.scheduled.get(*s as u64).is_some()).unwrap_or(false),
                Target::Core(a) => nsr.core(a).and_then(|c| c.main.get(*s as u64)).map(|m| m.state == MsgState::Scheduled).unwrap_or(false),
            };
            if !found {
                return Err(SbError::message_not_found(*s));
            }
        }
        let weight = seqs.len().max(1);
        self.txns.add(txn, crate::txn::TxOp::Cancel { target: target.clone(), seqs }, weight, now)?;
        Ok(Response::ok(None))
    }

    /// `update-disposition` (settle by lock token) inside a transaction.
    fn txn_update_disposition(&mut self, st: &mut State, addr: &Address, body: &[(Value, Value)], txn: &[u8]) -> Result<Response, SbError> {
        let ns = self.ns.clone();
        let status = get(body, "disposition-status").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let reason = get(body, "deadletter-reason").and_then(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty());
        let desc = get(body, "deadletter-description").and_then(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty());
        let modify = get(body, "properties-to-modify").and_then(|v| v.as_map()).map(|m| m.to_vec());
        let outcome = match status.as_str() {
            "completed" => Outcome::Complete,
            "abandoned" => Outcome::Abandon { modify },
            "defered" | "deferred" => Outcome::Defer { modify },
            "suspended" => Outcome::DeadLetter { reason, description: desc, modify },
            "released" => Outcome::Release,
            other => return Err(SbError::argument(format!("unknown disposition status '{other}'"))),
        };
        let tokens = uuid_list(get(body, "lock-tokens"));
        let now = std::time::Instant::now();
        self.txns.check_open(txn, now)?;
        let core = core_of(st, &ns, addr)?;
        for t in &tokens {
            core.check_lock(t, now_ms())?;
        }
        for t in tokens {
            self.txns.add(txn, crate::txn::TxOp::Settle { addr: addr.clone(), token: t, outcome: outcome.clone() }, 1, now)?;
        }
        Ok(Response::ok(None))
    }

    fn txn_set_session_state(&mut self, st: &mut State, addr: &Address, body: &[(Value, Value)], txn: &[u8]) -> Result<Response, SbError> {
        let ns = self.ns.clone();
        let sid = get(body, "session-id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| SbError::argument("session-id is required"))?;
        let state = get(body, "session-state").and_then(|v| v.as_binary()).map(|b| b.to_vec()).filter(|s| !s.is_empty());
        let now = std::time::Instant::now();
        self.txns.check_open(txn, now)?;
        if !core_of(st, &ns, addr)?.session_locked(&sid, now_ms()) {
            return Err(SbError::session_lock_lost());
        }
        self.txns.add(txn, crate::txn::TxOp::SessionState { addr: addr.clone(), session: sid, state }, 1, now)?;
        Ok(Response::ok(None))
    }

    fn core_op(&mut self, st: &mut State, addr: &Address, op: &str, body: &[(Value, Value)], now: i64) -> Result<Response, SbError> {
        let ns = self.ns.clone();
        let sq = match addr {
            Address::Queue { sub, .. } | Address::Subscription { sub, .. } => *sub,
            _ => SubQueue::Main,
        };
        let owner = Owner::default();
        let session_id = get(body, "session-id").and_then(|v| v.as_str()).map(str::to_string);
        match op {
            "com.microsoft:renew-lock" => {
                let core = core_of(st, &ns, addr)?;
                let mut out = Vec::new();
                for t in uuid_list(get(body, "lock-tokens")) {
                    out.push(Value::Timestamp(core.renew_lock(&t, now)?));
                }
                Ok(Response::ok(Some(map(vec![("expirations", Value::Array(out))]))))
            }
            "com.microsoft:peek-message" => {
                let from = get(body, "from-sequence-number").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as u64;
                let count = get(body, "message-count").and_then(|v| v.as_i64()).unwrap_or(1).clamp(0, MAX_PEEK_COUNT) as usize;
                let core = core_of(st, &ns, addr)?;
                let msgs = core.peek_limited(sq, from, count, session_id.as_deref(), now, MAX_PEEK_BYTES);
                if msgs.is_empty() {
                    return Ok(Response::no_content("No Content"));
                }
                let list: Vec<Value> =
                    msgs.iter().map(|m| map(vec![("message", Value::Binary(render(m, Some(state_code(m))).encode()))])).collect();
                Ok(Response::ok(Some(map(vec![("messages", Value::List(list))]))))
            }
            "com.microsoft:receive-by-sequence-number" => {
                let seqs = long_list(get(body, "sequence-numbers"));
                let mode = if get(body, "receiver-settle-mode").and_then(|v| v.as_u64()).unwrap_or(1) == 0 {
                    ReceiveMode::ReceiveAndDelete
                } else {
                    ReceiveMode::PeekLock
                };
                let core = core_of(st, &ns, addr)?;
                let got = core.receive_deferred(&seqs, mode, owner, now)?;
                let list: Vec<Value> = got
                    .iter()
                    .map(|d| {
                        let mut pairs = vec![("message", Value::Binary(render(&d.msg, Some(1)).encode()))];
                        if let Some(t) = d.token {
                            pairs.push(("lock-token", Value::Uuid(t)));
                        }
                        map(pairs)
                    })
                    .collect();
                Ok(Response::ok(Some(map(vec![("messages", Value::List(list))]))))
            }
            "com.microsoft:update-disposition" => {
                let status = get(body, "disposition-status").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let reason = get(body, "deadletter-reason").and_then(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty());
                let desc = get(body, "deadletter-description").and_then(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty());
                let modify = get(body, "properties-to-modify").and_then(|v| v.as_map()).map(|m| m.to_vec());
                let make = |status: &str| -> Result<Outcome, SbError> {
                    Ok(match status {
                        "completed" => Outcome::Complete,
                        "abandoned" => Outcome::Abandon { modify: modify.clone() },
                        "defered" | "deferred" => Outcome::Defer { modify: modify.clone() },
                        "suspended" => Outcome::DeadLetter { reason: reason.clone(), description: desc.clone(), modify: modify.clone() },
                        "released" => Outcome::Release,
                        other => return Err(SbError::argument(format!("unknown disposition status '{other}'"))),
                    })
                };
                let tokens = uuid_list(get(body, "lock-tokens"));
                let mut fwd = Vec::new();
                {
                    let core = core_of(st, &ns, addr)?;
                    for t in &tokens {
                        let outcome = make(&status)?;
                        // a request that names a session is made by a session receiver
                        fwd.extend(core.settle(t, outcome, now).map_err(|e| if session_id.is_some() { e.in_session() } else { e })?);
                    }
                }
                st.forward_all(&ns, fwd, now);
                Ok(Response::ok(None))
            }
            "com.microsoft:renew-session-lock" => {
                let sid = session_id.ok_or_else(|| SbError::argument("session-id is required"))?;
                let until = core_of(st, &ns, addr)?.renew_session_lock(&sid, now)?;
                Ok(Response::ok(Some(map(vec![("expiration", Value::Timestamp(until))]))))
            }
            "com.microsoft:get-session-state" => {
                let sid = session_id.ok_or_else(|| SbError::argument("session-id is required"))?;
                let core = core_of(st, &ns, addr)?;
                if !core.session_locked(&sid, now) {
                    return Err(SbError::session_lock_lost());
                }
                let state = core.session_state(&sid);
                Ok(Response::ok(Some(map(vec![("session-state", state.map(Value::Binary).unwrap_or(Value::Null))]))))
            }
            "com.microsoft:set-session-state" => {
                let sid = session_id.ok_or_else(|| SbError::argument("session-id is required"))?;
                let state = get(body, "session-state").and_then(|v| v.as_binary()).map(|b| b.to_vec());
                core_of(st, &ns, addr)?.set_session_state(&sid, state, now)?;
                Ok(Response::ok(None))
            }
            "com.microsoft:get-message-sessions" => {
                let skip = get(body, "skip").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
                let top = get(body, "top").and_then(|v| v.as_i64()).unwrap_or(100).clamp(0, 10_000) as usize;
                let core = core_of(st, &ns, addr)?;
                let ids = core.session_ids(now);
                let page: Vec<Value> = ids.iter().skip(skip).take(top).map(|s| Value::String(s.clone())).collect();
                if page.is_empty() {
                    return Ok(Response::no_content("No Content"));
                }
                let next = if skip + page.len() >= ids.len() { 0 } else { (skip + page.len()) as i32 };
                Ok(Response::ok(Some(map(vec![("skip", Value::Int(next)), ("sessions-ids", Value::Array(page))]))))
            }
            "com.microsoft:add-rule" | "com.microsoft:remove-rule" | "com.microsoft:enumerate-rules" => {
                rule_op(st, &ns, addr, op, body, now)
            }
            other => Err(SbError::not_implemented(&format!("management operation '{other}'"))),
        }
    }
}

fn node_name(n: &Node) -> String {
    match n {
        Node::Cbs => "$cbs".into(),
        Node::Management(p) => format!("{p}/$management"),
    }
}

pub(crate) fn path_of(a: &Address) -> String {
    match a {
        Address::Queue { name, .. } | Address::Topic { name } => name.clone(),
        Address::Subscription { topic, name, .. } => format!("{topic}/Subscriptions/{name}"),
        _ => String::new(),
    }
}

fn core_of<'a>(st: &'a mut State, ns: &str, addr: &Address) -> Result<&'a mut Core, SbError> {
    st.ns(ns).core_mut(addr).ok_or_else(|| SbError::entity_not_found(&path_of(addr), ns))
}

fn op_cancel(st: &mut State, ns: &str, target: &Target, body: &[(Value, Value)]) -> Result<Response, SbError> {
    let seqs = long_list(get(body, "sequence-numbers"));
    op_cancel_scheduled(st, ns, target, &seqs)?;
    Ok(Response::ok(None))
}

/// Cancel scheduled messages (a plain `cancel-scheduled-message`, or the commit of one made in a transaction).
pub(crate) fn op_cancel_scheduled(st: &mut State, ns: &str, target: &Target, seqs: &[i64]) -> Result<(), SbError> {
    match target {
        Target::Topic(name) => {
            let t = st.ns(ns).topic_mut(name).ok_or_else(|| SbError::entity_not_found(name, ns))?;
            for s in seqs {
                match t.scheduled.get(*s as u64) {
                    Some(_) => {
                        t.scheduled.remove(*s as u64);
                    }
                    None => return Err(SbError::message_not_found(*s)),
                }
            }
        }
        Target::Core(addr) => {
            let core = core_of(st, ns, addr)?;
            for s in seqs {
                core.cancel_scheduled(*s as u64)?;
            }
        }
    }
    Ok(())
}

// ---- rules over $management ------------------------------------------------------------------

// Descriptor codes of the rule types (the .NET SDK's `Amqp*Codec` classes and `@azure/core-amqp`
// `Constants.descriptorCodes` agree on these). The rule description and the two rule actions are
// 0x00000137_0000000N; the four filters are 0x00000013_7000000N, one digit shorter.
const D_RULE_DESCRIPTION: u64 = 0x0000_0137_0000_0004;
const D_EMPTY_ACTION: u64 = 0x0000_0137_0000_0005;
const D_SQL_ACTION: u64 = 0x0000_0137_0000_0006;
const D_SQL_FILTER: u64 = 0x0000_0013_7000_0006;
const D_TRUE_FILTER: u64 = 0x0000_0013_7000_0007;
const D_FALSE_FILTER: u64 = 0x0000_0013_7000_0008;
const D_CORRELATION_FILTER: u64 = 0x0000_0013_7000_0009;

fn rule_op(st: &mut State, ns: &str, addr: &Address, op: &str, body: &[(Value, Value)], now: i64) -> Result<Response, SbError> {
    let Address::Subscription { topic, name, .. } = addr else {
        return Err(SbError::not_allowed("rules exist only on topic subscriptions"));
    };
    let sub =
        st.ns(ns).topic_mut(topic).and_then(|t| t.subs.get_mut(&key(name))).ok_or_else(|| SbError::entity_not_found(&path_of(addr), ns))?;
    match op {
        "com.microsoft:add-rule" => {
            let rname = get(body, "rule-name").and_then(|v| v.as_str()).ok_or_else(|| SbError::argument("rule-name is required"))?;
            let desc = get(body, "rule-description").ok_or_else(|| SbError::argument("rule-description is required"))?;
            let rule = rule_from_amqp(rname, desc, now).map_err(SbError::argument)?;
            if sub.rules.iter().any(|r| r.name.eq_ignore_ascii_case(rname)) {
                return Err(SbError::new(
                    errors::ENTITY_ALREADY_EXISTS,
                    409,
                    format!("The messaging entity '{}/Rules/{rname}' already exists.", path_of(addr)),
                ));
            }
            sub.rules.push(rule);
            next_revision();
            Ok(Response::ok(None))
        }
        "com.microsoft:remove-rule" => {
            let rname = get(body, "rule-name").and_then(|v| v.as_str()).ok_or_else(|| SbError::argument("rule-name is required"))?;
            let before = sub.rules.len();
            sub.rules.retain(|r| !r.name.eq_ignore_ascii_case(rname));
            next_revision();
            if sub.rules.len() == before {
                return Err(SbError::new(
                    errors::ENTITY_NOT_FOUND,
                    404,
                    format!("The messaging entity '{}/Rules/{rname}' could not be found.", path_of(addr)),
                ));
            }
            Ok(Response::ok(None))
        }
        _ => {
            let skip = get(body, "skip").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
            let top = get(body, "top").and_then(|v| v.as_i64()).unwrap_or(100).clamp(0, 1000) as usize;
            let list: Vec<Value> =
                sub.rules.iter().skip(skip).take(top).map(|r| map(vec![("rule-description", rule_to_amqp(r))])).collect();
            Ok(Response::ok(Some(map(vec![("rules", Value::List(list))]))))
        }
    }
}

fn sql_value_to_amqp(v: &azemu_servicebus_sql::Value) -> Value {
    match v {
        azemu_servicebus_sql::Value::Null => Value::Null,
        azemu_servicebus_sql::Value::Bool(b) => Value::Bool(*b),
        azemu_servicebus_sql::Value::Long(l) => Value::Long(*l),
        azemu_servicebus_sql::Value::Double(d) => Value::Double(*d),
        azemu_servicebus_sql::Value::Str(s) => Value::String(s.clone()),
        azemu_servicebus_sql::Value::Timestamp(t) => Value::Timestamp(*t),
    }
}

fn amqp_params(v: Option<&Value>) -> Params {
    let mut p = Params::new();
    if let Some(Value::Map(m)) = v {
        for (k, v) in m {
            let Some(k) = k.as_str() else { continue };
            let val = match v {
                Value::Bool(b) => azemu_servicebus_sql::Value::Bool(*b),
                Value::String(s) | Value::Symbol(s) => azemu_servicebus_sql::Value::Str(s.clone()),
                Value::Double(d) => azemu_servicebus_sql::Value::Double(*d),
                Value::Float(d) => azemu_servicebus_sql::Value::Double(*d as f64),
                Value::Timestamp(t) => azemu_servicebus_sql::Value::Timestamp(*t),
                other => match other.as_i64() {
                    Some(i) => azemu_servicebus_sql::Value::Long(i),
                    None => azemu_servicebus_sql::Value::Null,
                },
            };
            p.insert(k.to_string(), val);
        }
    }
    p
}

fn desc(code: u64, fields: Vec<Value>) -> Value {
    Value::described_code(code, Value::List(fields))
}

/// A rule as `enumerate-rules` answers it: a described list `[filter, action, name, created-at]`
/// whose filter and action are described lists too (the SQL ones hold the expression and the
/// compatibility level; AMQP has no place for SQL parameters).
pub fn rule_to_amqp(r: &Rule) -> Value {
    let filter = match &r.filter {
        RuleFilter::True => desc(D_TRUE_FILTER, vec![]),
        RuleFilter::False => desc(D_FALSE_FILTER, vec![]),
        // the model's `$Default` rule is the SQL filter `1=1`; Service Bus calls it a true filter (as the HTTP API does)
        RuleFilter::Sql { expression, compatibility_level: 20, .. } if r.name == crate::rules::DEFAULT_RULE_NAME && expression == "1=1" => {
            desc(D_TRUE_FILTER, vec![])
        }
        RuleFilter::Sql { expression, compatibility_level, .. } => {
            desc(D_SQL_FILTER, vec![Value::String(expression.clone()), Value::Int(*compatibility_level)])
        }
        RuleFilter::Correlation(c) => {
            let s = |o: &Option<String>| o.clone().map(Value::String).unwrap_or(Value::Null);
            // the map is always there: the JavaScript SDK throws on a null properties field
            let props = Value::Map(c.properties.iter().map(|(k, v)| (Value::str(k.clone()), sql_value_to_amqp(v))).collect());
            desc(
                D_CORRELATION_FILTER,
                vec![
                    s(&c.correlation_id),
                    s(&c.message_id),
                    s(&c.to),
                    s(&c.reply_to),
                    s(&c.label),
                    s(&c.session_id),
                    s(&c.reply_to_session_id),
                    s(&c.content_type),
                    props,
                ],
            )
        }
    };
    let action = match &r.action {
        None => desc(D_EMPTY_ACTION, vec![]),
        Some(a) => desc(D_SQL_ACTION, vec![Value::String(a.expression.clone()), Value::Int(20)]),
    };
    desc(D_RULE_DESCRIPTION, vec![filter, action, Value::String(r.name.clone()), Value::Timestamp(r.created_ms)])
}

fn code_of(v: &Value) -> Option<u64> {
    let d = v.as_described()?;
    d.descriptor.as_u64().or_else(|| match d.descriptor.as_str()? {
        "com.microsoft:rule-description:list" => Some(D_RULE_DESCRIPTION),
        "com.microsoft:sql-filter:list" => Some(D_SQL_FILTER),
        "com.microsoft:true-filter:list" => Some(D_TRUE_FILTER),
        "com.microsoft:false-filter:list" => Some(D_FALSE_FILTER),
        "com.microsoft:correlation-filter:list" => Some(D_CORRELATION_FILTER),
        "com.microsoft:sql-rule-action:list" => Some(D_SQL_ACTION),
        "com.microsoft:empty-rule-action:list" => Some(D_EMPTY_ACTION),
        _ => None,
    })
}

/// Build a rule from the `rule-description` of an `add-rule` request. The SDKs (JavaScript, .NET)
/// send a map: `sql-filter` or `correlation-filter` (a map), optionally `sql-rule-action`
/// (`{expression}`) and `rule-name`. The described-list form of `enumerate-rules` is accepted too.
pub fn rule_from_amqp(name: &str, desc: &Value, now: i64) -> Result<Rule, String> {
    match desc {
        Value::Map(_) => rule_from_map(name, desc, now),
        Value::Described(_) => rule_from_described(name, desc, now),
        _ => Err("rule-description is neither a map nor a described list".into()),
    }
}

/// A string-valued entry of a map, null (an unset SDK property) counts as absent.
fn map_str(m: &Value, key: &str) -> Option<String> {
    m.map_get(key).and_then(|v| v.undescribed().as_str()).map(str::to_string)
}

fn rule_from_map(name: &str, desc: &Value, now: i64) -> Result<Rule, String> {
    let mut rule = if let Some(f) = desc.map_get("sql-filter") {
        let expr = map_str(f, "expression").ok_or("the sql-filter has no expression")?;
        Rule::sql_filter(name, &expr, amqp_params(f.map_get("parameters")), now)?
    } else if let Some(f) = desc.map_get("correlation-filter") {
        let mut cf = CorrelationFilter {
            correlation_id: map_str(f, "correlation-id"),
            message_id: map_str(f, "message-id"),
            to: map_str(f, "to"),
            reply_to: map_str(f, "reply-to"),
            label: map_str(f, "label"),
            session_id: map_str(f, "session-id"),
            reply_to_session_id: map_str(f, "reply-to-session-id"),
            content_type: map_str(f, "content-type"),
            properties: Default::default(),
        };
        cf.properties = amqp_params(f.map_get("properties")).into_iter().collect();
        Rule { name: name.into(), filter: RuleFilter::Correlation(cf), action: None, created_ms: now }
    } else {
        return Err("rule-description has neither a sql-filter nor a correlation-filter".into());
    };
    if let Some(a) = desc.map_get("sql-rule-action").filter(|a| a.as_map().is_some()) {
        if let Some(expr) = map_str(a, "expression") {
            rule = rule.with_action(&expr, amqp_params(a.map_get("parameters")))?;
        }
    }
    Ok(rule)
}

fn rule_from_described(name: &str, desc: &Value, now: i64) -> Result<Rule, String> {
    let fields = match desc.as_described().map(|d| &d.value) {
        Some(Value::List(l)) => l.clone(),
        _ => return Err("rule-description is not a described list".into()),
    };
    let filter = fields.first().ok_or("rule-description has no filter")?;
    let f = |i: usize| -> Option<&Value> { filter.as_described().and_then(|d| d.value.as_list()).and_then(|l| l.get(i)) };
    let mut rule = match code_of(filter) {
        Some(D_SQL_FILTER) => {
            let expr = f(0).and_then(|v| v.as_str()).ok_or("sql filter without expression")?;
            Rule::sql_filter(name, expr, amqp_params(f(3)), now)?
        }
        Some(D_TRUE_FILTER) => Rule { name: name.into(), filter: RuleFilter::True, action: None, created_ms: now },
        Some(D_FALSE_FILTER) => Rule { name: name.into(), filter: RuleFilter::False, action: None, created_ms: now },
        Some(D_CORRELATION_FILTER) => {
            let s = |i: usize| f(i).and_then(|v| v.as_str()).map(str::to_string);
            let mut cf = CorrelationFilter {
                correlation_id: s(0),
                message_id: s(1),
                to: s(2),
                reply_to: s(3),
                label: s(4),
                session_id: s(5),
                reply_to_session_id: s(6),
                content_type: s(7),
                properties: Default::default(),
            };
            cf.properties = amqp_params(f(8)).into_iter().collect();
            Rule { name: name.into(), filter: RuleFilter::Correlation(cf), action: None, created_ms: now }
        }
        _ => return Err("unknown filter type".into()),
    };
    if let Some(a) = fields.get(1) {
        if code_of(a) == Some(D_SQL_ACTION) {
            let l = a.as_described().and_then(|d| d.value.as_list()).unwrap_or(&[]);
            if let Some(expr) = l.first().and_then(|v| v.as_str()) {
                rule = rule.with_action(expr, amqp_params(l.get(3)))?;
            }
        }
    }
    Ok(rule)
}
