//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/arm/` holds what `Service::save` wrote (`AZEMU-STATE arm 1` for `arm.state` and
//! `AZEMU-STATE arm-resources 1` for the resources of each group or subscription scope) for the small state built by
//! `build_fixed_state`. The files are checked in and the test run never rewrites them. `version_1_files_still_load`
//! loads them into a new service and reads back through the REST API what they hold.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in files
//! and do not weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (reading it into the
//! same state this test expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-arm --test state_v1 -- --ignored write_the_fixture`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_arm::{ArmService, Backends};
use azemu_core::testing::TestServer;
use azemu_core::Service;
use common::{Arm, SUB};
use serde_json::json;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("arm")
}

async fn build_fixed_state() -> (Arm, Arc<ArmService>) {
    let svc = Arc::new(ArmService::with_strict(Backends::none(), false));
    let a = Arm { srv: TestServer::start(vec![svc.clone()]).await };
    let r = a
        .put(
            &format!("/subscriptions/{SUB}/resourcegroups/fixture-rg"),
            json!({"location": "westeurope", "tags": {"team": "x", "env": "dev"}, "managedBy": "me"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    a.group("second-rg").await;
    let r = a
        .put(
            &format!("/subscriptions/{SUB}/resourceGroups/fixture-rg/providers/Microsoft.Foo/things/t1"),
            json!({"location": "eastus", "tags": {"k": "v"}, "properties": {"size": 3, "list": ["a", "b"]}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = a
        .put(
            &format!("/subscriptions/{SUB}/resourceGroups/fixture-rg/providers/Microsoft.Foo/things/t1/parts/p1"),
            json!({"properties": {"n": 1}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    // tags on a scope and a subscription-wide tag name
    let scope = format!("/subscriptions/{SUB}/resourceGroups/second-rg/providers/Microsoft.Resources/tags/default");
    assert_eq!(a.put(&scope, json!({"properties": {"tags": {"x": "1"}}})).await.status, 200);
    a.put(&format!("/subscriptions/{SUB}/tagNames/costcenter"), json!({})).await;
    // a deployment whose output holds an address with the port of this run (stored without it)
    let port = a.srv.port();
    let template = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [],
        "outputs": {"where": {"type": "string", "value": format!("[concat('https://acct.blob.core.localhost:', '{port}', '/c')]")}}
    });
    let r = a
        .put(
            &format!("/subscriptions/{SUB}/resourceGroups/fixture-rg/providers/Microsoft.Resources/deployments/d1"),
            json!({"properties": {"mode": "Incremental", "template": template}}),
        )
        .await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text());
    assert!(r.text().contains(&format!("acct.blob.core.localhost:{port}/c")), "{}", r.text());
    (a, svc)
}

#[tokio::test]
#[ignore = "writes the checked-in fixture; see the module text"]
async fn write_the_fixture() {
    let (_a, svc) = build_fixed_state().await;
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    svc.save(&dir).unwrap();
}

#[tokio::test]
async fn version_1_files_still_load() {
    let dir = std::env::temp_dir().join(format!("azemu-arm-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut copied = 0;
    for entry in std::fs::read_dir(fixture_dir()).expect("tests/state-v1/arm is checked in") {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
        copied += 1;
    }
    assert!(copied >= 2, "the fixture has the definitions file and one file for the group with resources ({copied} files found)");

    let svc = Arc::new(ArmService::with_strict(Backends::none(), false));
    if let Err(e) = svc.load(&dir) {
        panic!(
            "Version 1 ARM state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/persist.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        );
    }
    let a = Arm { srv: TestServer::start(vec![svc]).await };

    let g = a.get(&format!("/subscriptions/{SUB}/resourcegroups/fixture-rg")).await.json();
    assert_eq!(g["location"], "westeurope");
    assert_eq!(g["tags"], json!({"team": "x", "env": "dev"}));
    assert_eq!(g["managedBy"], "me");
    assert_eq!(a.get(&format!("/subscriptions/{SUB}/resourcegroups/second-rg")).await.status, 200);
    let t = a.get(&format!("/subscriptions/{SUB}/resourceGroups/fixture-rg/providers/Microsoft.Foo/things/t1")).await.json();
    assert_eq!(t["location"], "eastus");
    assert_eq!(t["tags"], json!({"k": "v"}));
    assert_eq!(t["properties"]["size"], 3);
    assert_eq!(t["properties"]["list"], json!(["a", "b"]));
    assert_eq!(t["properties"]["provisioningState"], "Succeeded");
    let p = a.get(&format!("/subscriptions/{SUB}/resourceGroups/fixture-rg/providers/Microsoft.Foo/things/t1/parts/p1")).await.json();
    assert_eq!(p["properties"]["n"], 1);
    let scope = format!("/subscriptions/{SUB}/resourceGroups/second-rg/providers/Microsoft.Resources/tags/default");
    assert_eq!(a.get(&scope).await.json()["properties"]["tags"], json!({"x": "1"}));
    let names = a.get(&format!("/subscriptions/{SUB}/tagNames")).await.json();
    assert!(names["value"].as_array().unwrap().iter().any(|n| n["tagName"] == "costcenter"), "{names}");
    // the deployment, and its output with the port of this run, not of the run that wrote the fixture
    let d = a.get(&format!("/subscriptions/{SUB}/resourceGroups/fixture-rg/providers/Microsoft.Resources/deployments/d1")).await;
    assert_eq!(d.status, 200, "{}", d.text());
    assert_eq!(d.json()["properties"]["provisioningState"], "Succeeded");
    assert_eq!(d.json()["properties"]["outputs"]["where"]["value"], format!("https://acct.blob.core.localhost:{}/c", a.srv.port()));
    let list = a.get(&format!("/subscriptions/{SUB}/resourceGroups/fixture-rg/resources")).await.json();
    assert!(list["value"].as_array().unwrap().iter().any(|r| r["name"] == "t1"), "{list}");
    let _ = std::fs::remove_dir_all(&dir);
}
