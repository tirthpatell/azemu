//! Resetting one store (`POST /_azemu/reset?account=<store>`): its key-values, history and snapshots go; the
//! other stores stay; persistence sees a change.

mod common;

use std::sync::Arc;

use azemu_appconfig::AppConfigService;
use azemu_core::Service;
use common::*;
use serde_json::json;

#[tokio::test]
async fn one_store_is_emptied_and_the_other_is_not() {
    let svc = Arc::new(AppConfigService::new());
    let srv = azemu_core::testing::TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let t = T { base: srv.url("azconfig", "alpha"), srv };
    let beta = t.base_of("beta");
    t.put_value("k", None, "alpha-value").await;
    t.call("PUT", "/snapshots/s1", &[("Content-Type", "application/json")], r#"{"filters":[{"key":"*"}]}"#).await;
    let url = format!("{beta}/kv/k?api-version={VERSION}");
    let r = t.srv.send("PUT", &url, &[AUTH, ("Content-Type", "application/json")], json!({"value": "beta-value"}).to_string()).await;
    assert_eq!(r.status, 200);

    let before = svc.persist_generation();
    assert!(svc.reset_account("Alpha"));
    assert_ne!(svc.persist_generation(), before, "persistence must see the reset");

    assert_eq!(t.get("/kv/k").await.status, 404);
    assert!(keys_of(&t.get("/kv").await).is_empty());
    assert_eq!(t.get("/snapshots/s1").await.status, 404);
    assert!(t.get("/revisions").await.json()["items"].as_array().unwrap().is_empty(), "the history went too");
    let r = t.srv.send("GET", &url, &[AUTH], "").await;
    assert_eq!(r.json()["value"], "beta-value");
    assert_eq!(t.put_value("k", None, "again").await.status, 200);
    assert!(svc.reset_account("never-used"));
}

#[tokio::test]
async fn the_admin_endpoint_resets_one_store() {
    let srv = azemu_core::testing::TestServer::start(vec![Arc::new(AppConfigService::new())]).await;
    let t = T { base: srv.url("azconfig", "alpha"), srv };
    let beta = t.base_of("beta");
    t.put_value("k", None, "v").await;
    let url = format!("{beta}/kv/k?api-version={VERSION}");
    t.srv.send("PUT", &url, &[AUTH, ("Content-Type", "application/json")], json!({"value": "v"}).to_string()).await;
    let r = t.srv.send("POST", &format!("{}/_azemu/reset?account=alpha", t.srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(t.get("/kv/k").await.status, 404);
    assert_eq!(t.srv.send("GET", &url, &[AUTH], "").await.status, 200);
}
