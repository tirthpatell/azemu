//! JSON and headers of key-values.

use azemu_core::resp::Resp;
use azemu_core::time;
use azemu_core::Response;
use serde_json::{json, Map, Value};

use crate::state::Setting;
use crate::timefmt::iso_secs;

/// Field names of a key-value in wire order (`$select` values).
pub const KV_FIELDS: &[&str] = &["etag", "key", "label", "content_type", "value", "description", "tags", "locked", "last_modified"];

pub const KV_CT: &str = "application/vnd.microsoft.appconfig.kv+json; charset=utf-8";

fn tags_json(tags: &[(String, String)]) -> Value {
    let mut m = Map::new();
    for (k, v) in tags {
        m.insert(k.clone(), json!(v));
    }
    Value::Object(m)
}

/// A key-value as JSON. `select` limits the fields; `description` only exists for newer versions.
pub fn setting_json(s: &Setting, select: Option<&[&str]>, newer: bool) -> Value {
    let mut m = Map::new();
    for f in KV_FIELDS {
        if let Some(sel) = select {
            if !sel.contains(f) {
                continue;
            }
        }
        let v = match *f {
            "etag" => json!(s.etag),
            "key" => json!(s.key),
            "label" => json!(s.label),
            "content_type" => json!(s.content_type),
            "value" => json!(s.value),
            "description" => {
                if !newer {
                    continue;
                }
                json!(s.description)
            }
            "tags" => tags_json(&s.tags),
            "locked" => json!(s.locked),
            _ => json!(iso_secs(s.last_modified)),
        };
        m.insert((*f).to_string(), v);
    }
    Value::Object(m)
}

/// A single key-value response: body, `ETag` (quoted) and `Last-Modified`.
pub fn kv_response(status: u16, s: &Setting, select: Option<&[&str]>, newer: bool) -> Response {
    let body = serde_json::to_vec(&setting_json(s, select, newer)).unwrap_or_default();
    Resp::new(status)
        .header("Content-Type", KV_CT)
        .header("ETag", format!("\"{}\"", s.etag))
        .header("Last-Modified", time::rfc1123(s.last_modified))
        .body(body)
        .build()
}
