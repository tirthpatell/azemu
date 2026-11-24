//! The inspector of Event Hubs and its Kafka endpoint (`docs/inspector.md`): namespaces, event hubs, partitions with their sequence
//! numbers and offsets, the events of a partition from a sequence number on, consumer groups, the offsets Kafka consumers committed,
//! and the Schema Registry's groups and schemas. Only reads: no receiver is registered, no retention is applied on the way (events
//! past their retention are shown as held until the next operation drops them), no round-robin counter moves, and a namespace that
//! does not exist is a 404 and is not made. The Kafka groups' members are not shown: asking for them makes the coordinator expire
//! members, which is a change. AMQP consumers keep their checkpoints in a blob container of their own, not here.

use azemu_amqp::inspect::message_json;
use azemu_core::inspect::{marker_for, rfc3339, InspectError, InspectRequest, InspectResult, Node, BODY_LIMIT};
use azemu_core::time::OffsetDateTime;
use serde_json::{json, Map, Value};

use crate::model::{Hub, Partition, NEVER_MS};
use crate::schema_registry::SchemaGroup;
use crate::state::{Namespace, State};
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

/// Committed offsets of Kafka consumers: (namespace, group, [(topic, partition, offset, metadata, leader epoch)]).
type Committed = Vec<(String, String, Vec<(String, i32, crate::kafka::groups::Committed)>)>;

pub(crate) fn node(shared: &Shared, req: &InspectRequest) -> InspectResult {
    let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
    // the coordinator has its own lock: read it before the state's, never inside it
    let needs_kafka =
        segs.len() == 1 || segs.get(1).is_some_and(|s| *s == "kafka-groups") || segs.get(3).is_some_and(|s| *s == "partitions");
    let kafka: Committed = if needs_kafka { shared.kafka.export_offsets() } else { Vec::new() };
    let state = shared.state.lock();
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
        [] => Ok(namespace_node(shared, ns_name, ns, &kafka_groups_of(&kafka, ns_name), req)),
        ["hubs"] => {
            let (rows, next) = req.page_map(&ns.hubs);
            let items = rows.into_iter().map(|(k, h)| hub_row(&at(&["hubs", k]), h)).collect();
            Ok(Node::new(at(&["hubs"]), "hubs", "event hubs").prop("count", ns.hubs.len()).with_items(items, next))
        }
        ["hubs", h, tail @ ..] => {
            let hub = ns.hubs.get(*h).ok_or_else(|| InspectError::not_found(format!("There is no event hub '{h}'.")))?;
            hub_tree(shared, ns_name, h, hub, tail, req, &kafka, &at)
        }
        ["kafka-groups"] => {
            let groups = kafka_groups_of(&kafka, ns_name);
            let (range, next) = req.page_range(groups.len());
            let items =
                groups[range].iter().map(|(g, n)| json!({"name": g, "path": at(&["kafka-groups", g]), "committedOffsets": n})).collect();
            Ok(Node::new(at(&["kafka-groups"]), "kafka-groups", "Kafka consumer groups with committed offsets")
                .prop("count", groups.len())
                .with_items(items, next))
        }
        ["kafka-groups", g] => {
            let rows: Vec<Value> = kafka
                .iter()
                .filter(|(n, id, _)| n.eq_ignore_ascii_case(&state.resolve(ns_name)) && id == g)
                .flat_map(|(_, _, o)| o.iter())
                .map(|(t, p, c)| json!({"name": format!("{t}-{p}"), "topic": t, "partition": p, "offset": c.offset, "metadata": c.metadata, "leaderEpoch": c.leader_epoch}))
                .collect();
            if rows.is_empty() {
                return Err(InspectError::not_found(format!("There is no Kafka consumer group '{g}' with committed offsets.")));
            }
            let (range, next) = req.page_range(rows.len());
            Ok(Node::new(at(&["kafka-groups", g]), "kafka-group", *g)
                .prop("committedOffsets", rows.len())
                .with_items(rows[range].to_vec(), next))
        }
        ["schemagroups"] => {
            let (rows, next) = req.page_map(&ns.schema_groups);
            let items = rows
                .into_iter()
                .map(|(k, g)| json!({"name": g.name, "path": at(&["schemagroups", k]), "schemaType": g.schema_type.name(), "compatibility": g.compatibility.name(), "schemas": g.schemas.len()}))
                .collect();
            Ok(Node::new(at(&["schemagroups"]), "schemagroups", "schema groups")
                .prop("count", ns.schema_groups.len())
                .with_items(items, next))
        }
        ["schemagroups", g, tail @ ..] => {
            let group = ns.schema_groups.get(*g).ok_or_else(|| InspectError::not_found(format!("There is no schema group '{g}'.")))?;
            schema_tree(g, group, tail, req, &at)
        }
        _ => Err(missing(req)),
    }
}

fn kafka_groups_of(all: &Committed, ns: &str) -> Vec<(String, usize)> {
    all.iter().filter(|(n, _, _)| n.eq_ignore_ascii_case(ns)).map(|(_, g, o)| (g.clone(), o.len())).collect()
}

fn service_node(state: &State, req: &InspectRequest) -> Node {
    let (rows, next) = req.page_map(&state.namespaces);
    let items = rows
        .into_iter()
        .map(|(k, n)| json!({"name": k, "path": req.path(&[k]), "hubs": n.hubs.len(), "schemaGroups": n.schema_groups.len()}))
        .collect();
    Node::new(req.path(&[]), "service", "eventhubs")
        .prop("accounts", state.namespaces.len())
        .prop("note", "The Kafka endpoint shares these namespaces: a Kafka topic is an event hub, and the committed offsets of Kafka consumer groups are under each namespace.")
        .with_items(items, next)
}

fn namespace_node(shared: &Shared, name: &str, ns: &Namespace, kafka: &[(String, usize)], req: &InspectRequest) -> Node {
    let rules: Vec<Value> = shared.sas.at(name, None).iter().map(|r| json!({"name": r.name, "rights": r.rights.names()})).collect();
    Node::new(req.path(&[name]), "namespace", name)
        .prop("authorizationRules", rules)
        .child("hubs", "hubs", Some(ns.hubs.len() as u64), req.path(&[name, "hubs"]))
        .child("kafka-groups", "kafka-groups", Some(kafka.len() as u64), req.path(&[name, "kafka-groups"]))
        .child("schemagroups", "schemagroups", Some(ns.schema_groups.len() as u64), req.path(&[name, "schemagroups"]))
}

fn hub_row(path: &str, h: &Hub) -> Value {
    json!({"name": h.name, "path": path, "partitions": h.partitions.len(), "retentionMs": h.retention_ms, "consumerGroups": h.consumer_groups.len(),
           "retainedEvents": h.partitions.iter().map(|p| p.events.len()).sum::<usize>(), "autocreated": h.autocreated})
}

#[allow(clippy::too_many_arguments)]
fn hub_tree(
    shared: &Shared,
    ns: &str,
    h: &str,
    hub: &Hub,
    tail: &[&str],
    req: &InspectRequest,
    kafka: &Committed,
    at: &dyn Fn(&[&str]) -> String,
) -> InspectResult {
    let own = at(&["hubs", h]);
    match tail {
        [] => {
            let rules: Vec<Value> =
                shared.sas.at(ns, Some(h)).iter().map(|r| json!({"name": r.name, "rights": r.rights.names()})).collect();
            Ok(Node::new(&own, "hub", &hub.name)
                .prop("id", hub.id)
                .prop("createdAt", ms(hub.created_ms))
                .prop("partitionCount", hub.partitions.len())
                .prop("retentionMs", hub.retention_ms)
                .prop("consumerGroups", hub.consumer_groups.values().collect::<Vec<_>>())
                .prop("autocreated", hub.autocreated)
                .prop("authorizationRules", rules)
                .prop("totalEventsEverStored", hub.partitions.iter().map(|p| p.total_events).sum::<u64>())
                .child("partitions", "partitions", Some(hub.partitions.len() as u64), at(&["hubs", h, "partitions"])))
        }
        ["partitions"] => {
            let (range, next) = req.page_range(hub.partitions.len());
            let items =
                range.clone().map(|i| partition_row(&at(&["hubs", h, "partitions", &i.to_string()]), i, &hub.partitions[i])).collect();
            Ok(Node::new(at(&["hubs", h, "partitions"]), "partitions", "partitions")
                .prop("count", hub.partitions.len())
                .with_items(items, next))
        }
        ["partitions", pid] => {
            let i: usize = pid
                .parse()
                .ok()
                .filter(|i| *i < hub.partitions.len())
                .ok_or_else(|| InspectError::not_found(format!("There is no partition '{pid}'.")))?;
            Ok(partition_node(&at(&["hubs", h, "partitions", pid]), &hub.name, i, &hub.partitions[i], hub.retention_ms, kafka, ns, req))
        }
        _ => Err(missing(req)),
    }
}

fn partition_row(path: &str, id: usize, p: &Partition) -> Value {
    json!({
        "name": id,
        "path": path,
        "beginSequenceNumber": p.begin_seq_reported(),
        "lastSequenceNumber": p.last_seq(),
        "lastOffset": p.last_offset_string(),
        "nextOffset": p.next_offset,
        "lastEnqueuedTime": if p.last_time_reported() == NEVER_MS { Value::Null } else { ms(p.last_time_reported()) },
        "retainedEvents": p.events.len(),
        "retainedBytes": p.bytes(),
        "totalEvents": p.total_events,
    })
}

/// One partition: its numbers, and the events from `?from=<sequence number>` (or the marker, or the first retained) on.
#[allow(clippy::too_many_arguments)]
fn partition_node(
    own: &str,
    hub_name: &str,
    id: usize,
    p: &Partition,
    retention_ms: i64,
    kafka: &Committed,
    ns: &str,
    req: &InspectRequest,
) -> Node {
    let from = match req.marker().and_then(|m| m.parse::<i64>().ok()) {
        Some(m) => m,
        None => req.param("from").and_then(|f| f.parse::<i64>().ok()).unwrap_or_else(|| p.first_seq()),
    };
    let first = p.first_seq();
    let start = (from.max(first) - first).max(0) as usize;
    let end = (start + req.limit()).min(p.events.len());
    let items: Vec<Value> = p.events.range(start.min(p.events.len())..end).map(|e| event_row(e)).collect();
    let next = (end < p.events.len()).then(|| marker_for(&(first + end as i64).to_string()));
    let now = crate::state::now_ms();
    let past_retention = p.events.iter().take_while(|e| e.enqueued_ms.saturating_add(retention_ms) <= now).count();
    let committed: Vec<Value> = kafka
        .iter()
        .filter(|(n, _, _)| n.eq_ignore_ascii_case(ns))
        .flat_map(|(_, g, o)| {
            o.iter()
                .filter(|(t, part, _)| t.eq_ignore_ascii_case(hub_name) && *part as usize == id)
                .map(move |(_, _, c)| json!({"group": g, "offset": c.offset}))
        })
        .collect();
    let mut row = partition_row(own, id, p);
    if let Value::Object(m) = &mut row {
        m.remove("path");
        m.remove("name");
    }
    let mut n = Node::new(own, "partition", format!("{hub_name}~{id}")).accepts(&["from"]);
    if let Value::Object(m) = row {
        for (k, v) in m {
            n = n.prop(&k, v);
        }
    }
    n.prop("eventsPastRetention", past_retention).prop("kafkaCommittedOffsets", committed).with_items(items, next)
}

fn event_row(e: &crate::model::Event) -> Value {
    let mut row = Map::new();
    row.insert("name".into(), json!(e.seq));
    row.insert("sequenceNumber".into(), json!(e.seq));
    row.insert("offset".into(), json!(e.offset.to_string()));
    row.insert("enqueuedTime".into(), ms(e.enqueued_ms));
    row.insert("partitionKey".into(), json!(e.partition_key));
    row.insert("sizeInBytes".into(), json!(e.size));
    row.insert("writtenThroughKafka".into(), json!(e.kafka));
    row.extend(message_json(&e.message));
    Value::Object(row)
}

fn schema_tree(g: &str, group: &SchemaGroup, tail: &[&str], req: &InspectRequest, at: &dyn Fn(&[&str]) -> String) -> InspectResult {
    let own = at(&["schemagroups", g]);
    match tail {
        [] => {
            let props: Map<String, Value> = group.properties.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            Ok(Node::new(&own, "schemagroup", &group.name)
                .prop("schemaType", group.schema_type.name())
                .prop("compatibility", group.compatibility.name())
                .prop("properties", props)
                .prop("createdAt", ms(group.created_ms))
                .prop("updatedAt", ms(group.updated_ms))
                .prop("etag", &group.etag)
                .child("schemas", "schemas", Some(group.schemas.len() as u64), at(&["schemagroups", g, "schemas"])))
        }
        ["schemas"] => {
            let (rows, next) = req.page_map(&group.schemas);
            let items = rows
                .into_iter()
                .map(|(k, s)| json!({"name": s.name, "path": at(&["schemagroups", g, "schemas", k]), "versions": s.versions.len(), "latestVersion": s.versions.last().map(|v| v.version)}))
                .collect();
            Ok(Node::new(at(&["schemagroups", g, "schemas"]), "schemas", "schemas")
                .prop("count", group.schemas.len())
                .with_items(items, next))
        }
        ["schemas", name] => {
            let s = group.schemas.get(*name).ok_or_else(|| InspectError::not_found(format!("There is no schema '{name}'.")))?;
            let (range, next) = req.page_range(s.versions.len());
            let items = s.versions[range]
                .iter()
                .map(|v| {
                    let shown: String = v.content.chars().take(BODY_LIMIT).collect();
                    json!({"name": v.version, "version": v.version, "id": v.id, "type": v.kind.name(), "createdAt": ms(v.created_ms), "content": shown, "size": v.content.len()})
                })
                .collect();
            Ok(Node::new(at(&["schemagroups", g, "schemas", name]), "schema", &s.name)
                .prop("versions", s.versions.len())
                .with_items(items, next))
        }
        _ => Err(missing(req)),
    }
}
