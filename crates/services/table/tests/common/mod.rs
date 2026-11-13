#![allow(dead_code)]

use std::sync::Arc;

use azemu_core::serde_json::Value;
use azemu_core::testing::{TestResponse, TestServer};
use azemu_table::TableService;

pub const AUTH: (&str, &str) = ("Authorization", "SharedKey acct:c2ln");
pub const JSON: (&str, &str) = ("Content-Type", "application/json");

pub struct Env {
    pub srv: TestServer,
    pub base: String,
}

pub async fn env() -> Env {
    let srv = TestServer::start(vec![Arc::new(TableService::new())]).await;
    let base = srv.url("table", "acct");
    Env { srv, base }
}

impl Env {
    pub async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> TestResponse {
        let mut h = vec![AUTH];
        h.extend_from_slice(headers);
        self.srv.send(method, &format!("{}{}", self.base, path), &h, body.to_string()).await
    }

    pub async fn create_table(&self, name: &str) -> TestResponse {
        self.send("POST", "/Tables", &[JSON], &format!(r#"{{"TableName":"{name}"}}"#)).await
    }

    pub async fn insert(&self, table: &str, body: &str) -> TestResponse {
        self.send("POST", &format!("/{table}"), &[JSON], body).await
    }

    pub async fn get(&self, table: &str, pk: &str, rk: &str) -> TestResponse {
        self.send("GET", &format!("/{table}(PartitionKey='{pk}',RowKey='{rk}')"), &[], "").await
    }

    /// Insert `n` entities `p/000 .. p/{n-1}` with `v` set to the index.
    pub async fn seed(&self, table: &str, n: usize) {
        for i in 0..n {
            let r = self.insert(table, &format!(r#"{{"PartitionKey":"p","RowKey":"{i:04}","v":{i}}}"#)).await;
            assert_eq!(r.status, 201, "{}", r.text());
        }
    }

    pub async fn row_keys(&self, table: &str, query: &str) -> Vec<String> {
        let r = self.send("GET", &format!("/{table}(){query}"), &[], "").await;
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()["value"].as_array().unwrap().iter().map(|e| e["RowKey"].as_str().unwrap().to_string()).collect()
    }
}

pub fn code(r: &TestResponse) -> String {
    r.json()["odata.error"]["code"].as_str().unwrap_or_default().to_string()
}

pub fn msg(r: &TestResponse) -> String {
    r.json()["odata.error"]["message"]["value"].as_str().unwrap_or_default().to_string()
}

pub fn value(r: &TestResponse) -> Value {
    r.json()
}

/// A multipart batch holding one change set of the given requests
/// (`method`, `path`, extra headers, body).
pub type Op<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], &'a str);

pub fn batch(ops: &[Op]) -> String {
    let mut s = String::from("--batch_b\r\nContent-Type: multipart/mixed; boundary=changeset_c\r\n\r\n");
    for (i, (method, path, headers, body)) in ops.iter().enumerate() {
        s.push_str(&format!(
            "--changeset_c\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\nContent-ID: {i}\r\n\r\n{method} http://acct.table.core.windows.net{path} HTTP/1.1\r\n"
        ));
        if !body.is_empty() {
            s.push_str("Content-Type: application/json\r\n");
        }
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("accept")) {
            s.push_str("Accept: application/json;odata=minimalmetadata\r\n");
        }
        for (k, v) in *headers {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        s.push_str(&format!("\r\n{body}\r\n"));
    }
    s.push_str("--changeset_c--\r\n\r\n--batch_b--\r\n");
    s
}

pub const BATCH_CT: (&str, &str) = ("Content-Type", "multipart/mixed; boundary=batch_b");

/// The status line and body of every part of a batch response.
pub fn parts(r: &TestResponse) -> Vec<(u16, String)> {
    let text = r.text();
    let mut out = Vec::new();
    for chunk in text.split("HTTP/1.1 ").skip(1) {
        let status: u16 = chunk[..3].parse().unwrap();
        let body = chunk.split("\r\n\r\n").nth(1).unwrap_or("").split("\r\n--").next().unwrap_or("").to_string();
        out.push((status, body));
    }
    out
}
