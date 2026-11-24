//! What the SDKs call besides `track`: the application id lookup and Live Metrics (QuickPulse).
//!
//! Live Metrics is answered as "nobody is watching" (`x-ms-qps-subscribed: false`): the SDK keeps pinging at its own
//! interval and never starts the heavier `post` calls. The answers follow what Azure's Live Metrics service gave in
//! November 2026 (`docs/services/monitor.md`).

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The application id of an instrumentation key. Azure hands out a GUID that has nothing to do with the key; here it is
/// made from the key, so the same key always has the same id.
pub fn app_id(ikey: &str) -> String {
    let digest = Sha256::digest(ikey.to_ascii_lowercase().as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    // a version 4 GUID: random-looking bits, with the version and variant marked
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(b).to_string()
}

/// `GET /api/profiles/{ikey}/appId`: the id as plain text; `404 Profile Not Found` for what is not a key. Azure also
/// answers 404 for a key it does not know; the emulator knows none, so every GUID has an application id.
pub(crate) fn app_id_answer(ikey: &str) -> Response {
    match uuid::Uuid::parse_str(ikey.trim()) {
        Ok(u) => Resp::new(200u16).text(app_id(&u.hyphenated().to_string())).build(),
        Err(_) => Resp::new(404u16).text("Profile Not Found").build(),
    }
}

/// Azure's error body for a Live Metrics call it cannot read.
fn refused(code: &str, message: &str) -> Response {
    let now = azemu_core::time::iso8601_7(time::OffsetDateTime::now_utc());
    Resp::new(400u16)
        .json(&json!({"RequestId": "00000000-0000-0000-0000-000000000000", "ResponseDateTime": now, "Code": code, "Message": message, "Exception": ""}))
        .build()
}

fn not_subscribed() -> Response {
    Resp::new(200u16).header("x-ms-qps-subscribed", "false").header("content-length", "0").build()
}

/// `ping` of Live Metrics, in the old form (`?ikey=`, data in `x-ms-qps-*` headers) and the new one
/// (`?api-version=2024-04-01-preview&ikey=`, data in a JSON body): the same path, so one answer. Azure takes a ping that has
/// either, for any key; one with no key at all is `InvalidInput`.
pub(crate) fn ping(req: &Request) -> Response {
    if req.query.get("ikey").is_none_or(str::is_empty) {
        return refused("InvalidInput", "iKey is garbage or not in a valid GUID format");
    }
    let has_headers = req.headers.keys().any(|k| k.as_str().starts_with("x-ms-qps-"));
    let has_body = serde_json::from_slice::<Value>(&req.body).is_ok();
    if has_headers || has_body {
        not_subscribed()
    } else {
        refused("InvalidOperation", "Failed to parse monitoring data points from the ping request.")
    }
}

/// `post` of Live Metrics: the body has to be JSON.
pub(crate) fn post(req: &Request) -> Response {
    if serde_json::from_slice::<Value>(&req.body).is_ok() {
        not_subscribed()
    } else {
        refused("InvalidOperation", "Failed to parse monitoring data points.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_is_a_stable_guid() {
        let a = app_id("00000000-0000-0000-0000-000000000001");
        assert_eq!(a, app_id("00000000-0000-0000-0000-000000000001"));
        assert_ne!(a, app_id("00000000-0000-0000-0000-000000000002"));
        assert_eq!(uuid::Uuid::parse_str(&a).unwrap().get_version_num(), 4);
        // the case and the form of the key do not matter
        let upper = app_id_answer("ABCDEF00-0000-0000-0000-000000000001");
        assert_eq!(app_id_answer("abcdef00000000000000000000000001").body(), upper.body(), "without dashes");
        assert_eq!(upper.body(), app_id_answer("abcdef00-0000-0000-0000-000000000001").body());
        assert_eq!(app_id_answer("abc").status(), 404);
        assert_eq!(app_id_answer("abc").body().as_ref(), b"Profile Not Found");
    }
}
