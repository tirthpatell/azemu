//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::sync::Arc;

use azemu_core::testing::{TestResponse, TestServer};
use azemu_keyvault::KeyVaultService;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;

pub struct Kv {
    pub srv: TestServer,
    pub base: String,
}

impl Kv {
    pub async fn new() -> Kv {
        Kv::in_vault("testvault").await
    }

    pub async fn in_vault(vault: &str) -> Kv {
        let srv = TestServer::start(vec![Arc::new(KeyVaultService::new())]).await;
        let base = srv.url("vault", vault);
        Kv { srv, base }
    }

    /// Another vault on the same server.
    pub fn vault_base(&self, vault: &str) -> String {
        self.srv.url("vault", vault)
    }

    /// Authenticated call. `path` may carry a query; `api-version=7.6` is added.
    pub async fn call(&self, method: &str, path: &str, body: Option<Value>) -> TestResponse {
        self.call_at(&self.base, method, path, body).await
    }

    pub async fn call_at(&self, base: &str, method: &str, path: &str, body: Option<Value>) -> TestResponse {
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = format!("{base}{path}{sep}api-version=7.6");
        let mut rb = self.srv.request(method, &url).header("Authorization", "Bearer test-token");
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        rb.send().await
    }

    pub async fn get(&self, path: &str) -> TestResponse {
        self.call("GET", path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> TestResponse {
        self.call("POST", path, Some(body)).await
    }
}

pub fn b64(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

pub fn unb64(s: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(s).expect("valid base64url")
}

pub fn code(r: &TestResponse) -> String {
    r.json()["error"]["code"].as_str().unwrap_or("").to_string()
}

pub fn inner_code(r: &TestResponse) -> String {
    r.json()["error"]["innererror"]["code"].as_str().unwrap_or("").to_string()
}

pub fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}
