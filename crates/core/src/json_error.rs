//! `{"error":{"code":…,"message":…}}` bodies (Key Vault, App Configuration, ...).

use serde_json::{json, Value};

use crate::resp::{IntoStatus, Resp};
use crate::Response;

/// `{"error":{"code":"…","message":"…"}}`, `Content-Type: application/json; charset=utf-8`.
pub fn response(status: impl IntoStatus, code: &str, message: &str) -> Response {
    Resp::new(status).json(&body(code, message, None)).build()
}

/// Same, with `"innererror":{"code":"…"}` (Key Vault style).
pub fn response_with_inner(status: impl IntoStatus, code: &str, message: &str, inner_code: &str) -> Response {
    Resp::new(status).json(&body(code, message, Some(inner_code))).build()
}

/// The body as a JSON value, for services that need to add headers or fields.
pub fn body(code: &str, message: &str, inner_code: Option<&str>) -> Value {
    let mut err = json!({ "code": code, "message": message });
    if let Some(inner) = inner_code {
        err["innererror"] = json!({ "code": inner });
    }
    json!({ "error": err })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain() {
        let r = response(404, "SecretNotFound", "A secret with name \"x\" was not found");
        assert_eq!(r.status(), 404);
        assert_eq!(r.headers()["content-type"], "application/json; charset=utf-8");
        let v: Value = serde_json::from_slice(r.body()).unwrap();
        assert_eq!(v["error"]["code"], "SecretNotFound");
        assert_eq!(v["error"]["message"], "A secret with name \"x\" was not found");
        assert!(v["error"].get("innererror").is_none());
    }

    #[test]
    fn with_inner() {
        let r = response_with_inner(400, "BadParameter", "m", "InvalidName");
        let v: Value = serde_json::from_slice(r.body()).unwrap();
        assert_eq!(v["error"]["innererror"]["code"], "InvalidName");
    }

    #[test]
    fn exact_bytes() {
        let r = response(501, "NotImplemented", "blob: Foo");
        assert_eq!(r.body().as_ref(), br#"{"error":{"code":"NotImplemented","message":"blob: Foo"}}"#);
    }
}
