//! The inspector of the telemetry sink (`docs/inspector.md`): what was taken in and is still kept, as the sink keeps it. The sink has no
//! accounts, so the service node has one called `sink`. Only reads: the rings are looked at under their lock and nothing is dropped,
//! counted or renumbered.

use std::collections::{BTreeMap, VecDeque};

use azemu_core::inspect::{InspectError, InspectRequest, InspectResult, Node};
use serde_json::{json, Value};

use crate::store::{Entry, LogRow, Telemetry};
use crate::MonitorService;

const ACCOUNT: &str = "sink";

impl MonitorService {
    pub(crate) fn inspect_node(&self, req: &InspectRequest) -> InspectResult {
        let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
        match segs.as_slice() {
            [] => Ok(Node::new(req.path(&[]), "service", "monitor")
                .prop("accounts", 1)
                .prop("note", "The sink keeps telemetry and log rows of every instrumentation key and rule together.")
                .with_items(vec![json!({"name": ACCOUNT, "path": req.path(&[ACCOUNT])})], None)),
            [ACCOUNT] => Ok(self.sink_node(req)),
            [ACCOUNT, "telemetry"] => Ok(self.telemetry_node(req)),
            [ACCOUNT, "logs"] => Ok(self.logs_node(req)),
            _ => Err(InspectError::not_found(format!("There is no node {}.", req.own_path()))),
        }
    }

    fn sink_node(&self, req: &InspectRequest) -> Node {
        let (t, by_key) = self.telemetry.with_items(|items, snap| {
            let mut m: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
            for e in items {
                *m.entry(e.value.ikey.clone()).or_default().entry(e.value.base_type.clone()).or_default() += 1;
            }
            (snap, m)
        });
        let l = self.logs.snapshot();
        Node::new(req.path(&[ACCOUNT]), "sink", ACCOUNT)
            .prop("telemetryStored", t.stored)
            .prop("telemetryDropped", t.dropped)
            .prop("telemetryLastSequence", t.last_sequence)
            .prop("byInstrumentationKey", by_key)
            .prop("logRowsStored", l.stored)
            .prop("logRowsDropped", l.dropped)
            .prop("logRowsLastSequence", l.last_sequence)
            .prop(
                "limits",
                json!({"maxItems": self.limits.max_items, "maxLogRows": self.limits.max_log_rows, "maxBytes": self.limits.max_bytes}),
            )
            .child("telemetry", "telemetry", Some(t.stored as u64), req.path(&[ACCOUNT, "telemetry"]))
            .child("logs", "logs", Some(l.stored as u64), req.path(&[ACCOUNT, "logs"]))
    }

    fn telemetry_node(&self, req: &InspectRequest) -> Node {
        let (ikey, ty, name, op) = (req.param("ikey"), req.param("type"), req.param("name"), req.param("operationId"));
        let (items, next, stored) = self.telemetry.with_items(|items, snap| {
            let rows = from_marker(items, req).filter(|e| {
                let t: &Telemetry = &e.value;
                ikey.is_none_or(|k| t.ikey.eq_ignore_ascii_case(k))
                    && ty.is_none_or(|k| t.base_type == k)
                    && name.is_none_or(|n| t.envelope_name == n || t.names.iter().any(|x| x == n))
                    && op.is_none_or(|o| t.operation_id.as_deref() == Some(o))
            });
            let (page, next) = req.page_keyed(rows.map(|e| (e.seq.to_string(), telemetry_row(e))));
            (page, next, snap.stored)
        });
        Node::new(req.path(&[ACCOUNT, "telemetry"]), "telemetry", "telemetry")
            .accepts(&["ikey", "type", "name", "operationId"])
            .prop("stored", stored)
            .with_items(items, next)
    }

    fn logs_node(&self, req: &InspectRequest) -> Node {
        let (dcr, stream) = (req.param("dcr"), req.param("stream"));
        let (items, next, stored) = self.logs.with_items(|items, snap| {
            let rows =
                from_marker(items, req).filter(|e| dcr.is_none_or(|d| e.value.dcr == d) && stream.is_none_or(|s| e.value.stream == s));
            let (page, next) = req.page_keyed(rows.map(|e| (e.seq.to_string(), log_row(e))));
            (page, next, snap.stored)
        });
        Node::new(req.path(&[ACCOUNT, "logs"]), "logs", "log rows")
            .accepts(&["dcr", "stream"])
            .prop("stored", stored)
            .with_items(items, next)
    }
}

/// The entries after the marker (a sequence number), oldest first. The ring is in sequence order, so the start is found by search.
fn from_marker<'a, T>(items: &'a VecDeque<Entry<T>>, req: &InspectRequest) -> impl Iterator<Item = &'a Entry<T>> + 'a {
    let after: u64 = req.marker().and_then(|m| m.parse().ok()).unwrap_or(0);
    let start = items.partition_point(|e| e.seq <= after);
    items.range(start..)
}

fn telemetry_row(e: &Entry<Telemetry>) -> Value {
    let t = &e.value;
    json!({"name": t.names.first().cloned().unwrap_or_else(|| t.envelope_name.clone()), "sequence": e.seq, "receivedAt": t.received, "iKey": t.ikey,
           "type": t.base_type, "operationId": t.operation_id, "envelope": t.envelope})
}

fn log_row(e: &Entry<LogRow>) -> Value {
    let r = &e.value;
    json!({"name": r.stream, "sequence": e.seq, "receivedAt": r.received, "dcr": r.dcr, "row": r.row})
}
