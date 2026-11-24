//! The inspector of Event Grid (`docs/inspector.md`): topics, domains, namespaces and event sources, their subscriptions, and for each
//! subscription the delivery log, the dead letters, the events waiting in a pull queue and the push deliveries waiting for a retry.
//! The same data the admin API under `/_azemu/eventgrid` gives, in the shape of the inspector and paged. Only reads: no event is
//! locked or released, a lock token is never shown, and no counter moves.

use std::collections::BTreeMap;

use azemu_core::inspect::{InspectError, InspectRequest, InspectResult, Node};
use serde_json::{json, Value};

use crate::admin_subs::sub_view;
use crate::model::TopicKind;
use crate::pull::Now;
use crate::state::{Host, State, Sub, Topic};

pub(crate) fn node(st: &State, req: &InspectRequest) -> InspectResult {
    let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
    let [host_key, rest @ ..] = segs.as_slice() else {
        return Ok(service_node(st, req));
    };
    let host = st
        .hosts
        .get(*host_key)
        .ok_or_else(|| InspectError::not_found(format!("There is no topic, domain, namespace or event source '{host_key}'.")))?;
    let now = Now::now();
    match rest {
        [] => Ok(host_node(host_key, host, req)),
        ["subscriptions", tail @ ..] => subs_tree(&[host_key], &host.subs, tail, req, &now),
        ["topics"] => Ok(topics_node(host_key, host, req)),
        ["topics", t, tail @ ..] => {
            let topic = host.topics.get(*t).ok_or_else(|| InspectError::not_found(format!("There is no topic '{t}' in '{host_key}'.")))?;
            match tail {
                [] => Ok(topic_node(host_key, host, topic, req)),
                ["subscriptions", tail @ ..] => subs_tree(&[host_key, "topics", t], &topic.subs, tail, req, &now),
                _ => Err(missing(req)),
            }
        }
        _ => Err(missing(req)),
    }
}

fn missing(req: &InspectRequest) -> InspectError {
    InspectError::not_found(format!("There is no node {}.", req.own_path()))
}

fn kind_name(k: TopicKind) -> &'static str {
    match k {
        TopicKind::Custom => "custom topic",
        TopicKind::Domain => "domain",
        TopicKind::Namespace => "namespace",
        TopicKind::Source => "event source",
    }
}

fn service_node(st: &State, req: &InspectRequest) -> Node {
    let (rows, next) = req.page_map(&st.hosts);
    let items = rows
        .into_iter()
        .map(|(k, h)| json!({"name": h.name, "path": req.path(&[k]), "kind": kind_name(h.kind), "subscriptions": h.subs.len(), "topics": h.topics.len(), "published": h.published}))
        .collect();
    Node::new(req.path(&[]), "service", "eventgrid").prop("accounts", st.hosts.len()).with_items(items, next)
}

fn host_node(key: &str, h: &Host, req: &InspectRequest) -> Node {
    let mut n = Node::new(req.path(&[key]), "host", &h.name)
        .prop("kind", kind_name(h.kind))
        .prop("inputSchema", h.input_schema)
        .prop("published", h.published)
        .prop("resourceId", &h.resource_id)
        // the keys themselves are not shown
        .prop("accessKeys", h.keys.len())
        .child("subscriptions", "subscriptions", Some(h.subs.len() as u64), req.path(&[key, "subscriptions"]));
    if matches!(h.kind, TopicKind::Domain | TopicKind::Namespace) {
        n = n.child("topics", "topics", Some(h.topics.len() as u64), req.path(&[key, "topics"]));
    }
    n
}

fn topics_node(key: &str, h: &Host, req: &InspectRequest) -> Node {
    let (rows, next) = req.page_map(&h.topics);
    let items = rows
        .into_iter()
        .map(|(k, t)| json!({"name": t.name, "path": req.path(&[key, "topics", k]), "subscriptions": t.subs.len(), "published": t.published}))
        .collect();
    Node::new(req.path(&[key, "topics"]), "topics", "topics").prop("count", h.topics.len()).with_items(items, next)
}

fn topic_node(key: &str, h: &Host, t: &Topic, req: &InspectRequest) -> Node {
    Node::new(req.path(&[key, "topics", &t.name.to_ascii_lowercase()]), "topic", &t.name)
        .prop("host", &h.name)
        .prop("inputSchema", t.input_schema)
        .prop("published", t.published)
        .child(
            "subscriptions",
            "subscriptions",
            Some(t.subs.len() as u64),
            req.path(&[key, "topics", &t.name.to_ascii_lowercase(), "subscriptions"]),
        )
}

/// The subscriptions below `at` (the path parts of the topic or host) and what is below one of them.
fn subs_tree(at: &[&str], subs: &BTreeMap<String, Sub>, tail: &[&str], req: &InspectRequest, now: &Now) -> InspectResult {
    let path = |more: &[&str]| {
        let mut p: Vec<&str> = at.to_vec();
        p.push("subscriptions");
        p.extend_from_slice(more);
        req.path(&p)
    };
    match tail {
        [] => {
            let (rows, next) = req.page_map(subs);
            let items = rows
                .into_iter()
                .map(|(k, s)| {
                    let v = sub_view(s, now);
                    json!({"name": s.name, "path": path(&[k]), "mode": v["deliveryMode"], "destination": v["destination"], "validation": s.validation.status,
                           "stats": v["stats"]})
                })
                .collect();
            Ok(Node::new(path(&[]), "subscriptions", "subscriptions").prop("count", subs.len()).with_items(items, next))
        }
        [name, what @ ..] => {
            let s = subs.get(*name).ok_or_else(|| InspectError::not_found(format!("There is no subscription '{name}' here.")))?;
            match what {
                [] => {
                    let mut n = Node::new(path(&[name]), "subscription", &s.name);
                    if let Value::Object(m) = sub_view(s, now) {
                        n.properties = m;
                    }
                    Ok(n.child("deliveries", "deliveries", Some(s.log.len() as u64), path(&[name, "deliveries"]))
                        .child("deadletters", "deadletters", Some(s.queue.dead.len() as u64), path(&[name, "deadletters"]))
                        .child("events", "events", Some(s.queue.queued() as u64), path(&[name, "events"]))
                        .child("pending", "pending", Some(s.pending.len() as u64), path(&[name, "pending"])))
                }
                ["deliveries"] => {
                    let (range, next) = req.page_range(s.log.len());
                    let items = s.log.range(range).filter_map(|a| serde_json::to_value(a).ok()).map(|mut v| {
                        v["name"] = v["eventId"].clone();
                        v
                    });
                    Ok(Node::new(path(&[name, "deliveries"]), "deliveries", "delivery attempts (the last 200)")
                        .with_items(items.collect(), next))
                }
                ["deadletters"] => {
                    let (range, next) = req.page_range(s.queue.dead.len());
                    let items = s.queue.dead[range]
                        .iter()
                        .filter_map(|d| serde_json::to_value(d).ok())
                        .map(|mut v| {
                            v["name"] = v["reason"].clone();
                            v
                        })
                        .collect();
                    Ok(Node::new(path(&[name, "deadletters"]), "deadletters", "dead letters").with_items(items, next))
                }
                ["events"] => {
                    let after = req.marker().and_then(|m| m.parse::<u64>().ok());
                    let mut page = s.queue.page(now, after, req.limit() + 1);
                    let more = page.len() > req.limit();
                    page.truncate(req.limit());
                    let next = if more { page.last().map(|r| azemu_core::inspect::marker_for(&r.0.to_string())) } else { None };
                    let items = page.into_iter().map(|(seq, event, count, state)| json!({"name": event["id"], "sequence": seq, "state": state, "deliveryCount": count, "event": event})).collect();
                    Ok(Node::new(path(&[name, "events"]), "events", "events waiting in the pull queue").with_items(items, next))
                }
                ["pending"] => {
                    let (range, next) = req.page_range(s.pending.len());
                    let items = s
                        .pending
                        .iter()
                        .skip(range.start)
                        .take(range.len())
                        .map(|(id, p)| json!({"name": p.event_id, "id": id, "attempts": p.attempts, "bytes": p.bytes, "startedAt": azemu_core::inspect::unix_time_value(p.started_ms / 1000), "event": &*p.body}))
                        .collect();
                    Ok(Node::new(path(&[name, "pending"]), "pending", "push deliveries waiting for a retry").with_items(items, next))
                }
                _ => Err(missing(req)),
            }
        }
    }
}
