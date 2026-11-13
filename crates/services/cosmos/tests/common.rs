#![allow(dead_code)]

use std::sync::Arc;

use azemu_core::testing::{TestResponse, TestServer};
use azemu_cosmos::{CosmosService, ScriptSettings};
use serde_json::{json, Value};

pub struct H {
    pub srv: TestServer,
    pub base: String,
    pub svc: Arc<CosmosService>,
}

/// The default script settings, with the worker program of this crate (in a test the current executable is the test binary, which
/// cannot be a script worker).
pub fn settings() -> ScriptSettings {
    ScriptSettings { worker: Some(env!("CARGO_BIN_EXE_azemu-cosmos-script-worker").into()), ..ScriptSettings::default() }
}

pub async fn start() -> H {
    let svc = Arc::new(CosmosService::with_script_settings(settings()));
    let srv = TestServer::start(vec![svc.clone()]).await;
    let base = srv.url("documents", "acct1");
    H { srv, base, svc }
}

impl H {
    pub async fn req(&self, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> TestResponse {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let mut headers: Vec<(&str, &str)> = vec![
            ("authorization", "type%3Dmaster%26ver%3D1.0%26sig%3Dxyz"),
            ("x-ms-date", "Wed, 11 Nov 2026 12:00:00 GMT"),
            ("x-ms-version", "2020-07-15"),
        ];
        headers.extend_from_slice(extra);
        self.srv.send(method, &url, &headers, body.to_string()).await
    }

    /// A request with a binary body and no content type (what the .NET SDK sends for a batch).
    pub async fn raw(&self, method: &str, path: &str, extra: &[(&str, &str)], body: Vec<u8>) -> TestResponse {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let mut headers: Vec<(&str, &str)> = vec![
            ("authorization", "type%3Dmaster%26ver%3D1.0%26sig%3Dxyz"),
            ("x-ms-date", "Wed, 11 Nov 2026 12:00:00 GMT"),
            ("x-ms-version", "2020-07-15"),
        ];
        headers.extend_from_slice(extra);
        self.srv.send(method, &url, &headers, body).await
    }

    pub async fn json(&self, method: &str, path: &str, extra: &[(&str, &str)], body: &Value) -> TestResponse {
        let mut h = vec![("content-type", "application/json")];
        h.extend_from_slice(extra);
        self.req(method, path, &h, &body.to_string()).await
    }

    pub async fn db(&self, id: &str) -> Value {
        let r = self.json("POST", "dbs", &[], &json!({ "id": id })).await;
        assert_eq!(r.status, 201, "{}", r.text());
        r.json()
    }

    pub async fn coll(&self, db: &str, id: &str, pk: &str) -> Value {
        let r = self
            .json(
                "POST",
                &format!("dbs/{db}/colls"),
                &[],
                &json!({ "id": id, "partitionKey": { "paths": [pk], "kind": "Hash", "version": 2 } }),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.text());
        r.json()
    }

    /// Create a document with the partition key header `["pkvalue"]`.
    pub async fn doc(&self, db: &str, coll: &str, pk: &Value, body: &Value) -> TestResponse {
        let pkh = json!([pk]).to_string();
        self.json("POST", &format!("dbs/{db}/colls/{coll}/docs"), &[("x-ms-documentdb-partitionkey", &pkh)], body).await
    }

    pub async fn query(&self, db: &str, coll: &str, extra: &[(&str, &str)], q: Value) -> TestResponse {
        let mut h = vec![("content-type", "application/query+json"), ("x-ms-documentdb-isquery", "True")];
        h.extend_from_slice(extra);
        self.req("POST", &format!("dbs/{db}/colls/{coll}/docs"), &h, &q.to_string()).await
    }
}
