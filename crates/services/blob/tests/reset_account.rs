//! Resetting one storage account (`POST /_azemu/reset?account=<name>`): the account is emptied, others are
//! not, the account's versioning switch goes back to the default, and persistence sees a change.

mod common;

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::testing::TestServer;
use azemu_core::Service;

const AUTH: (&str, &str) = ("Authorization", "SharedKey acct:c2ln");

async fn put(srv: &TestServer, base: &str, path: &str, body: &str) {
    let r = srv.send("PUT", &format!("{base}{path}"), &[AUTH, ("x-ms-blob-type", "BlockBlob")], body.to_string()).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

#[tokio::test]
async fn one_account_is_emptied_and_the_other_is_not() {
    let svc = Arc::new(BlobService::new());
    let lake = svc.datalake();
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>, Arc::new(lake)]).await;
    let (a, b) = (srv.url("blob", "alpha"), srv.url("blob", "beta"));
    for base in [&a, &b] {
        assert_eq!(srv.send("PUT", &format!("{base}/cnt?restype=container"), &[AUTH], "").await.status, 201);
        put(&srv, base, "/cnt/blob", "data").await;
    }
    svc.set_versioning_enabled("alpha", true);
    svc.set_versioning_enabled("beta", true);

    let before = svc.persist_generation();
    assert!(svc.reset_account("Alpha"));
    assert_ne!(svc.persist_generation(), before, "persistence must see the reset");

    assert_eq!(srv.send("GET", &format!("{a}/cnt/blob"), &[AUTH], "").await.status, 404);
    assert_eq!(srv.send("GET", &format!("{b}/cnt/blob"), &[AUTH], "").await.text(), "data");
    // the per-account switch is back at the default (off); the other account keeps its own
    assert!(!svc.versioning_enabled("alpha"));
    assert!(svc.versioning_enabled("beta"));
    // the account works again as a new one
    assert_eq!(srv.send("PUT", &format!("{a}/cnt?restype=container"), &[AUTH], "").await.status, 201);
    // an account that was never used: nothing to do, still true
    assert!(svc.reset_account("never-used"));
    assert_eq!(srv.send("GET", &format!("{b}/cnt/blob"), &[AUTH], "").await.text(), "data");
}

#[tokio::test]
async fn the_admin_endpoint_resets_one_account_of_blob_and_data_lake() {
    let svc = Arc::new(BlobService::new());
    let lake = svc.datalake();
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>, Arc::new(lake)]).await;
    let (a, b) = (srv.url("blob", "alpha"), srv.url("blob", "beta"));
    for base in [&a, &b] {
        assert_eq!(srv.send("PUT", &format!("{base}/cnt?restype=container"), &[AUTH], "").await.status, 201);
        put(&srv, base, "/cnt/blob", "data").await;
    }
    let r = srv.send("POST", &format!("{}/_azemu/reset?account=alpha", srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(srv.send("GET", &format!("{a}/cnt/blob"), &[AUTH], "").await.status, 404);
    assert_eq!(srv.send("GET", &format!("{b}/cnt/blob"), &[AUTH], "").await.status, 200);
}
