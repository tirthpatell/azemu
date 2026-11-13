//! Error responses as App Configuration sends them.
//!
//! Most failures are `application/problem+json` with `type`, `title`, `name`, `detail`,
//! `status`. A few (404, 412) are an empty body in the recordings and stay empty here.

use azemu_core::resp::Resp;
use azemu_core::Response;
use serde_json::{json, Map, Value};

pub const PROBLEM_CT: &str = "application/problem+json; charset=utf-8";
const ERROR_BASE: &str = "https://azconfig.io/errors";

/// A problem+json response. `name` and `pos` are left out when `None`.
pub fn problem(status: u16, kind: &str, title: &str, name: Option<&str>, detail: Option<&str>, pos: Option<usize>) -> Response {
    let mut m = Map::new();
    m.insert("type".into(), json!(format!("{ERROR_BASE}/{kind}")));
    m.insert("title".into(), json!(title));
    if let Some(n) = name {
        m.insert("name".into(), json!(n));
    }
    if let Some(d) = detail {
        m.insert("detail".into(), json!(d));
    }
    if let Some(p) = pos {
        m.insert("pos".into(), json!(p));
    }
    m.insert("status".into(), json!(status));
    problem_body(status, Value::Object(m))
}

fn problem_body(status: u16, body: Value) -> Response {
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    Resp::new(status).header("Content-Type", PROBLEM_CT).body(bytes).build()
}

/// `400` for a bad parameter, in the form recorded for an invalid label filter.
pub fn invalid_argument(param: &str, detail: &str, pos: Option<usize>) -> Response {
    problem(400, "invalid-argument", &format!("Invalid request parameter '{param}'"), Some(param), Some(detail), pos)
}

/// `400` for a request body that cannot be used.
pub fn invalid_body(detail: &str) -> Response {
    problem(400, "invalid-argument", "Invalid request body", Some("body"), Some(detail), None)
}

/// `409` for a write or delete of a locked (read-only) setting.
pub fn key_locked(key: &str) -> Response {
    problem(
        409,
        "key-locked",
        &format!("Modifing key '{key}' is not allowed"),
        Some(key),
        Some("The key is read-only. To allow modification unlock it first."),
        None,
    )
}

/// `409` for a snapshot name that is taken.
pub fn snapshot_exists(name: &str) -> Response {
    problem(409, "already-exists", "The snapshot already exists", Some(name), Some("A snapshot with the same name already exists."), None)
}

/// `501` naming the operation that is not built.
pub fn not_implemented(method: &str, path: &str) -> Response {
    let resp = problem(
        501,
        "not-implemented",
        &format!("appconfig: {method} {path} is not implemented"),
        None,
        Some("This operation is not built in the emulator."),
        None,
    );
    // for `GET /_azemu/unsupported`: the fixed words of the path, not the names of keys and snapshots
    let shape: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if matches!(s, "kv" | "labels" | "keys" | "revisions" | "snapshots" | "locks" | "operations" | "feature-flags" | "kvset") {
                s
            } else {
                "{}"
            }
        })
        .collect();
    azemu_core::unsupported::mark(resp, format!("AppConfig_Unknown ({method} /{})", shape.join("/")))
}

/// `401` for a request without usable credentials.
pub fn unauthorized() -> Response {
    let body = json!({
        "type": format!("{ERROR_BASE}/unauthorized"),
        "title": "Authentication failed",
        "detail": "The request has no Authorization header. Use HMAC-SHA256 (connection string) or Bearer (Entra ID).",
        "status": 401,
    });
    let mut r = problem_body(401, body);
    r.headers_mut().insert("WWW-Authenticate", azemu_core::http::HeaderValue::from_static("HMAC-SHA256, Bearer"));
    r
}

/// An empty body, as 404 and 412 are sent.
pub fn empty(status: u16) -> Response {
    Resp::new(status).build()
}

pub fn not_found() -> Response {
    empty(404)
}

pub fn precondition_failed() -> Response {
    empty(412)
}

/// `429` when a store's quota is used up, in the form recorded for the request-rate quota (`policy` names which quota).
pub fn quota_exceeded(policy: &str) -> Response {
    let body = json!({
        "type": format!("{ERROR_BASE}/too-many-requests"),
        "title": "Resource utilization has surpassed the assigned quota",
        "policy": policy,
        "status": 429,
    });
    problem_body(429, body)
}
