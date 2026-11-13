#![allow(dead_code)]

use std::sync::Arc;

use azemu_core::testing::{TestResponse, TestServer};
use azemu_core::time::{iso8601_secs, Duration, OffsetDateTime};
use azemu_queue::QueueService;

pub const ACCOUNT: &str = "acct1";

/// A queue service behind a gateway, and helpers to talk to it with a shared key.
pub struct Q {
    pub srv: TestServer,
    pub base: String,
}

pub async fn start() -> Q {
    let srv = TestServer::start(vec![Arc::new(QueueService::new())]).await;
    let base = srv.url("queue", ACCOUNT);
    Q { srv, base }
}

impl Q {
    /// Request with a shared key header (never checked).
    pub async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> TestResponse {
        let mut h: Vec<(&str, &str)> = vec![("Authorization", "SharedKey acct1:c2ln")];
        h.extend_from_slice(headers);
        self.srv.send(method, &format!("{}{}", self.base, path), &h, body.to_string()).await
    }

    /// Request with no credentials at all.
    pub async fn anon(&self, method: &str, path: &str, body: &str) -> TestResponse {
        self.srv.send(method, &format!("{}{}", self.base, path), &[], body.to_string()).await
    }

    /// Request with no credentials and the given headers.
    pub async fn anon_with(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> TestResponse {
        self.srv.send(method, &format!("{}{}", self.base, path), headers, String::new()).await
    }

    /// Request authenticated by a SAS token appended to the query.
    pub async fn with_sas(&self, method: &str, path: &str, sas: &str, body: &str) -> TestResponse {
        let sep = if path.contains('?') { '&' } else { '?' };
        self.srv.send(method, &format!("{}{}{}{}", self.base, path, sep, sas), &[], body.to_string()).await
    }

    pub async fn create(&self, queue: &str) {
        let r = self.send("PUT", &format!("/{queue}"), &[], "").await;
        assert_eq!(r.status, 201, "create {queue}: {}", r.text());
    }

    /// Put a message; returns the response.
    pub async fn put(&self, queue: &str, text: &str, query: &str) -> TestResponse {
        let q = if query.is_empty() { String::new() } else { format!("?{query}") };
        self.send("POST", &format!("/{queue}/messages{q}"), &[], &msg_body(text)).await
    }

    pub async fn get(&self, queue: &str, query: &str) -> TestResponse {
        let q = if query.is_empty() { String::new() } else { format!("?{query}") };
        self.send("GET", &format!("/{queue}/messages{q}"), &[], "").await
    }

    pub async fn peek(&self, queue: &str, query: &str) -> TestResponse {
        let q = if query.is_empty() { String::new() } else { format!("&{query}") };
        self.send("GET", &format!("/{queue}/messages?peekonly=true{q}"), &[], "").await
    }
}

pub fn msg_body(text: &str) -> String {
    format!("<QueueMessage><MessageText>{text}</MessageText></QueueMessage>")
}

/// Text of the first `<tag>…</tag>` in a body.
pub fn tag(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&format!("</{name}>"))? + start;
    Some(body[start..end].to_string())
}

/// Every `<tag>…</tag>` in a body, in order.
pub fn tags(body: &str, name: &str) -> Vec<String> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let mut out = vec![];
    let mut rest = body;
    while let Some(s) = rest.find(&open) {
        let after = &rest[s + open.len()..];
        let Some(e) = after.find(&close) else { break };
        out.push(after[..e].to_string());
        rest = &after[e + close.len()..];
    }
    out
}

pub fn error_code(r: &TestResponse) -> String {
    r.header("x-ms-error-code").unwrap_or("").to_string()
}

pub fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A time `secs` from now (negative: in the past), as a SAS wants it.
pub fn sas_time(secs: i64) -> String {
    iso8601_secs(OffsetDateTime::now_utc() + Duration::seconds(secs))
}
