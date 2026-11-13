#![allow(dead_code)]

use std::sync::Arc;

use azemu_appconfig::AppConfigService;
use azemu_core::testing::{TestResponse, TestServer};
use serde_json::Value;

pub const VERSION: &str = "2024-09-01";
pub const AUTH: (&str, &str) = ("Authorization", "Bearer test-token");
pub const HMAC: (&str, &str) =
    ("Authorization", "HMAC-SHA256 Credential=id&SignedHeaders=x-ms-date;host;x-ms-content-sha256&Signature=abc");

/// A running emulator and one store on it.
pub struct T {
    pub srv: TestServer,
    pub base: String,
}

pub async fn start() -> T {
    let srv = TestServer::start(vec![Arc::new(AppConfigService::new())]).await;
    let base = srv.url("azconfig", "teststore");
    T { srv, base }
}

/// A store with limits other than the defaults (small ones, so a test reaches them with little data).
pub async fn start_with(limits: azemu_appconfig::Limits) -> T {
    let srv = TestServer::start(vec![Arc::new(AppConfigService::with_limits(limits))]).await;
    let base = srv.url("azconfig", "teststore");
    T { srv, base }
}

impl T {
    pub fn base_of(&self, store: &str) -> String {
        self.srv.url("azconfig", store)
    }

    /// Send with `Bearer` auth and `api-version=2024-09-01` added to the path.
    pub async fn call(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> TestResponse {
        self.call_v(VERSION, method, path, headers, body).await
    }

    pub async fn call_v(&self, version: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> TestResponse {
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = format!("{}{}{}api-version={}", self.base, path, sep, version);
        let mut h = vec![AUTH];
        h.extend_from_slice(headers);
        self.srv.send(method, &url, &h, body.to_string()).await
    }

    pub async fn get(&self, path: &str) -> TestResponse {
        self.call("GET", path, &[], "").await
    }

    /// PUT a setting with a JSON body; label goes in the query.
    pub async fn put(&self, key: &str, label: Option<&str>, body: Value) -> TestResponse {
        self.put_h(key, label, body, &[]).await
    }

    pub async fn put_h(&self, key: &str, label: Option<&str>, body: Value, headers: &[(&str, &str)]) -> TestResponse {
        let path = match label {
            Some(l) => format!("/kv/{}?label={}", enc(key), enc(l)),
            None => format!("/kv/{}", enc(key)),
        };
        let mut h = vec![("Content-Type", "application/json")];
        h.extend_from_slice(headers);
        self.call("PUT", &path, &h, &body.to_string()).await
    }

    pub async fn put_value(&self, key: &str, label: Option<&str>, value: &str) -> TestResponse {
        self.put(key, label, serde_json::json!({"value": value})).await
    }

    pub async fn delete(&self, key: &str, label: Option<&str>) -> TestResponse {
        let path = match label {
            Some(l) => format!("/kv/{}?label={}", enc(key), enc(l)),
            None => format!("/kv/{}", enc(key)),
        };
        self.call("DELETE", &path, &[], "").await
    }
}

/// Percent-encode everything but unreserved characters.
pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn keys_of(r: &TestResponse) -> Vec<String> {
    r.json()["items"].as_array().map(|a| a.iter().map(|i| i["key"].as_str().unwrap_or("").to_string()).collect()).unwrap_or_default()
}

pub fn labels_of(r: &TestResponse) -> Vec<Option<String>> {
    r.json()["items"].as_array().map(|a| a.iter().map(|i| i["label"].as_str().map(str::to_string)).collect()).unwrap_or_default()
}
