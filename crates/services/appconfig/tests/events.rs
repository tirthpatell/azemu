//! Events raised for key-value writes: which requests raise one, and the subject.

mod common;

use std::sync::Arc;

use azemu_appconfig::{AppConfigService, StoreEvent, StoreEventSink};
use azemu_core::testing::TestServer;
use serde_json::json;

#[derive(Default)]
struct Recorder(parking_lot::Mutex<Vec<StoreEvent>>);

impl StoreEventSink for Recorder {
    fn wants(&self, _store: &str) -> bool {
        true
    }
    fn raise(&self, event: StoreEvent) {
        self.0.lock().push(event);
    }
}

async fn start() -> (TestServer, Arc<Recorder>) {
    let svc = Arc::new(AppConfigService::new());
    let rec = Arc::new(Recorder::default());
    svc.set_event_sink(rec.clone());
    (TestServer::start(vec![svc]).await, rec)
}

async fn send(srv: &TestServer, method: &str, path: &str, body: serde_json::Value) {
    let url = format!("{}{path}", srv.url("azconfig", "evstore"));
    let sep = if path.contains('?') { '&' } else { '?' };
    let r = srv
        .send(
            method,
            &format!("{url}{sep}api-version=2024-09-01"),
            &[("Authorization", "Bearer x"), ("Content-Type", "application/json")],
            body.to_string(),
        )
        .await;
    assert_eq!(r.status, 200, "{method} {path}: {}", r.text());
}

#[tokio::test]
async fn a_key_with_slashes_in_the_path_raises_an_event_and_the_label_is_encoded_in_the_subject() {
    let (srv, rec) = start().await;
    send(&srv, "PUT", "/kv/plain", json!({"value": "1"})).await;
    send(&srv, "PUT", "/kv/app/color", json!({"value": "red"})).await; // the slash is not encoded
    send(&srv, "PUT", "/kv/app%2Fsize", json!({"value": "big"})).await; // and here it is
    send(&srv, "PUT", "/kv/lbl?label=a%20b%26c", json!({"value": "x"})).await;
    send(&srv, "DELETE", "/kv/app/color", json!({})).await;
    let ev = rec.0.lock().clone();
    let subjects: Vec<&str> = ev.iter().map(|e| e.subject.rsplit_once("/kv/").unwrap().1).collect();
    assert_eq!(subjects, ["plain", "app/color", "app/size", "lbl?label=a%20b%26c", "app/color"]);
    let kinds: Vec<&str> = ev.iter().map(|e| e.event_type.rsplit('.').next().unwrap()).collect();
    assert_eq!(kinds, ["KeyValueModified", "KeyValueModified", "KeyValueModified", "KeyValueModified", "KeyValueDeleted"]);
    assert_eq!(ev[1].data["key"], "app/color");
    assert_eq!(ev[3].data["label"], "a b&c", "the data carries the label as it is, the subject (a URL) carries it encoded");
}
