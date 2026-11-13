//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/cosmos/` holds what `Service::save` wrote (`AZEMU-STATE cosmos 1` for `cosmos.state` and
//! `AZEMU-STATE cosmos-docs 1` for the document file of each container) for the small state built by `build_fixed_state`.
//! The files are checked in and the test run never rewrites them. `version_1_files_still_load` loads them into a new
//! service and reads back through the REST API what they hold.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in files
//! and do not weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (reading it into the
//! same state this test expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-cosmos --test state_v1 -- --ignored write_the_fixture`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_cosmos::CosmosService;
use common::{settings, H};
use serde_json::json;

const PK: &str = "x-ms-documentdb-partitionkey";

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("cosmos")
}

async fn build_fixed_state() -> H {
    let h = common::start().await;
    h.db("fixture").await;
    let r = h
        .json(
            "POST",
            "dbs/fixture/colls",
            &[],
            &json!({
                "id": "items",
                "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2},
                "defaultTtl": 100000,
                "uniqueKeyPolicy": {"uniqueKeys": [{"paths": ["/email"]}]},
                "indexingPolicy": {"indexingMode": "consistent", "automatic": true, "includedPaths": [{"path": "/*"}], "excludedPaths": [{"path": "/\"_etag\"/?"}]}
            }),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    for (id, pk, extra) in [
        ("a", "p1", json!({"email": "a@x", "n": 1})),
        ("USER#1?x", "p1", json!({"email": "b@x", "n": 2})),
        ("c", "p2", json!({"email": "c@x", "n": 3, "tags": ["t1", "t2"]})),
    ] {
        let mut body = json!({"id": id, "pk": pk});
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert_eq!(h.doc("fixture", "items", &json!(pk), &body).await.status, 201);
    }
    // a replace and a delete, so that the change feed and the LSN have a history
    let r = h
        .json("PUT", "dbs/fixture/colls/items/docs/a", &[(PK, "[\"p1\"]")], &json!({"id": "a", "pk": "p1", "email": "a@x", "n": 10}))
        .await;
    assert_eq!(r.status, 200);
    h.doc("fixture", "items", &json!("p2"), &json!({"id": "gone", "pk": "p2", "email": "g@x"})).await;
    assert_eq!(h.req("DELETE", "dbs/fixture/colls/items/docs/gone", &[(PK, "[\"p2\"]")], "").await.status, 204);
    // a second container with a hierarchical key
    let r = h
        .json(
            "POST",
            "dbs/fixture/colls",
            &[],
            &json!({"id": "multi", "partitionKey": {"paths": ["/a", "/b"], "kind": "MultiHash", "version": 2}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    h.json("POST", "dbs/fixture/colls/multi/docs", &[(PK, "[\"x\",\"y\"]")], &json!({"id": "m1", "a": "x", "b": "y"})).await;
    // scripts, a user and a permission
    for (kind, body) in [
        ("sprocs", json!({"id": "hello", "body": "function () { getContext().getResponse().setBody('hi'); }"})),
        ("triggers", json!({"id": "pre", "body": "function () { }", "triggerType": "Pre", "triggerOperation": "All"})),
        ("udfs", json!({"id": "double", "body": "function (x) { return x * 2; }"})),
    ] {
        assert_eq!(h.json("POST", &format!("dbs/fixture/colls/items/{kind}"), &[], &body).await.status, 201);
    }
    assert_eq!(h.json("POST", "dbs/fixture/users", &[], &json!({"id": "u1"})).await.status, 201);
    let r = h
        .json(
            "POST",
            "dbs/fixture/users/u1/permissions",
            &[],
            &json!({"id": "p1", "permissionMode": "Read", "resource": "dbs/fixture/colls/items"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    h
}

#[tokio::test]
#[ignore = "writes the checked-in fixture; see the module text"]
async fn write_the_fixture() {
    let h = build_fixed_state().await;
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    h.svc.save(&dir).unwrap();
}

#[tokio::test]
async fn version_1_files_still_load() {
    let dir = std::env::temp_dir().join(format!("azemu-cosmos-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut copied = 0;
    for entry in std::fs::read_dir(fixture_dir()).expect("tests/state-v1/cosmos is checked in") {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
        copied += 1;
    }
    assert!(copied >= 3, "the fixture has the definitions file and one file per container ({copied} files found)");

    let svc = Arc::new(CosmosService::with_script_settings(settings()));
    if let Err(e) = svc.load(&dir) {
        panic!(
            "Version 1 Cosmos state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/persist.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        );
    }
    let srv = TestServer::start(vec![svc.clone()]).await;
    let base = srv.url("documents", "acct1");
    let h = H { srv, base, svc };

    let r = h.req("GET", "dbs/fixture/colls", &[], "").await;
    let ids: Vec<String> =
        r.json()["DocumentCollections"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&"items".to_string()) && ids.contains(&"multi".to_string()));
    let c = h.req("GET", "dbs/fixture/colls/items", &[], "").await.json();
    assert_eq!(c["partitionKey"]["paths"], json!(["/pk"]));
    assert_eq!(c["defaultTtl"], 100000);
    assert_eq!(c["uniqueKeyPolicy"]["uniqueKeys"][0]["paths"], json!(["/email"]));
    assert_eq!(c["indexingPolicy"]["excludedPaths"][0]["path"], "/\"_etag\"/?");
    let m = h.req("GET", "dbs/fixture/colls/multi", &[], "").await.json();
    assert_eq!(m["partitionKey"]["paths"], json!(["/a", "/b"]));
    assert_eq!(m["partitionKey"]["kind"], "MultiHash");

    // documents, an id with # and ?, a replace that took effect, a delete that stayed
    let r = h.req("GET", "dbs/fixture/colls/items/docs/a", &[(PK, "[\"p1\"]")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["n"], 10);
    assert_eq!(h.req("GET", "dbs/fixture/colls/items/docs/USER%231%3Fx", &[(PK, "[\"p1\"]")], "").await.json()["email"], "b@x");
    assert_eq!(h.req("GET", "dbs/fixture/colls/items/docs/gone", &[(PK, "[\"p2\"]")], "").await.status, 404);
    let q = h
        .query("fixture", "items", &[("x-ms-documentdb-query-enablecrosspartition", "True")], json!({"query": "SELECT c.id FROM c"}))
        .await;
    assert_eq!(q.status, 200, "{}", q.text());
    let mut got: Vec<_> = q.json()["Documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    got.sort();
    assert_eq!(got, ["USER#1?x", "a", "c"]);
    assert_eq!(h.req("GET", "dbs/fixture/colls/multi/docs/m1", &[(PK, "[\"x\",\"y\"]")], "").await.status, 200);
    // a unique key still holds
    let dup = h.doc("fixture", "items", &json!("p1"), &json!({"id": "other", "pk": "p1", "email": "a@x"})).await;
    assert_eq!(dup.status, 409, "{}", dup.text());
    // scripts, user, permission
    let r = h.json("POST", "dbs/fixture/colls/items/sprocs/hello", &[(PK, "[\"p1\"]")], &json!([])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!("hi"));
    assert_eq!(h.req("GET", "dbs/fixture/colls/items/triggers/pre", &[], "").await.json()["triggerType"], "Pre");
    assert_eq!(h.req("GET", "dbs/fixture/colls/items/udfs/double", &[], "").await.status, 200);
    assert_eq!(h.req("GET", "dbs/fixture/users/u1/permissions/p1", &[], "").await.json()["permissionMode"], "Read");
    // the change feed knows the writes
    let r = h.req("GET", "dbs/fixture/colls/items/docs", &[("A-IM", "Incremental Feed")], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["Documents"].as_array().unwrap().len(), 3);
    // new writes continue the resource ids and the LSN
    let r = h.doc("fixture", "items", &json!("p2"), &json!({"id": "after", "pk": "p2", "email": "z@x"})).await;
    assert_eq!(r.status, 201);
    let rids: Vec<String> = h.req("GET", "dbs/fixture/colls/items/docs", &[], "").await.json()["Documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["_rid"].as_str().unwrap().to_string())
        .collect();
    let mut unique = rids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), rids.len(), "a new document does not reuse a resource id: {rids:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
