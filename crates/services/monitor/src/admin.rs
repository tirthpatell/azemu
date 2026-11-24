//! The admin API, `/_azemu/monitor/...`: read back what the sink stored, and empty it.

use std::collections::BTreeMap;

use azemu_core::http::Method;
use azemu_core::resp::Resp;
use azemu_core::{json_error, Request, Response};
use serde_json::{json, Value};

use crate::store::{Entry, LogRow, Snapshot, Telemetry};
use crate::MonitorService;

fn bad_parameter(message: String) -> Response {
    json_error::response(400, "InvalidParameter", &message)
}

/// The query parameters of a read: only the known ones, and a number where a number is expected.
struct Filters<'a> {
    req: &'a Request,
}

impl<'a> Filters<'a> {
    fn new(req: &'a Request, known: &[&str]) -> Result<Filters<'a>, Response> {
        for (k, _) in req.query.pairs() {
            if !known.iter().any(|n| n.eq_ignore_ascii_case(k)) {
                return Err(bad_parameter(format!("Unknown parameter '{k}'. Known: {}.", known.join(", "))));
            }
        }
        Ok(Filters { req })
    }

    fn text(&self, name: &str) -> Option<&'a str> {
        self.req.query.get(name).filter(|v| !v.is_empty())
    }

    fn number(&self, name: &str) -> Result<Option<u64>, Response> {
        match self.req.query.get(name) {
            None | Some("") => Ok(None),
            Some(v) => v
                .trim()
                .parse::<u64>()
                .map(Some)
                .map_err(|_| bad_parameter(format!("The parameter '{name}' must be a whole number, not '{v}'."))),
        }
    }
}

fn read_answer(items: Vec<Value>, snap: Snapshot, truncated: bool) -> Response {
    Resp::new(200u16)
        .json(&json!({
            "count": items.len(),
            "stored": snap.stored,
            "dropped": snap.dropped,
            "lastSequence": snap.last_sequence,
            "truncated": truncated,
            "items": items,
        }))
        .build()
}

fn telemetry_json(e: &Entry<Telemetry>) -> Value {
    let t = &e.value;
    let mut v = json!({"sequence": e.seq, "receivedAt": t.received, "iKey": t.ikey, "type": t.base_type, "envelope": t.envelope});
    if let Some(n) = t.names.first() {
        v["name"] = json!(n);
    }
    if let Some(o) = &t.operation_id {
        v["operationId"] = json!(o);
    }
    v
}

fn log_json(e: &Entry<LogRow>) -> Value {
    let r = &e.value;
    json!({"sequence": e.seq, "receivedAt": r.received, "dcr": r.dcr, "stream": r.stream, "row": r.row})
}

impl MonitorService {
    pub(crate) fn admin(&self, req: &Request, rest: &[&str]) -> Response {
        match rest {
            [] => self.only(req, &[Method::GET], || self.summary()),
            ["telemetry"] => match req.method {
                Method::DELETE => self.clear_telemetry(),
                _ => self.only(req, &[Method::GET], || self.read_telemetry(req)),
            },
            ["logs"] => match req.method {
                Method::DELETE => self.clear_logs(),
                _ => self.only(req, &[Method::GET], || self.read_logs(req)),
            },
            _ => {
                let r = json_error::response(
                    404,
                    "NotFound",
                    &format!("Unknown monitor admin path '{}'. GET /_azemu/monitor/ shows what there is.", req.decoded_path()),
                );
                r
            }
        }
    }

    fn only(&self, req: &Request, allowed: &[Method], f: impl FnOnce() -> Response) -> Response {
        if allowed.contains(&req.method) {
            return f();
        }
        let list = allowed.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ");
        let mut r = json_error::response(
            405,
            "MethodNotAllowed",
            &format!("{} {} is not an admin operation (use {list}).", req.method, req.decoded_path()),
        );
        if let Ok(v) = azemu_core::http::HeaderValue::from_str(&list) {
            r.headers_mut().insert("allow", v);
        }
        r
    }

    fn summary(&self) -> Response {
        let (t_snap, by_key) = self.telemetry.with_items(|items, snap| {
            let mut by_key: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
            for e in items {
                *by_key.entry(e.value.ikey.clone()).or_default().entry(e.value.base_type.clone()).or_default() += 1;
            }
            (snap, by_key)
        });
        let (l_snap, by_stream) = self.logs.with_items(|items, snap| {
            let mut by_stream: BTreeMap<(String, String), u64> = BTreeMap::new();
            for e in items {
                *by_stream.entry((e.value.dcr.clone(), e.value.stream.clone())).or_default() += 1;
            }
            (snap, by_stream)
        });
        let streams: Vec<Value> =
            by_stream.into_iter().map(|((dcr, stream), n)| json!({"dcr": dcr, "stream": stream, "count": n})).collect();
        Resp::new(200u16)
            .json(&json!({
                "telemetry": {
                    "stored": t_snap.stored, "dropped": t_snap.dropped, "lastSequence": t_snap.last_sequence,
                    "byInstrumentationKey": by_key,
                },
                "logs": {
                    "stored": l_snap.stored, "dropped": l_snap.dropped, "lastSequence": l_snap.last_sequence,
                    "byStream": streams,
                },
                "limits": {"maxItems": self.limits.max_items, "maxLogRows": self.limits.max_log_rows, "maxBytes": self.limits.max_bytes},
            }))
            .build()
    }

    fn read_telemetry(&self, req: &Request) -> Response {
        let f = match Filters::new(req, &["ikey", "type", "name", "operationId", "since", "limit"]) {
            Ok(f) => f,
            Err(r) => return r,
        };
        let (since, limit) = match (f.number("since"), f.number("limit")) {
            (Ok(s), Ok(l)) => (s.unwrap_or(0), l),
            (Err(r), _) | (_, Err(r)) => return r,
        };
        // the key in any GUID form, as the items are kept under the usual one
        let ikey = f
            .text("ikey")
            .map(|k| uuid::Uuid::parse_str(k.trim()).map(|u| u.hyphenated().to_string()).unwrap_or_else(|_| k.to_ascii_lowercase()));
        let (ty, name, op) = (f.text("type"), f.text("name"), f.text("operationId"));
        let (items, snap, truncated) = self.telemetry.with_items(|items, snap| {
            let mut out = Vec::new();
            let mut truncated = false;
            for e in items.iter().filter(|e| e.seq > since) {
                let t = &e.value;
                let matches = ikey.as_ref().is_none_or(|k| &t.ikey == k)
                    && ty.is_none_or(|k| t.base_type == k)
                    && name.is_none_or(|n| t.envelope_name == n || t.names.iter().any(|x| x == n))
                    && op.is_none_or(|o| t.operation_id.as_deref() == Some(o));
                if !matches {
                    continue;
                }
                if limit.is_some_and(|l| out.len() as u64 >= l) {
                    truncated = true;
                    break;
                }
                out.push(telemetry_json(e));
            }
            (out, snap, truncated)
        });
        read_answer(items, snap, truncated)
    }

    fn read_logs(&self, req: &Request) -> Response {
        let f = match Filters::new(req, &["dcr", "stream", "since", "limit"]) {
            Ok(f) => f,
            Err(r) => return r,
        };
        let (since, limit) = match (f.number("since"), f.number("limit")) {
            (Ok(s), Ok(l)) => (s.unwrap_or(0), l),
            (Err(r), _) | (_, Err(r)) => return r,
        };
        let (dcr, stream) = (f.text("dcr"), f.text("stream"));
        let (items, snap, truncated) = self.logs.with_items(|items, snap| {
            let mut out = Vec::new();
            let mut truncated = false;
            for e in items.iter().filter(|e| e.seq > since) {
                if !(dcr.is_none_or(|d| e.value.dcr == d) && stream.is_none_or(|s| e.value.stream == s)) {
                    continue;
                }
                if limit.is_some_and(|l| out.len() as u64 >= l) {
                    truncated = true;
                    break;
                }
                out.push(log_json(e));
            }
            (out, snap, truncated)
        });
        read_answer(items, snap, truncated)
    }

    fn clear_telemetry(&self) -> Response {
        let n = self.telemetry.snapshot().stored;
        self.telemetry.clear();
        Resp::new(200u16).json(&json!({"deleted": n})).build()
    }

    fn clear_logs(&self) -> Response {
        let n = self.logs.snapshot().stored;
        self.logs.clear();
        Resp::new(200u16).json(&json!({"deleted": n})).build()
    }
}
