//! The small JSON admin API under `/_azemu/eventhubs`: create, read, delete and list event hubs
//! and consumer groups, and per-partition counters.
//!
//! | Request | Result |
//! |---|---|
//! | `GET /_azemu/eventhubs/namespaces` | namespaces and their hub counts |
//! | `GET /_azemu/eventhubs/{ns}/hubs` | every event hub of the namespace |
//! | `PUT /_azemu/eventhubs/{ns}/hubs/{hub}` | create (201) or update (200); body `{"partitionCount":4,"retentionSeconds":86400,"consumerGroups":["a"]}` |
//! | `GET`, `DELETE /_azemu/eventhubs/{ns}/hubs/{hub}` | read with partition counters, delete |
//! | `GET /_azemu/eventhubs/{ns}/hubs/{hub}/partitions[/{n}]` | counters |
//! | `GET /_azemu/eventhubs/{ns}/hubs/{hub}/consumergroups` | consumer groups |
//! | `PUT`, `GET`, `DELETE /_azemu/eventhubs/{ns}/hubs/{hub}/consumergroups/{cg}` | one consumer group |

use azemu_core::resp::Resp;
use azemu_core::time::{iso8601_7, OffsetDateTime};
use azemu_core::{json_error, Request, Response};
use serde_json::{json, Value};

use crate::errors::EhError;
use crate::model::*;
use crate::state::*;

const PREFIX_SEGMENTS: usize = 2; // "_azemu", "eventhubs"

fn iso(ms: i64) -> Value {
    match OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000) {
        Ok(t) => Value::String(iso8601_7(t)),
        Err(_) => Value::Null,
    }
}

fn partition_json(id: usize, p: &Partition) -> Value {
    json!({
        "id": id.to_string(),
        "beginSequenceNumber": p.begin_seq_reported(),
        "lastEnqueuedSequenceNumber": p.last_seq(),
        "lastEnqueuedOffset": p.last_offset_string(),
        "lastEnqueuedTimeUtc": p.last_ms.map(iso).unwrap_or(Value::Null),
        "isEmpty": p.is_empty(),
        "eventCount": p.events.len(),
        "sizeBytes": p.bytes(),
        "totalEventsReceived": p.total_events,
    })
}

fn hub_json(ns: &str, h: &Hub, receivers: usize) -> Value {
    json!({
        "name": h.name,
        "namespace": ns,
        "partitionCount": h.partitions.len(),
        "partitionIds": h.partition_ids(),
        "createdAt": iso(h.created_ms),
        "retentionSeconds": h.retention_ms as f64 / 1000.0,
        "consumerGroups": h.consumer_groups.values().collect::<Vec<_>>(),
        "autoCreated": h.autocreated,
        "receivers": receivers,
        "partitions": h.partitions.iter().enumerate().map(|(i, p)| partition_json(i, p)).collect::<Vec<_>>(),
    })
}

fn error(e: EhError) -> Response {
    let code = match e.status {
        404 => "NotFound",
        409 => "EntityAlreadyExists",
        501 => "NotImplemented",
        _ => "BadRequest",
    };
    json_error::response(e.status as u16, code, &e.description)
}

fn not_found(ns: &str, what: &str) -> Response {
    json_error::response(404, "NotFound", &format!("{what} was not found in namespace '{ns}'."))
}

fn ok(v: Value) -> Response {
    Resp::new(200u16).json(&v).build()
}

fn unbuilt(req: &Request) -> Response {
    json_error::response(501, "NotImplemented", &format!("eventhubs: {} {}", req.method, req.decoded_path()))
}

pub fn handle(shared: &Shared, req: Request) -> Response {
    let writes = !matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS");
    let answer = handle_inner(shared, req);
    if writes {
        // after the work, so that a save that began in between still finds the number moved at its next look
        crate::model::next_revision();
    }
    answer
}

fn handle_inner(shared: &Shared, req: Request) -> Response {
    let all = req.segments();
    let segs: Vec<&str> = all.iter().skip(PREFIX_SEGMENTS).map(String::as_str).collect();
    let method = req.method.as_str();
    match segs.as_slice() {
        ["namespaces"] if method == "GET" => {
            let st = shared.state.lock();
            let list: Vec<Value> = st.namespaces.iter().map(|(n, ns)| json!({"name": n, "eventHubs": ns.hubs.len()})).collect();
            ok(json!({"namespaces": list}))
        }
        [ns, "hubs"] if method == "GET" => {
            let st = shared.state.lock();
            let nsk = st.resolve(ns);
            let hubs: Vec<Value> = st
                .namespaces
                .get(&nsk)
                .map(|n| n.hubs.values().map(|h| hub_json(&nsk, h, st.receiver_count(&nsk, &h.name))).collect())
                .unwrap_or_default();
            ok(json!({"eventHubs": hubs}))
        }
        [ns, "hubs", hub] => hub_route(shared, &req, ns, hub),
        [ns, "hubs", hub, "partitions"] if method == "GET" => {
            let mut st = shared.state.lock();
            let nsk = st.resolve(ns);
            let Some(h) = st.hub_mut(ns, hub) else { return not_found(&nsk, &format!("The event hub '{hub}'")) };
            h.trim_all(now_ms());
            ok(json!({"partitions": h.partitions.iter().enumerate().map(|(i, p)| partition_json(i, p)).collect::<Vec<_>>()}))
        }
        [ns, "hubs", hub, "partitions", pid] if method == "GET" => {
            let mut st = shared.state.lock();
            let nsk = st.resolve(ns);
            let Some(h) = st.hub_mut(ns, hub) else { return not_found(&nsk, &format!("The event hub '{hub}'")) };
            h.trim_all(now_ms());
            match h.parse_pid(pid) {
                Ok(i) => ok(partition_json(i, &h.partitions[i])),
                Err(_) => not_found(&nsk, &format!("The partition '{pid}' of '{hub}'")),
            }
        }
        [ns, "hubs", hub, "consumergroups"] if method == "GET" => {
            let st = shared.state.lock();
            let nsk = st.resolve(ns);
            match st.hub(ns, hub) {
                Some(h) => ok(json!({"consumerGroups": h.consumer_groups.values().collect::<Vec<_>>()})),
                None => not_found(&nsk, &format!("The event hub '{hub}'")),
            }
        }
        [ns, "hubs", hub, "consumergroups", cg] => cg_route(shared, &req, ns, hub, cg),
        _ => unbuilt(&req),
    }
}

fn hub_route(shared: &Shared, req: &Request, ns: &str, hub: &str) -> Response {
    let method = req.method.as_str();
    let mut st = shared.state.lock();
    let nsk = st.resolve(ns);
    match method {
        "GET" => match st.hub_mut(ns, hub) {
            Some(h) => {
                h.trim_all(now_ms());
                let n = st.receiver_count(&nsk, hub);
                let h = st.hub(ns, hub).map(|h| hub_json(&nsk, h, n));
                h.map(ok).unwrap_or_else(|| not_found(&nsk, &format!("The event hub '{hub}'")))
            }
            None => not_found(&nsk, &format!("The event hub '{hub}'")),
        },
        "DELETE" => {
            drop(st);
            if shared.delete_hub(ns, hub) {
                Resp::new(204u16).build()
            } else {
                not_found(&nsk, &format!("The event hub '{hub}'"))
            }
        }
        "PUT" => {
            let body: Value = if req.body.is_empty() {
                json!({})
            } else {
                match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(e) => return json_error::response(400, "BadRequest", &format!("The body is not valid JSON: {e}")),
                }
            };
            let partitions = body.get("partitionCount").and_then(|v| v.as_u64()).map(|p| p as u32);
            let retention_ms =
                match (body.get("retentionSeconds").and_then(|v| v.as_f64()), body.get("retentionDays").and_then(|v| v.as_f64())) {
                    (Some(s), _) => Some((s * 1000.0) as i64),
                    (None, Some(d)) => Some((d * 86_400_000.0) as i64),
                    _ => None,
                };
            let groups: Vec<String> = body
                .get("consumerGroups")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let now = now_ms();
            if let Some(h) = st.hub_mut(ns, hub) {
                if let Some(p) = partitions {
                    if p as usize != h.partitions.len() {
                        return json_error::response(400, "BadRequest", "The partition count of an event hub cannot change.");
                    }
                }
                if let Some(r) = retention_ms.filter(|r| *r > 0) {
                    h.retention_ms = r;
                }
                for g in &groups {
                    h.add_consumer_group(g);
                }
                let n = st.receiver_count(&nsk, hub);
                return st.hub(ns, hub).map(|h| ok(hub_json(&nsk, h, n))).unwrap_or_else(|| unbuilt(req));
            }
            match st.create_hub(ns, hub, partitions.unwrap_or(DEFAULT_PARTITIONS), retention_ms.unwrap_or(DEFAULT_RETENTION_MS), now) {
                Ok(h) => {
                    for g in &groups {
                        h.add_consumer_group(g);
                    }
                    let v = hub_json(&nsk, h, 0);
                    Resp::new(201u16).json(&v).build()
                }
                Err(e) => error(e),
            }
        }
        _ => unbuilt(req),
    }
}

fn cg_route(shared: &Shared, req: &Request, ns: &str, hub: &str, cg: &str) -> Response {
    let mut st = shared.state.lock();
    let nsk = st.resolve(ns);
    let Some(h) = st.hub_mut(ns, hub) else { return not_found(&nsk, &format!("The event hub '{hub}'")) };
    match req.method.as_str() {
        "PUT" => {
            let created = h.add_consumer_group(cg);
            let name = h.consumer_groups.get(&key(cg)).cloned().unwrap_or_else(|| cg.to_string());
            Resp::new(if created { 201u16 } else { 200 }).json(&json!({"name": name, "eventHub": h.name, "namespace": nsk})).build()
        }
        "GET" => match h.consumer_groups.get(&key(cg)) {
            Some(name) => ok(json!({"name": name, "eventHub": h.name, "namespace": nsk})),
            None => not_found(&nsk, &format!("The consumer group '{cg}' of '{hub}'")),
        },
        "DELETE" => {
            if key(cg) == key(DEFAULT_CONSUMER_GROUP) {
                return json_error::response(400, "BadRequest", "The $Default consumer group cannot be deleted.");
            }
            if h.consumer_groups.remove(&key(cg)).is_some() {
                Resp::new(204u16).build()
            } else {
                not_found(&nsk, &format!("The consumer group '{cg}' of '{hub}'"))
            }
        }
        _ => unbuilt(req),
    }
}
