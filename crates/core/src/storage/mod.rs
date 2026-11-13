//! Helpers shared by the blob, queue and table services.

pub mod auth;
pub mod cors;
pub mod sig;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use bytes::Bytes;
use http::HeaderValue;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::resp::{IntoStatus, Resp};
use crate::time::iso8601_7;
use crate::{Request, RequestCtx, Response};

/// `x-ms-version` sent when the request carries none.
pub const DEFAULT_VERSION: &str = "2025-11-05";

/// The well-known Azurite / Storage Emulator account name.
pub const DEV_ACCOUNT: &str = "devstoreaccount1";

/// The well-known Azurite key for [`DEV_ACCOUNT`].
pub const DEV_ACCOUNT_KEY: &str = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

/// Account name to keys. Cheap to clone; clones share one cache.
///
/// Keys are derived from the account name, so they are the same after a reset or a
/// restart and no account has to be created first. Every call for a new account name
/// "creates" it.
#[derive(Clone, Default)]
pub struct StorageAccounts {
    cache: Arc<Mutex<HashMap<String, [String; 2]>>>,
}

impl StorageAccounts {
    pub fn new() -> StorageAccounts {
        StorageAccounts::default()
    }

    /// Base64 key (64 bytes decoded) for the account. `which` 2 gives key2, anything
    /// else gives key1. The account name is case-insensitive.
    pub fn key(&self, account: &str, which: u8) -> String {
        let [k1, k2] = self.keys(account);
        if which == 2 {
            k2
        } else {
            k1
        }
    }

    /// Both keys.
    pub fn keys(&self, account: &str) -> [String; 2] {
        let name = account.to_ascii_lowercase();
        let mut cache = self.cache.lock();
        cache.entry(name.clone()).or_insert_with(|| [derive_key(&name, 1), derive_key(&name, 2)]).clone()
    }

    /// Account names seen so far, sorted.
    pub fn accounts(&self) -> Vec<String> {
        let mut v: Vec<String> = self.cache.lock().keys().cloned().collect();
        v.sort();
        v
    }

    /// Forget which accounts were seen. Keys do not change.
    pub fn reset(&self) {
        self.cache.lock().clear();
    }
}

impl std::fmt::Debug for StorageAccounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageAccounts").field("accounts", &self.accounts()).finish()
    }
}

fn derive_key(account: &str, which: u8) -> String {
    if account == DEV_ACCOUNT && which == 1 {
        return DEV_ACCOUNT_KEY.to_string();
    }
    let mut raw = Vec::with_capacity(64);
    for part in 0u8..2 {
        let mut h = Sha256::new();
        h.update(b"azemu-storage-key-v1\0");
        h.update(account.as_bytes());
        h.update([0, which, part]);
        raw.extend_from_slice(&h.finalize());
    }
    B64.encode(raw)
}

/// Escape `& < > " '` for XML text and attribute values. Borrows when nothing changes.
pub fn xml_escape(s: &str) -> Cow<'_, str> {
    if !s.contains(['&', '<', '>', '"', '\'']) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// The XML error body Azure Storage sends, with `x-ms-error-code`.
///
/// ```text
/// <BOM><?xml version="1.0" encoding="utf-8"?><Error><Code>QueueNotFound</Code><Message>The specified queue does not exist.
/// RequestId:6b5d713f-e003-0088-7a50-ea1328000000
/// Time:2022-10-27T22:06:34.6827829Z</Message></Error>
/// ```
pub fn error(ctx: &RequestCtx, status: impl IntoStatus, code: &str, message: &str) -> Response {
    error_with_details(ctx, status, code, message, &[])
}

/// Same, with extra elements after `Message` (for example `("QueueName", "q1")`).
/// Element names are used as given; values are XML-escaped.
pub fn error_with_details(ctx: &RequestCtx, status: impl IntoStatus, code: &str, message: &str, details: &[(&str, &str)]) -> Response {
    let mut body = String::with_capacity(256);
    body.push('\u{feff}');
    body.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>");
    body.push_str(&xml_escape(code));
    body.push_str("</Code><Message>");
    body.push_str(&xml_escape(message));
    body.push_str("\nRequestId:");
    body.push_str(&xml_escape(&ctx.request_id));
    body.push_str("\nTime:");
    body.push_str(&iso8601_7(ctx.time));
    body.push_str("</Message>");
    for (k, v) in details {
        body.push('<');
        body.push_str(k);
        body.push('>');
        body.push_str(&xml_escape(v));
        body.push_str("</");
        body.push_str(k);
        body.push('>');
    }
    body.push_str("</Error>");
    Resp::new(status).xml(Bytes::from(body)).header("x-ms-error-code", code).build()
}

/// Add the headers every storage response carries. Headers the service already set
/// are kept.
///
/// - `x-ms-request-id` from the request context
/// - `x-ms-version`: the request's `x-ms-version`, else [`DEFAULT_VERSION`]
/// - `x-ms-client-request-id`: echoed when the request has it
/// - `Server`: `server`, for example `Windows-Azure-Blob/1.0 Microsoft-HTTPAPI/2.0`
pub fn finish(mut resp: Response, req: &Request, server: &str) -> Response {
    let h = resp.headers_mut();
    if !h.contains_key("x-ms-request-id") {
        if let Ok(v) = HeaderValue::from_str(&req.ctx.request_id) {
            h.insert("x-ms-request-id", v);
        }
    }
    if !h.contains_key("x-ms-version") {
        let v = req.headers.get("x-ms-version").cloned().unwrap_or_else(|| HeaderValue::from_static(DEFAULT_VERSION));
        h.insert("x-ms-version", v);
    }
    if !h.contains_key("x-ms-client-request-id") {
        if let Some(v) = req.headers.get("x-ms-client-request-id") {
            h.insert("x-ms-client-request-id", v.clone());
        }
    }
    if !h.contains_key(http::header::SERVER) {
        if let Ok(v) = HeaderValue::from_str(server) {
            h.insert(http::header::SERVER, v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Endpoint, Query};
    use ::time::macros::datetime;

    fn ctx() -> RequestCtx {
        RequestCtx {
            request_id: "6b5d713f-e003-0088-7a50-ea1328000000".into(),
            time: datetime!(2022-10-27 22:06:34.6827829 UTC),
            endpoint: Endpoint { scheme: "http", authority: "a.queue.localhost:1".into(), path_style: false, account: "a".into() },
        }
    }

    fn req(headers: &[(&str, &str)]) -> Request {
        let mut h = http::HeaderMap::new();
        for (k, v) in headers {
            h.insert(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        Request {
            method: http::Method::GET,
            path: "/".into(),
            query: Query::default(),
            headers: h,
            body: Bytes::new(),
            account: "a".into(),
            ctx: ctx(),
        }
    }

    #[test]
    fn error_body_is_byte_exact() {
        let r = error(&ctx(), 404, "QueueNotFound", "The specified queue does not exist.");
        let expected = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>QueueNotFound</Code><Message>The specified queue does not exist.\nRequestId:6b5d713f-e003-0088-7a50-ea1328000000\nTime:2022-10-27T22:06:34.6827829Z</Message></Error>";
        assert_eq!(r.body().as_ref(), expected.as_bytes());
        assert_eq!(&r.body()[..3], &[0xEF, 0xBB, 0xBF]);
        assert_eq!(r.status(), 404);
        assert_eq!(r.headers()["content-type"], "application/xml");
        assert_eq!(r.headers()["x-ms-error-code"], "QueueNotFound");
    }

    #[test]
    fn error_matches_recording_shape() {
        // recording: Content-Length 217 for this exact message with a 36 char request id
        let r = error(&ctx(), 404, "QueueNotFound", "The specified queue does not exist.");
        assert_eq!(r.body().len(), 217);
    }

    #[test]
    fn error_details_and_escaping() {
        let r = error_with_details(&ctx(), 409, "Bad<Code>", "a & b \"c\" <d>", &[("QueueName", "q&1"), ("Other", "x")]);
        let s = std::str::from_utf8(&r.body()[3..]).unwrap();
        assert!(s.contains("<Code>Bad&lt;Code&gt;</Code>"));
        assert!(s.contains("<Message>a &amp; b &quot;c&quot; &lt;d&gt;\nRequestId:"));
        assert!(s.ends_with("</Message><QueueName>q&amp;1</QueueName><Other>x</Other></Error>"));
    }

    #[test]
    fn xml_escape_borrows_when_clean() {
        assert!(matches!(xml_escape("plain text"), Cow::Borrowed(_)));
        assert_eq!(xml_escape("<&>\"'"), "&lt;&amp;&gt;&quot;&apos;");
    }

    #[test]
    fn dev_account_key_is_azurite() {
        let a = StorageAccounts::new();
        assert_eq!(a.key("devstoreaccount1", 1), DEV_ACCOUNT_KEY);
        assert_eq!(a.key("DevStoreAccount1", 1), DEV_ACCOUNT_KEY);
        assert_ne!(a.key("devstoreaccount1", 2), DEV_ACCOUNT_KEY);
        assert_eq!(B64.decode(DEV_ACCOUNT_KEY).unwrap().len(), 64);
    }

    #[test]
    fn derived_keys_are_stable_and_distinct() {
        let a = StorageAccounts::new();
        let k = a.key("acct", 1);
        assert_eq!(B64.decode(&k).unwrap().len(), 64);
        assert_eq!(B64.decode(a.key("acct", 2)).unwrap().len(), 64);
        assert_ne!(k, a.key("acct", 2));
        assert_ne!(k, a.key("acct2", 1));
        // survives reset and fresh handles
        a.reset();
        assert_eq!(a.key("acct", 1), k);
        assert_eq!(StorageAccounts::new().key("ACCT", 1), k);
        // clones share the cache
        let b = a.clone();
        b.key("zzz", 1);
        assert_eq!(a.accounts(), vec!["acct", "zzz"]);
    }

    #[test]
    fn finish_adds_headers_without_overwriting() {
        let r = finish(
            Resp::new(200u16).build(),
            &req(&[("x-ms-version", "2020-02-10"), ("x-ms-client-request-id", "cid")]),
            "Windows-Azure-Blob/1.0 Microsoft-HTTPAPI/2.0",
        );
        assert_eq!(r.headers()["x-ms-request-id"], "6b5d713f-e003-0088-7a50-ea1328000000");
        assert_eq!(r.headers()["x-ms-version"], "2020-02-10");
        assert_eq!(r.headers()["x-ms-client-request-id"], "cid");
        assert_eq!(r.headers()["server"], "Windows-Azure-Blob/1.0 Microsoft-HTTPAPI/2.0");

        let r = finish(Resp::new(200u16).build(), &req(&[]), "S");
        assert_eq!(r.headers()["x-ms-version"], DEFAULT_VERSION);
        assert!(r.headers().get("x-ms-client-request-id").is_none());

        let pre = Resp::new(200u16).header("x-ms-request-id", "mine").header("x-ms-version", "v").header("server", "svc").build();
        let r = finish(pre, &req(&[("x-ms-version", "other")]), "S");
        assert_eq!(r.headers()["x-ms-request-id"], "mine");
        assert_eq!(r.headers()["x-ms-version"], "v");
        assert_eq!(r.headers()["server"], "svc");
    }
}
