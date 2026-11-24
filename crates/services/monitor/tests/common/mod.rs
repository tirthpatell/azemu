#![allow(dead_code)]

use std::io::Write;
use std::sync::Arc;

use azemu_core::testing::{TestResponse, TestServer};
use azemu_monitor::{Limits, MonitorService};
use serde_json::{json, Value};

pub const IKEY: &str = "11111111-2222-3333-4444-555555555555";

/// A running emulator with the monitor service on it.
pub struct T {
    pub srv: TestServer,
    /// `http://eastus.in.localhost:<port>`: the ingestion endpoint in the gateway host form.
    pub ingest: String,
    /// `http://eastus.livediagnostics.localhost:<port>`
    pub live: String,
    /// `http://dce1.ingest.localhost:<port>`
    pub logs: String,
    /// `http://127.0.0.1:<port>`: no host that names a service.
    pub root: String,
}

pub async fn start() -> T {
    start_with(Limits::default()).await
}

pub async fn start_with(limits: Limits) -> T {
    let srv = TestServer::start(vec![Arc::new(MonitorService::with_limits(limits))]).await;
    T {
        ingest: srv.url("in", "eastus"),
        live: srv.url("livediagnostics", "eastus"),
        logs: srv.url("ingest", "dce1"),
        root: srv.gateway_url(),
        srv,
    }
}

/// The `baseData` Azure accepts for each type, with `name` where the type has one.
pub fn base_data(base_type: &str, name: &str) -> Value {
    match base_type {
        "RequestData" => json!({"ver": 2, "id": "abc", "name": name, "duration": "0.00:00:00.012", "responseCode": "200", "success": true}),
        "RemoteDependencyData" => json!({"ver": 2, "name": name, "type": "HTTP", "duration": "0.00:00:00.003"}),
        "ExceptionData" => json!({"ver": 2, "exceptions": [{"typeName": "ValueError", "message": name}]}),
        "MessageData" => json!({"ver": 2, "message": name, "severityLevel": 1}),
        "MetricData" => json!({"ver": 2, "metrics": [{"name": name, "value": 4.0}]}),
        "PageViewData" => json!({"ver": 2, "name": name, "id": "p1"}),
        "AvailabilityData" => json!({"ver": 2, "id": "a1", "name": name, "duration": "00:00:01", "success": true}),
        _ => json!({"ver": 2, "name": name}),
    }
}

/// An envelope as the SDKs build it.
pub fn envelope(base_type: &str, name: &str, operation: &str) -> Value {
    let now = azemu_core::time::iso8601_7(time::OffsetDateTime::now_utc());
    json!({
        "ver": 1,
        "name": format!("Microsoft.ApplicationInsights.{}", base_type.trim_end_matches("Data")),
        "time": now,
        "sampleRate": 100.0,
        "iKey": IKEY,
        "tags": {"ai.operation.id": operation, "ai.cloud.role": "test"},
        "data": {"baseType": base_type, "baseData": base_data(base_type, name)},
    })
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

impl T {
    pub async fn track(&self, body: &Value) -> TestResponse {
        self.srv.send("POST", &format!("{}/v2/track", self.ingest), &[("content-type", "application/json")], body.to_string()).await
    }

    pub async fn track_raw(&self, path: &str, headers: &[(&str, &str)], body: Vec<u8>) -> TestResponse {
        self.srv.send("POST", &format!("{}{path}", self.ingest), headers, body).await
    }

    pub async fn admin(&self, method: &str, path: &str) -> TestResponse {
        self.srv.send(method, &format!("{}/_azemu/monitor{path}", self.root), &[], "").await
    }

    pub async fn telemetry(&self, query: &str) -> Value {
        let r = self.admin("GET", &format!("/telemetry{query}")).await;
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()
    }

    pub async fn upload(&self, dcr: &str, stream: &str, headers: &[(&str, &str)], body: Vec<u8>) -> TestResponse {
        let url = format!("{}/dataCollectionRules/{dcr}/streams/{stream}?api-version=2023-01-01", self.logs);
        self.srv.send("POST", &url, headers, body).await
    }
}

pub const BEARER: (&str, &str) = ("authorization", "Bearer token");
pub const JSON: (&str, &str) = ("content-type", "application/json");
