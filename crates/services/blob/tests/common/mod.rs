#![allow(dead_code)]

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::testing::{TestResponse, TestServer};

pub const AUTH: (&str, &str) = ("Authorization", "SharedKey acct:c2ln");

pub struct Env {
    pub srv: TestServer,
    /// `http://acct.blob.localhost:port`
    pub base: String,
}

pub async fn start() -> Env {
    start_with(BlobService::new()).await
}

/// A service whose accounts keep versions (blob versioning is off by default, as on Azure).
pub async fn start_versioned() -> Env {
    start_with(BlobService::with_versioning(true, 256 << 20)).await
}

pub async fn start_with(svc: BlobService) -> Env {
    let srv = TestServer::start(vec![Arc::new(svc)]).await;
    let base = srv.url("blob", "acct");
    Env { srv, base }
}

impl Env {
    /// Send with the shared key header plus `extra` headers.
    pub async fn send(&self, method: &str, path: &str, extra: &[(&str, &str)], body: impl Into<azemu_core::bytes::Bytes>) -> TestResponse {
        let mut h = vec![AUTH];
        h.extend_from_slice(extra);
        self.srv.send(method, &format!("{}{path}", self.base), &h, body).await
    }

    /// Send with no credentials at all.
    pub async fn anon(&self, method: &str, path: &str, extra: &[(&str, &str)], body: impl Into<azemu_core::bytes::Bytes>) -> TestResponse {
        self.srv.send(method, &format!("{}{path}", self.base), extra, body).await
    }

    pub async fn create_container(&self, name: &str) {
        let r = self.send("PUT", &format!("/{name}?restype=container"), &[], "").await;
        assert_eq!(r.status, 201, "{}", r.text());
    }

    pub async fn put_blob(&self, container: &str, name: &str, body: &str) -> TestResponse {
        self.send("PUT", &format!("/{container}/{name}"), &[("x-ms-blob-type", "BlockBlob")], body.to_string()).await
    }
}

pub fn code(r: &TestResponse) -> String {
    r.header("x-ms-error-code").unwrap_or("").to_string()
}

/// Text of every `<tag>...</tag>` in an XML document, in order.
pub fn tags(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(after[..j].to_string());
        rest = &after[j + close.len()..];
    }
    out
}

/// Text of `<NextMarker>`, empty when the element is empty.
pub fn next_marker(xml: &str) -> String {
    tags(xml, "NextMarker").into_iter().next().unwrap_or_default()
}
