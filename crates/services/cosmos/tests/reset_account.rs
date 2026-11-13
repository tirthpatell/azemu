//! Resetting one Cosmos account (`POST /_azemu/reset?service=cosmos&account=<name>`): that account is emptied,
//! the others are not, kept query results go, scripts that run for it end, and persistence sees a change.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_cosmos::{CosmosService, ScriptSettings};
use common::{settings, H};
use serde_json::json;

const PK: &str = "x-ms-documentdb-partitionkey";

async fn two_accounts(s: ScriptSettings) -> (H, H) {
    let svc = Arc::new(CosmosService::with_script_settings(s));
    let srv = TestServer::start(vec![svc.clone()]).await;
    // both handles talk to the same server; `H` holds the base URL of one account
    let a = H { base: srv.url("documents", "one"), svc: svc.clone(), srv };
    let b = H { base: a.srv.url("documents", "two"), svc, srv: TestServer::start(vec![]).await };
    for h in [&a, &b] {
        h.db("d").await;
        h.coll("d", "c", "/pk").await;
        for i in 0..5 {
            assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": format!("i{i}"), "pk": "p"})).await.status, 201);
        }
    }
    (a, b)
}

#[tokio::test]
async fn one_account_is_emptied_and_the_other_is_not() {
    let (a, b) = two_accounts(settings()).await;
    let before = a.svc.persist_generation();
    // a query with a second page leaves a kept result behind
    let r = a.query("d", "c", &[("x-ms-max-item-count", "2"), (PK, "[\"p\"]")], json!({"query": "SELECT * FROM c"})).await;
    assert!(r.header("x-ms-continuation").is_some());
    assert_eq!(a.svc.kept_results(), 1);

    let url = format!("{}/_azemu/reset?service=cosmos&account=one", a.srv.gateway_url());
    let r = a.srv.send("POST", &url, &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["reset"], json!(["cosmos"]));

    assert_ne!(a.svc.persist_generation(), before, "persistence must see the reset");
    assert_eq!(a.svc.kept_results(), 0, "kept query results are gone");
    assert_eq!(a.req("GET", "dbs/d", &[], "").await.status, 404);
    assert_eq!(a.req("GET", "dbs/d/colls/c/docs/i0", &[(PK, "[\"p\"]")], "").await.status, 404);
    // the other account is as it was
    let r = b.req("GET", "dbs/d/colls/c/docs/i0", &[(PK, "[\"p\"]")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    // the reset account can be used again, and a name that was never used is not an error
    a.db("d").await;
    let url = format!("{}/_azemu/reset?service=cosmos&account=never-used", a.srv.gateway_url());
    assert_eq!(a.srv.send("POST", &url, &[], "").await.status, 200);
    // names are not case sensitive, like the host label
    assert!(a.svc.reset_account("TWO"));
    assert_eq!(b.req("GET", "dbs/d", &[], "").await.status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_script_that_runs_for_the_account_ends_at_once() {
    let (a, b) = two_accounts(ScriptSettings { timeout: Duration::from_secs(30), ..settings() }).await;
    let spin = "function () { var t = Date.now(); while (Date.now() - t < 20000) { } }";
    for h in [&a, &b] {
        assert_eq!(h.json("POST", "dbs/d/colls/c/sprocs", &[], &json!({"id": "spin", "body": spin})).await.status, 201);
    }
    let run = |h: &H| {
        let (srv_url, auth) = (format!("{}/dbs/d/colls/c/sprocs/spin", h.base), "type%3Dmaster%26ver%3D1.0%26sig%3Dxyz");
        let client = a.srv.client().clone();
        async move {
            let t = Instant::now();
            let r = client
                .send(
                    "POST",
                    &srv_url,
                    &[("authorization", auth), ("x-ms-version", "2020-07-15"), ("content-type", "application/json"), (PK, "[\"p\"]")],
                    "[]",
                )
                .await;
            (r.status, t.elapsed())
        }
    };
    let ra = tokio::spawn(run(&a));
    let rb = tokio::spawn(run(&b));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(a.svc.reset_account("one"));
    let (status, took) = ra.await.unwrap();
    assert_eq!(status, 404, "a script of a removed account answers as for a removed container");
    assert!(took < Duration::from_secs(10), "the run ended after {took:?}");
    // the other account's script is still running
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rb.is_finished(), "the script of the other account must go on");
    // a full reset ends it too
    a.svc.reset();
    let (status, took) = rb.await.unwrap();
    assert_eq!(status, 404);
    assert!(took < Duration::from_secs(10), "the run ended after {took:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_reset_is_saved() {
    let dir = std::env::temp_dir().join(format!("azemu-cosmos-resetacct-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let make = |dir: std::path::PathBuf| async move {
        let svc = Arc::new(CosmosService::with_script_settings(settings()));
        let srv = TestServer::start_persistent(vec![svc.clone()], &dir).await;
        H { base: srv.url("documents", "one"), svc, srv }
    };
    let a = make(dir.clone()).await;
    a.db("d").await;
    a.coll("d", "c", "/pk").await;
    a.doc("d", "c", &json!("p"), &json!({"id": "x", "pk": "p"})).await;
    let other = H { base: a.srv.url("documents", "two"), svc: a.svc.clone(), srv: TestServer::start(vec![]).await };
    other.db("keep").await;
    a.srv.save_now().await;
    assert!(a.svc.reset_account("one"));
    a.srv.save_now().await;
    a.srv.shutdown().await.unwrap();
    let a2 = make(dir.clone()).await;
    assert_eq!(a2.req("GET", "dbs/d", &[], "").await.status, 404, "the removed account stays removed after a restart");
    let b2 = H { base: a2.srv.url("documents", "two"), svc: a2.svc.clone(), srv: TestServer::start(vec![]).await };
    assert_eq!(b2.req("GET", "dbs/keep", &[], "").await.status, 200);
    a2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
