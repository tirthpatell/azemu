#![allow(dead_code)]

use std::sync::Arc;

use azemu_arm::{ArmService, Backends};
use azemu_core::testing::{TestResponse, TestServer};
use serde_json::Value;

pub const SUB: &str = "11111111-2222-3333-4444-555555555555";

pub struct Arm {
    pub srv: TestServer,
}

pub async fn standalone() -> Arm {
    let svc = ArmService::with_strict(Backends::none(), false);
    Arm { srv: TestServer::start(vec![Arc::new(svc)]).await }
}

pub async fn strict() -> Arm {
    let svc = ArmService::with_strict(Backends::none(), true);
    Arm { srv: TestServer::start(vec![Arc::new(svc)]).await }
}

impl Arm {
    pub fn base(&self) -> String {
        format!("http://management.localhost:{}", self.srv.port())
    }

    /// Send with a bearer token and the given api-version appended.
    pub async fn call(&self, method: &str, path: &str, body: Option<Value>) -> TestResponse {
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = format!("{}{}{}api-version=2024-01-01", self.base(), path, sep);
        let b = body.map(|v| v.to_string()).unwrap_or_default();
        self.srv.send(method, &url, &[("Authorization", "Bearer x"), ("Content-Type", "application/json")], b).await
    }

    pub async fn put(&self, path: &str, body: Value) -> TestResponse {
        self.call("PUT", path, Some(body)).await
    }
    pub async fn get(&self, path: &str) -> TestResponse {
        self.call("GET", path, None).await
    }
    pub async fn delete(&self, path: &str) -> TestResponse {
        self.call("DELETE", path, None).await
    }
    pub async fn post(&self, path: &str, body: Value) -> TestResponse {
        self.call("POST", path, Some(body)).await
    }

    pub async fn group(&self, name: &str) {
        let r = self.put(&format!("/subscriptions/{SUB}/resourcegroups/{name}"), serde_json::json!({"location": "eastus"})).await;
        assert!(r.status == 200 || r.status == 201, "create group: {} {}", r.status, r.text());
    }
}

pub async fn full() -> Arm {
    let stack = azemu::full_stack();
    Arm { srv: TestServer::start_with(stack.services, stack.raw).await }
}

impl Arm {
    /// A data plane URL `http://{account}.{label}.core.localhost:{port}`.
    pub fn dp(&self, label: &str, account: &str) -> String {
        format!("http://{account}.{label}.core.localhost:{}", self.srv.port())
    }

    pub async fn dp_send(&self, method: &str, url: &str, headers: &[(&str, &str)], body: &str) -> TestResponse {
        let mut h = vec![("Authorization", "Bearer x")];
        h.extend_from_slice(headers);
        self.srv.send(method, url, &h, body.to_string()).await
    }
}

/// The real wiring on a data directory (no timed saves: `srv.save_now()` / `srv.shutdown()` write, dropping does not).
pub async fn full_persistent(
    dir: &std::path::Path,
) -> (Arm, std::sync::Arc<dyn azemu_core::Service>, Vec<std::sync::Arc<dyn azemu_core::Service>>) {
    let stack = azemu::full_stack();
    let all = stack.services.clone();
    let arm = stack.services.iter().find(|s| s.name() == "arm").cloned().expect("arm in the stack");
    let data = dir.to_path_buf();
    let srv =
        TestServer::start_configured(stack.services, stack.raw, move |b| b.data_dir(data).save_interval(std::time::Duration::ZERO)).await;
    (Arm { srv }, arm, all)
}
