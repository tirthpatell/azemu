//! The inspector of Service Bus (`docs/inspector.md`): namespaces, queues, topics and subscriptions with their settings and live
//! counts, subscription rules, authorization rules (names and rights; the keys are not shown), and the messages of each state
//! (active, locked, deferred, scheduled, dead-letter) and the sessions. It only reads: nothing is locked, received, settled or
//! counted as delivered, no lock or time to live is applied on the way (the ticker does that), and a namespace that does not exist
//! is a 404 and is not made.

use std::ops::Bound::{Excluded, Unbounded};

use azemu_amqp::inspect::message_json;
use azemu_core::inspect::{body_view, rfc3339, InspectError, InspectRequest, InspectResult, Node};
use azemu_core::time::OffsetDateTime;
use serde_json::{json, Map, Value};

use crate::core::{Core, ANN_PARTITION_KEY, PROP_DEAD_LETTER_DESCRIPTION, PROP_DEAD_LETTER_REASON};
use crate::rules::{Rule, RuleFilter};
use crate::state::{Namespace, State, Subscription, Topic};
use crate::store::{Msg, MsgState, Store};
use crate::Shared;

fn ms(ms: i64) -> Value {
    match OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000) {
        Ok(t) => json!(rfc3339(t)),
        Err(_) => json!(ms),
    }
}

fn missing(req: &InspectRequest) -> InspectError {
    InspectError::not_found(format!("There is no node {}.", req.own_path()))
}

pub(crate) fn node(shared: &Shared, req: &InspectRequest) -> InspectResult {
    let state = shared.state.lock();
    let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
    let [ns_name, rest @ ..] = segs.as_slice() else {
        return Ok(service_node(&state, req));
    };
    let ns = state
        .namespaces
        .get(&state.resolve(ns_name))
        .ok_or_else(|| InspectError::not_found(format!("There is no namespace '{ns_name}'.")))?;
    let at = |more: &[&str]| {
        let mut p: Vec<&str> = vec![ns_name];
        p.extend_from_slice(more);
        req.path(&p)
    };
    match rest {
        [] => Ok(namespace_node(shared, ns_name, ns, req)),
        ["queues"] => {
            let (rows, next) = req.page_map(&ns.queues);
            let items = rows.into_iter().map(|(k, q)| entity_row(&at(&["queues", k]), &q.name, &q.core)).collect();
            Ok(Node::new(at(&["queues"]), "queues", "queues").prop("count", ns.queues.len()).with_items(items, next))
        }
        ["queues", q, tail @ ..] => {
            let queue = ns.queues.get(*q).ok_or_else(|| InspectError::not_found(format!("There is no queue '{q}'.")))?;
            core_tree(shared, ns_name, &format!("queues/{q}"), "queue", &queue.name, &queue.core, tail, req, &at(&["queues", q]))
        }
        ["topics"] => {
            let (rows, next) = req.page_map(&ns.topics);
            let items = rows
                .into_iter()
                .map(|(k, t)| json!({"name": t.name, "path": at(&["topics", k]), "subscriptions": t.subs.len(), "scheduledForFanOut": t.scheduled.len(), "status": t.settings.status}))
                .collect();
            Ok(Node::new(at(&["topics"]), "topics", "topics").prop("count", ns.topics.len()).with_items(items, next))
        }
        ["topics", t, tail @ ..] => {
            let topic = ns.topics.get(*t).ok_or_else(|| InspectError::not_found(format!("There is no topic '{t}'.")))?;
            topic_tree(shared, ns_name, t, topic, tail, req, &at)
        }
        _ => Err(missing(req)),
    }
}

fn service_node(state: &State, req: &InspectRequest) -> Node {
    let (rows, next) = req.page_map(&state.namespaces);
    let items = rows
        .into_iter()
        .map(|(k, n)| json!({"name": k, "path": req.path(&[k]), "queues": n.queues.len(), "topics": n.topics.len()}))
        .collect();
    Node::new(req.path(&[]), "service", "servicebus").prop("accounts", state.namespaces.len()).with_items(items, next)
}

fn camel(s: &str) -> String {
    let mut out = String::new();
    let mut up = false;
    for c in s.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// The authorization rules that are declared or made by Resource Manager at one level: name and rights, never the keys.
fn rules_view(shared: &Shared, ns: &str, entity: Option<&str>) -> Value {
    let list: Vec<Value> = shared.sas.at(ns, entity).iter().map(|r| json!({"name": r.name, "rights": r.rights.names()})).collect();
    json!(list)
}

fn settings_props(mut node: Node, s: &crate::model::Settings) -> Node {
    if let Ok(Value::Object(m)) = serde_json::to_value(s) {
        for (k, v) in m {
            match k.as_str() {
                // the shared access keys of the old management API are not shown
                "auth_rules" => {
                    let names: Vec<Value> =
                        v.as_array().into_iter().flatten().map(|r| json!({"name": r["key_name"], "rights": r["rights"]})).collect();
                    node = node.prop("atomAuthorizationRules", names);
                }
                "created_ms" | "updated_ms" | "accessed_ms" => {
                    node = node.prop(&camel(&k.replace("_ms", "_at")), ms(v.as_i64().unwrap_or(0)))
                }
                _ => node = node.prop(&camel(&k), v),
            }
        }
    }
    node
}

fn namespace_node(shared: &Shared, name: &str, ns: &Namespace, req: &InspectRequest) -> Node {
    Node::new(req.path(&[name]), "namespace", name)
        .prop("authorizationRules", rules_view(shared, name, None))
        .child("queues", "queues", Some(ns.queues.len() as u64), req.path(&[name, "queues"]))
        .child("topics", "topics", Some(ns.topics.len() as u64), req.path(&[name, "topics"]))
}

/// How many messages of each state a store holds. A lock that has run out and was not applied yet still counts as locked.
#[derive(Default)]
struct ByState {
    active: usize,
    locked: usize,
    deferred: usize,
    scheduled: usize,
}

fn by_state(store: &Store) -> ByState {
    let mut c = ByState::default();
    for m in store.msgs.values() {
        match m.state {
            MsgState::Ready => c.active += 1,
            MsgState::Locked => c.locked += 1,
            MsgState::Deferred => c.deferred += 1,
            MsgState::Scheduled => c.scheduled += 1,
        }
    }
    c
}

fn entity_row(path: &str, name: &str, core: &Core) -> Value {
    let c = by_state(&core.main);
    json!({"name": name, "path": path, "active": c.active, "locked": c.locked, "deferred": c.deferred, "scheduled": c.scheduled,
           "deadLetter": core.dlq.len(), "sizeInBytes": core.size_bytes(), "status": core.settings.status})
}

/// A queue or a subscription: its settings, counts, and the lists of messages.
#[allow(clippy::too_many_arguments)]
fn core_tree(
    shared: &Shared,
    ns: &str,
    entity: &str,
    kind: &str,
    title: &str,
    core: &Core,
    tail: &[&str],
    req: &InspectRequest,
    own: &str,
) -> InspectResult {
    let path = |more: &str| format!("{own}/{more}");
    match tail {
        [] => {
            let c = by_state(&core.main);
            let mut n = Node::new(own, kind, title);
            n = settings_props(n, &core.settings)
                .prop("authorizationRules", rules_view(shared, ns, Some(entity)))
                .prop("activeMessages", c.active)
                .prop("lockedMessages", c.locked)
                .prop("deferredMessages", c.deferred)
                .prop("scheduledMessages", c.scheduled)
                .prop("deadLetterMessages", core.dlq.len())
                .prop("transferDeadLetterMessages", core.tdlq.len())
                .prop("sizeInBytes", core.size_bytes())
                .prop("nextSequenceNumber", core.main.next_seq())
                .prop("sessions", session_ids(core).len())
                .prop("duplicateDetectionIds", core.dup.len());
            for (name, count) in [("active", c.active), ("locked", c.locked), ("deferred", c.deferred), ("scheduled", c.scheduled)] {
                n = n.child(name, "messages", Some(count as u64), path(name));
            }
            n = n.child("deadletter", "messages", Some(core.dlq.len() as u64), path("deadletter"));
            if !core.tdlq.is_empty() {
                n = n.child("transfer-deadletter", "messages", Some(core.tdlq.len() as u64), path("transfer-deadletter"));
            }
            if core.settings.requires_session || !core.sessions.is_empty() {
                n = n.child("sessions", "sessions", Some(session_ids(core).len() as u64), path("sessions"));
            }
            Ok(n)
        }
        ["active"] => Ok(messages_node(&core.main, Some(MsgState::Ready), &path("active"), "active messages", req)),
        ["locked"] => Ok(messages_node(&core.main, Some(MsgState::Locked), &path("locked"), "locked messages", req)),
        ["deferred"] => Ok(messages_node(&core.main, Some(MsgState::Deferred), &path("deferred"), "deferred messages", req)),
        ["scheduled"] => Ok(messages_node(&core.main, Some(MsgState::Scheduled), &path("scheduled"), "scheduled messages", req)),
        ["deadletter"] => Ok(messages_node(&core.dlq, None, &path("deadletter"), "dead-lettered messages", req)),
        ["transfer-deadletter"] => {
            Ok(messages_node(&core.tdlq, None, &path("transfer-deadletter"), "transfer dead-lettered messages", req))
        }
        ["sessions"] => Ok(sessions_node(core, &path("sessions"), req)),
        ["sessions", id] => {
            if !session_ids(core).iter().any(|s| s.as_str() == *id) {
                return Err(InspectError::not_found(format!("There is no session '{id}'.")));
            }
            Ok(session_node(&path(&format!("sessions/{}", azemu_core::inspect::seg(id))), id, core.sessions.get(*id), core))
        }
        _ => Err(missing(req)),
    }
}

fn state_name(m: &Msg, now: i64) -> &'static str {
    match m.state {
        MsgState::Ready => "active",
        MsgState::Locked if m.lock.as_ref().is_some_and(|l| !l.via_session && l.until_ms <= now) => {
            "locked (the lock has run out; released at the next tick)"
        }
        MsgState::Locked => "locked",
        MsgState::Deferred => "deferred",
        MsgState::Scheduled => "scheduled",
    }
}

fn message_row(m: &Msg, now: i64) -> Value {
    let mut row = Map::new();
    row.insert("name".into(), json!(m.seq));
    row.insert("sequenceNumber".into(), json!(m.seq));
    row.insert("state".into(), json!(state_name(m, now)));
    row.insert("enqueuedSequenceNumber".into(), json!(m.enq_seq));
    row.insert("enqueuedTime".into(), ms(m.enqueued_ms));
    row.insert("deliveryCount".into(), json!(m.delivery_count));
    row.insert("sessionId".into(), json!(m.session_id));
    row.insert("scheduledEnqueueTime".into(), m.scheduled_ms.map_or(Value::Null, ms));
    row.insert("timeToLiveMs".into(), json!(m.ttl_ms));
    row.insert("expiresAt".into(), m.expires_ms.filter(|e| *e < crate::core::MAX_EXPIRY_MS).map_or(Value::Null, ms));
    row.insert("sizeInBytes".into(), json!(m.size));
    if let Some(l) = &m.lock {
        // who holds it: the connection and link that received it (0 and 0: nobody in particular). The lock token is not shown.
        row.insert(
            "lock".into(),
            json!({"until": ms(l.until_ms), "heldByConnection": l.owner.conn, "heldByLink": l.owner.link, "throughSession": l.via_session}),
        );
    }
    row.insert("deferred".into(), json!(m.deferred));
    row.insert("deadLetterSource".into(), json!(m.dead_letter_source));
    let message = message_json(&m.message);
    if let Some(Value::Object(app)) = message.get("applicationProperties") {
        if let Some(r) = app.get(PROP_DEAD_LETTER_REASON) {
            row.insert("deadLetterReason".into(), r.clone());
        }
        if let Some(d) = app.get(PROP_DEAD_LETTER_DESCRIPTION) {
            row.insert("deadLetterDescription".into(), d.clone());
        }
    }
    if let Some(Value::Object(ann)) = message.get("annotations") {
        if let Some(p) = ann.get(ANN_PARTITION_KEY) {
            row.insert("partitionKey".into(), p.clone());
        }
    }
    row.extend(message);
    Value::Object(row)
}

/// The messages of one store, in sequence order, of one state (`None`: all of them). The page reads from after the marker and stops when it is full.
fn messages_node(store: &Store, only: Option<MsgState>, own: &str, title: &str, req: &InspectRequest) -> Node {
    let now = crate::state::now_ms();
    let after = req.marker().and_then(|m| m.parse::<u64>().ok());
    let lower = after.map_or(Unbounded, Excluded);
    let rows = store
        .msgs
        .range((lower, Unbounded))
        .filter(|(_, m)| only.is_none_or(|s| m.state == s))
        .map(|(seq, m)| (seq.to_string(), message_row(m, now)));
    let (items, next) = req.page_keyed(rows);
    Node::new(own, "messages", title).with_items(items, next)
}

/// Every session: those with a record (a lock or a state) and those that only have messages waiting.
fn session_ids(core: &Core) -> Vec<&String> {
    let mut ids: std::collections::BTreeSet<&String> = core.sessions.keys().collect();
    ids.extend(core.main.msgs.values().filter_map(|m| m.session_id.as_ref()));
    ids.into_iter().collect()
}

fn sessions_node(core: &Core, own: &str, req: &InspectRequest) -> Node {
    let ids = session_ids(core);
    let (range, next) = req.page_range(ids.len());
    let now = crate::state::now_ms();
    let items = ids[range]
        .iter()
        .map(|id| {
            let s = core.sessions.get(*id);
            json!({"name": id, "path": format!("{own}/{}", azemu_core::inspect::seg(id)), "locked": s.and_then(|s| s.lock.as_ref()).is_some_and(|l| l.until_ms > now),
                   "lockedUntil": s.and_then(|s| s.lock.as_ref()).map(|l| ms(l.until_ms)), "stateBytes": s.and_then(|s| s.state.as_ref()).map(Vec::len),
                   "lastUpdated": s.map(|s| ms(s.last_updated_ms))})
        })
        .collect();
    Node::new(own, "sessions", "sessions").prop("count", ids.len()).with_items(items, next)
}

fn session_node(own: &str, id: &str, s: Option<&crate::core::SessionInfo>, core: &Core) -> Node {
    let ready = core.main.msgs.values().filter(|m| m.session_id.as_deref() == Some(id)).count();
    let mut n = Node::new(own, "session", id)
        .prop("lockedUntil", s.and_then(|s| s.lock.as_ref()).map(|l| ms(l.until_ms)))
        .prop("lockedByConnection", s.and_then(|s| s.lock.as_ref()).map(|l| l.owner.conn))
        .prop("lastUpdated", s.map(|s| ms(s.last_updated_ms)))
        .prop("messages", ready);
    if let Some(state) = s.and_then(|s| s.state.as_ref()) {
        n = n.with_content(body_view(state, None));
    }
    n
}

fn topic_tree(
    shared: &Shared,
    ns: &str,
    t: &str,
    topic: &Topic,
    tail: &[&str],
    req: &InspectRequest,
    at: &dyn Fn(&[&str]) -> String,
) -> InspectResult {
    let own = at(&["topics", t]);
    match tail {
        [] => {
            let n = Node::new(&own, "topic", &topic.name);
            let n = settings_props(n, &topic.settings)
                .prop("authorizationRules", rules_view(shared, ns, Some(t)))
                .prop("scheduledForFanOut", topic.scheduled.len())
                .prop("duplicateDetectionIds", topic.dup.len())
                .child("subscriptions", "subscriptions", Some(topic.subs.len() as u64), at(&["topics", t, "subscriptions"]));
            Ok(n.child("scheduled", "messages", Some(topic.scheduled.len() as u64), at(&["topics", t, "scheduled"])))
        }
        ["scheduled"] => Ok(messages_node(
            &topic.scheduled,
            None,
            &at(&["topics", t, "scheduled"]),
            "scheduled messages waiting to be sent to the subscriptions",
            req,
        )),
        ["subscriptions"] => {
            let (rows, next) = req.page_map(&topic.subs);
            let items = rows.into_iter().map(|(k, s)| entity_row(&at(&["topics", t, "subscriptions", k]), &s.name, &s.core)).collect();
            Ok(Node::new(at(&["topics", t, "subscriptions"]), "subscriptions", "subscriptions")
                .prop("count", topic.subs.len())
                .with_items(items, next))
        }
        ["subscriptions", s, rest @ ..] => {
            let sub: &Subscription =
                topic.subs.get(*s).ok_or_else(|| InspectError::not_found(format!("There is no subscription '{s}'.")))?;
            let sub_own = at(&["topics", t, "subscriptions", s]);
            match rest {
                [] => {
                    let mut n =
                        core_tree(shared, ns, &format!("{t}/subscriptions/{s}"), "subscription", &sub.name, &sub.core, &[], req, &sub_own)?;
                    n = n.child("rules", "rules", Some(sub.rules.len() as u64), format!("{sub_own}/rules"));
                    Ok(n)
                }
                ["rules"] => {
                    let (range, next) = req.page_range(sub.rules.len());
                    let items = sub.rules[range].iter().map(rule_row).collect();
                    Ok(Node::new(format!("{sub_own}/rules"), "rules", "rules").prop("count", sub.rules.len()).with_items(items, next))
                }
                _ => core_tree(shared, ns, &format!("{t}/subscriptions/{s}"), "subscription", &sub.name, &sub.core, rest, req, &sub_own),
            }
        }
        _ => Err(missing(req)),
    }
}

fn rule_row(r: &Rule) -> Value {
    let filter = match &r.filter {
        RuleFilter::True => json!({"type": "true"}),
        RuleFilter::False => json!({"type": "false"}),
        RuleFilter::Sql { expression, compatibility_level, .. } => {
            json!({"type": "sql", "expression": expression, "compatibilityLevel": compatibility_level})
        }
        RuleFilter::Correlation(c) => {
            let props: Map<String, Value> = c.properties.iter().map(|(k, v)| (k.clone(), json!(format!("{v:?}")))).collect();
            json!({"type": "correlation", "correlationId": c.correlation_id, "messageId": c.message_id, "to": c.to, "replyTo": c.reply_to, "label": c.label,
                   "sessionId": c.session_id, "replyToSessionId": c.reply_to_session_id, "contentType": c.content_type, "properties": props})
        }
    };
    json!({"name": r.name, "filter": filter, "action": r.action.as_ref().map(|a| a.expression.clone()), "created": ms(r.created_ms)})
}
