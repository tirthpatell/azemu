#![allow(dead_code)]

use std::sync::Arc;

use azemu_core::testing::{TestResponse, TestServer};
use azemu_file::FileService;

pub const ACCOUNT: &str = "acct1";

/// A file service behind a gateway, and helpers to talk to it with a shared key header (never compared for this account).
pub struct F {
    pub srv: TestServer,
    pub base: String,
}

pub async fn start() -> F {
    let srv = TestServer::start(vec![Arc::new(FileService::new())]).await;
    let base = srv.url("file.core", ACCOUNT);
    F { srv, base }
}

impl F {
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: impl Into<azemu_core::bytes::Bytes>,
    ) -> TestResponse {
        let mut h: Vec<(&str, &str)> = vec![("Authorization", "SharedKey acct1:c2ln")];
        h.extend_from_slice(headers);
        self.srv.send(method, &format!("{}{}", self.base, path), &h, body).await
    }

    pub async fn anon(&self, method: &str, path: &str) -> TestResponse {
        self.srv.send(method, &format!("{}{}", self.base, path), &[], "").await
    }

    /// Create a share and (with a path) the directories and files of `spec`: `d1/` makes a directory, `d1/f.txt:5` a file of 5 bytes.
    pub async fn share(&self, name: &str) {
        let r = self.send("PUT", &format!("/{name}?restype=share"), &[], "").await;
        assert_eq!(r.status, 201, "create share {name}: {}", r.text());
    }

    pub async fn dir(&self, path: &str) {
        let r = self.send("PUT", &format!("/{path}?restype=directory"), &[], "").await;
        assert_eq!(r.status, 201, "create dir {path}: {}", r.text());
    }

    pub async fn file(&self, path: &str, size: u64) {
        let r = self.send("PUT", &format!("/{path}"), &[("x-ms-type", "file"), ("x-ms-content-length", &size.to_string())], "").await;
        assert_eq!(r.status, 201, "create file {path}: {}", r.text());
    }

    pub async fn write(&self, path: &str, offset: u64, data: &str) {
        let range = format!("bytes={}-{}", offset, offset + data.len() as u64 - 1);
        let r =
            self.send("PUT", &format!("/{path}?comp=range"), &[("x-ms-range", &range), ("x-ms-write", "update")], data.to_string()).await;
        assert_eq!(r.status, 201, "write {path}: {}", r.text());
    }

    pub async fn write_with_lease(&self, path: &str, offset: u64, data: &str, lease: &str) {
        let range = format!("bytes={}-{}", offset, offset + data.len() as u64 - 1);
        let r = self
            .send(
                "PUT",
                &format!("/{path}?comp=range"),
                &[("x-ms-range", &range), ("x-ms-write", "update"), ("x-ms-lease-id", lease)],
                data.to_string(),
            )
            .await;
        assert_eq!(r.status, 201, "write {path}: {}", r.text());
    }

    pub async fn read(&self, path: &str) -> String {
        let r = self.send("GET", &format!("/{path}"), &[], "").await;
        assert_eq!(r.status, 200, "read {path}");
        r.text()
    }
}

pub fn error_code(r: &TestResponse) -> String {
    r.header("x-ms-error-code").unwrap_or("").to_string()
}

/// Text of the first `<tag>...</tag>` in a body.
pub fn tag(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&format!("</{name}>"))? + start;
    Some(body[start..end].to_string())
}

pub fn all(body: &str, name: &str) -> Vec<String> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let mut out = vec![];
    let mut rest = body;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(after[..j].to_string());
        rest = &after[j..];
    }
    out
}

/// Status and error code.
pub fn failed(r: &TestResponse) -> (u16, String) {
    (r.status, error_code(r))
}
