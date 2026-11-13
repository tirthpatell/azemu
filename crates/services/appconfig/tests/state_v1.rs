//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/appconfig/appconfig.state` (`AZEMU-STATE appconfig 1`) is what `Service::save` wrote for the small fixed
//! state built by `fixed_state`. The file is checked in and is never rewritten by a test run. `version_1_files_still_load`
//! copies it to a scratch directory, loads it into a new service and checks what it holds, through the requests a client
//! would send.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in file and do
//! not weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (reading into the same state this
//! test expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-appconfig --test state_v1 -- --ignored write_the_fixture`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_appconfig::AppConfigService;
use azemu_core::testing::TestServer;
use azemu_core::Service;
use common::*;
use serde_json::json;

mod common;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("appconfig")
}

async fn start(svc: Arc<AppConfigService>) -> T {
    let srv = TestServer::start(vec![svc as Arc<dyn Service>]).await;
    let base = srv.url("azconfig", "fixture");
    T { srv, base }
}

async fn fixed_state(svc: Arc<AppConfigService>) -> T {
    let t = start(svc).await;
    t.put("app/a", None, json!({"value": "a1", "content_type": "text/x", "tags": {"t": "1"}})).await;
    t.put("app/a", None, json!({"value": "a2", "tags": {"t": "2", "u": "3"}})).await;
    t.put("app/b", Some("prod"), json!({"value": "b1"})).await;
    t.put("app/gone", None, json!({"value": "x"})).await;
    t.delete("app/gone", None).await;
    t.put("locked", Some("L"), json!({"value": "keep"})).await;
    assert_eq!(t.call("PUT", "/locks/locked?label=L", &[], "").await.status, 200);
    // retention far in the future, so the fixture does not go stale
    let r = t
        .call(
            "PUT",
            "/snapshots/snap1",
            &[("Content-Type", "application/json")],
            r#"{"filters":[{"key":"app/*"}],"retention_period":604800}"#,
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    t.put("app/a", None, json!({"value": "a3-after-snapshot"})).await;
    let other = t.base_of("other");
    let r = t
        .srv
        .send(
            "PUT",
            &format!("{other}/kv/app%2Fa?api-version={VERSION}"),
            &[AUTH, ("Content-Type", "application/json")],
            r#"{"value":"in the other store"}"#,
        )
        .await;
    assert_eq!(r.status, 200);
    t
}

#[tokio::test]
#[ignore = "writes the checked-in fixture; see the module text"]
async fn write_the_fixture() {
    let svc = Arc::new(AppConfigService::new());
    let _t = fixed_state(svc.clone()).await;
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    svc.save(&dir).unwrap();
}

#[tokio::test]
async fn version_1_files_still_load() {
    let dir = std::env::temp_dir().join(format!("azemu-appconfig-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture_dir().join("appconfig.state"), dir.join("appconfig.state"))
        .expect("tests/state-v1/appconfig/appconfig.state is checked in");

    let svc = Arc::new(AppConfigService::new());
    if let Err(e) = svc.load(&dir) {
        panic!(
            "Version 1 App Configuration state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/persist.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        );
    }
    let t = start(svc).await;
    // current settings
    let kv = t.get("/kv").await;
    assert_eq!(kv.status, 200, "{}", kv.text());
    let mut keys = keys_of(&kv);
    keys.sort();
    assert_eq!(keys, vec!["app/a", "app/b", "locked"]);
    let a = t.get("/kv/app%2Fa").await.json();
    assert_eq!(a["value"], "a3-after-snapshot");
    assert!(t.get("/kv/app%2Fa").await.header("etag").is_some());
    let locked = t.get("/kv/locked?label=L").await.json();
    assert_eq!(locked["locked"], true);
    // the history, with the deleted setting's revisions
    let revs = t.get("/revisions?key=app%2Fa").await.json();
    assert_eq!(revs["items"].as_array().unwrap().len(), 3);
    assert!(revs["items"].as_array().unwrap().iter().any(|r| r["tags"]["u"] == "3" && r["value"] == "a2"), "{revs}");
    assert!(revs["items"].as_array().unwrap().iter().any(|r| r["content_type"] == "text/x" && r["value"] == "a1"), "{revs}");
    assert!(!t.get("/revisions?key=app%2Fgone").await.json()["items"].as_array().unwrap().is_empty());
    // the snapshot keeps what the settings were when it was taken
    let snap = t.get("/snapshots/snap1").await.json();
    assert_eq!(snap["status"], "ready");
    let inside = t.get("/kv?snapshot=snap1").await.json();
    let values: Vec<&str> = inside["items"].as_array().unwrap().iter().map(|i| i["value"].as_str().unwrap()).collect();
    assert!(values.contains(&"a2") && !values.contains(&"a3-after-snapshot"), "{values:?}");
    // the other store
    let other = t.base_of("other");
    let r = t.srv.send("GET", &format!("{other}/kv/app%2Fa?api-version={VERSION}"), &[AUTH], "").await;
    assert_eq!(r.json()["value"], "in the other store");
    let _ = std::fs::remove_dir_all(&dir);
}
