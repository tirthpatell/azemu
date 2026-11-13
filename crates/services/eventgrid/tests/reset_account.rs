//! Resetting one topic host (`POST /_azemu/reset?account=<name>`): that host is emptied, the others are
//! not, deliveries waiting for a retry stop, receivers waiting on a pull subscription are released, the config file's
//! topics come back, and persistence sees a change.
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_eventgrid::{EventGridService, Options};
use common::*;
use serde_json::json;

async fn start(opts: Options) -> (Eg, Arc<EventGridService>) {
    let svc = Arc::new(EventGridService::with_options(opts));
    let srv = TestServer::start(vec![svc.clone()]).await;
    (Eg { srv }, svc)
}

#[tokio::test]
async fn one_host_is_emptied_and_the_other_is_not() {
    let (t, svc) = start(fast_options()).await;
    for ns in ["one", "two"] {
        assert_eq!(t.receive(ns, "jobs", "w", "maxWaitTime=0").await.status, 200);
        assert_eq!(t.ns_publish(ns, "jobs", json!([ce("1", "t")])).await.status, 200);
    }
    let before = svc.persist_generation();
    assert!(svc.reset_account("One"));
    assert_ne!(svc.persist_generation(), before, "persistence must see the reset");
    // `autocreate` is on here, so look at the admin view instead of receiving (receiving would create it again)
    assert_eq!(t.get_admin("/topics/jobs/subscriptions/w?namespace=one").await.status, 404);
    assert_eq!(t.get_admin("/topics/jobs/subscriptions/w?namespace=two").await.status, 200);
    let r = t.receive("two", "jobs", "w", "maxWaitTime=1").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["value"].as_array().map(Vec::len), Some(1));
    // nothing to reset: still true
    assert!(svc.reset_account("never-used"));
}

#[tokio::test]
async fn a_retry_waiting_for_its_turn_does_not_go_out_after_the_reset() {
    let (t, svc) = start(Options { retry_base_ms: 150, retry_max_ms: 150, ..fast_options() }).await;
    let hook = Hook::start(|_, _| Reply::status(500)).await;
    assert_eq!(t.put_json("/topics/orders", &json!({})).await.status, 201);
    let r = t.put_json("/topics/orders/subscriptions/s", &json!({"webhookUrl": hook.url, "skipValidation": true})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(t.publish_eg("orders", json!([eg("e1", "T", "/s")])).await.status, 200);
    assert!(eventually(5.0, || (hook.count() >= 1).then_some(())).await.is_some(), "the first attempt arrives");
    assert!(svc.reset_account("orders"));
    let seen = hook.count();
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(hook.count(), seen, "no retry after the reset");
}

#[tokio::test]
async fn the_config_file_topics_of_the_host_come_back() {
    let dir = std::env::temp_dir().join(format!("azemu-eg-reset-account-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("eg.json");
    std::fs::write(
        &path,
        r#"{"topics":[{"name":"orders","subscriptions":[{"name":"local","webhookUrl":"http://127.0.0.1:9/hook"}]},{"name":"other"}]}"#,
    )
    .unwrap();
    let (t, svc) = start(Options { config_path: Some(path), ..fast_options() }).await;
    assert_eq!(
        t.put_json("/topics/orders/subscriptions/extra", &json!({"webhookUrl": "http://127.0.0.1:9/x", "skipValidation": true}))
            .await
            .status,
        201
    );
    assert_eq!(
        t.put_json("/topics/other/subscriptions/extra", &json!({"webhookUrl": "http://127.0.0.1:9/x", "skipValidation": true}))
            .await
            .status,
        201
    );
    assert!(svc.reset_account("orders"));
    assert_eq!(t.get_admin("/topics/orders/subscriptions/local").await.status, 200, "declared by the file");
    assert_eq!(t.get_admin("/topics/orders/subscriptions/extra").await.status, 404, "made by a client");
    assert_eq!(t.get_admin("/topics/other/subscriptions/extra").await.status, 200, "another host is untouched");
}

#[tokio::test]
async fn a_pull_receive_that_waits_returns_when_its_host_is_reset() {
    // no auto-creation: the namespace comes from the config file; the file is emptied before the reset, so nothing comes back
    let dir = std::env::temp_dir().join(format!("azemu-eg-reset-wait-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("eg.json");
    std::fs::write(&path, r#"{"namespaces":[{"name":"ns","topics":[{"name":"jobs","subscriptions":[{"name":"w"}]}]}]}"#).unwrap();
    let (t, svc) = start(Options { autocreate: false, config_path: Some(path.clone()), ..fast_options() }).await;
    assert_eq!(t.receive("ns", "jobs", "w", "maxWaitTime=0").await.status, 200);
    std::fs::write(&path, "{}").unwrap();
    let t = Arc::new(t);
    let t2 = t.clone();
    let started = Instant::now();
    let waiter = tokio::spawn(async move { t2.receive("ns", "jobs", "w", "maxWaitTime=10").await.status });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(svc.reset_account("ns"));
    let status = tokio::time::timeout(Duration::from_secs(5), waiter).await.expect("the waiting receive returns").unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "released at once, not after its 10 seconds");
    assert_eq!(status, 404);
    let _ = std::fs::remove_dir_all(dir);
}
