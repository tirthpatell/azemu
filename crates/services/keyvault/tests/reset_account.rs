//! Resetting one vault (`POST /_azemu/reset?account=<vault>`): its secrets, keys, certificates and their
//! deleted copies go; the other vaults stay; persistence sees a change.

mod common;

use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_keyvault::KeyVaultService;
use common::*;
use serde_json::json;

#[tokio::test]
async fn one_vault_is_emptied_and_the_other_is_not() {
    let svc = Arc::new(KeyVaultService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let kv = Kv { base: srv.url("vault", "alpha"), srv };
    let beta = kv.vault_base("beta");
    for base in [kv.base.clone(), beta.clone()] {
        assert_eq!(kv.call_at(&base, "PUT", "/secrets/s", Some(json!({"value": "v"}))).await.status, 200);
        assert_eq!(kv.call_at(&base, "PUT", "/secrets/gone", Some(json!({"value": "v"}))).await.status, 200);
        assert_eq!(kv.call_at(&base, "DELETE", "/secrets/gone", None).await.status, 200);
        assert_eq!(kv.call_at(&base, "POST", "/keys/k/create", Some(json!({"kty": "EC"}))).await.status, 200);
    }
    let before = svc.persist_generation();
    assert!(svc.reset_account("Alpha"));
    assert_ne!(svc.persist_generation(), before, "persistence must see the reset");

    assert_eq!(kv.get("/secrets/s").await.status, 404);
    assert_eq!(kv.get("/keys/k").await.status, 404);
    assert!(kv.get("/deletedsecrets").await.json()["value"].as_array().unwrap().is_empty(), "the deleted copies went too");
    assert_eq!(kv.call_at(&beta, "GET", "/secrets/s", None).await.status, 200);
    assert_eq!(kv.call_at(&beta, "GET", "/keys/k", None).await.status, 200);
    assert_eq!(kv.call_at(&beta, "GET", "/deletedsecrets", None).await.json()["value"].as_array().unwrap().len(), 1);
    // a name that was deleted before can be used at once (no soft-deleted copy blocks it)
    assert_eq!(kv.call("PUT", "/secrets/gone", Some(json!({"value": "new"}))).await.status, 200);
    assert!(svc.reset_account("never-used"));
}

#[tokio::test]
async fn the_admin_endpoint_resets_one_vault() {
    let srv = TestServer::start(vec![Arc::new(KeyVaultService::new())]).await;
    let kv = Kv { base: srv.url("vault", "alpha"), srv };
    let beta = kv.vault_base("beta");
    for base in [kv.base.clone(), beta.clone()] {
        assert_eq!(kv.call_at(&base, "PUT", "/secrets/s", Some(json!({"value": "v"}))).await.status, 200);
    }
    let r = kv.srv.send("POST", &format!("{}/_azemu/reset?account=alpha", kv.srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(kv.get("/secrets/s").await.status, 404);
    assert_eq!(kv.call_at(&beta, "GET", "/secrets/s", None).await.status, 200);
}
