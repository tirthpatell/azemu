//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/eventgrid/` holds the files `Service::save` wrote for the small state `build_the_state` makes
//! (`AZEMU-STATE eventgrid 1` and `AZEMU-STATE eventgrid-sub 1`). The files are checked in and are never rewritten by
//! the test run. `version_1_files_still_load` starts a service on a copy of them and checks what they hold.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in files
//! and do not weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (and read it into the
//! same state this test expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-eventgrid --test state_v1 -- --ignored write_the_fixture`.
//! The times in the files are those of the day the fixture was made; the test never looks at them, and the time to live of
//! the queued events is ten years so that they are still there.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_eventgrid::EventGridService;
use common::*;
use serde_json::json;

const NS: &str = "ns1";

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("eventgrid")
}

async fn build_the_state(t: &Eg) {
    assert_eq!(t.put_json("/topics/orders", &json!({"inputSchema": "EventGridSchema"})).await.status, 201);
    let sub = json!({
        "webhookUrl": "http://127.0.0.1:9/hook",
        "skipValidation": true,
        "filter": {"includedEventTypes": ["Order.Created"], "subjectBeginsWith": "/orders"},
        "retryPolicy": {"maxDeliveryAttempts": 7}
    });
    assert_eq!(t.put_json("/topics/orders/subscriptions/to-hook", &sub).await.status, 201);
    assert_eq!(t.put_json("/topics/dom", &json!({"kind": "domain"})).await.status, 201);
    assert_eq!(t.put_json("/topics/dt?domain=dom", &json!({})).await.status, 201);
    assert_eq!(t.put_json(&format!("/topics/jobs?namespace={NS}"), &json!({})).await.status, 201);
    let cfg = json!({"lockDurationSeconds": 120, "maxDeliveryCount": 3, "eventTimeToLiveSeconds": 315_360_000});
    assert_eq!(t.put_json(&format!("/topics/jobs/subscriptions/w?namespace={NS}"), &cfg).await.status, 201);
    assert_eq!(t.ns_publish(NS, "jobs", json!([ce("e1", "job.made"), ce("e2", "job.made")])).await.status, 200);
}

#[tokio::test]
#[ignore = "writes the checked-in fixture; see the module text"]
async fn write_the_fixture() {
    let data = std::env::temp_dir().join(format!("azemu-eg-state-v1-write-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let svc = Arc::new(EventGridService::with_options(fast_options()));
    let srv = TestServer::start_persistent(vec![svc], &data).await;
    let t = Eg { srv };
    build_the_state(&t).await;
    t.srv.shutdown().await.unwrap();
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for entry in std::fs::read_dir(data.join("eventgrid")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
    }
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test]
async fn version_1_files_still_load() {
    let data = std::env::temp_dir().join(format!("azemu-eg-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let service_dir = data.join("eventgrid");
    std::fs::create_dir_all(&service_dir).unwrap();
    let mut copied = 0;
    for entry in std::fs::read_dir(fixture_dir()).expect("tests/state-v1/eventgrid is checked in") {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), service_dir.join(entry.file_name())).unwrap();
        copied += 1;
    }
    assert!(copied >= 3, "the fixture has the definitions file and one file per subscription ({copied} files found)");

    let svc = Arc::new(EventGridService::with_options(fast_options()));
    let started = TestServer::try_start_persistent(vec![svc], &data, |b| b).await;
    let srv = match started {
        Ok(s) => s,
        Err(e) => panic!(
            "Version 1 Event Grid state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/persist.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        ),
    };
    let t = Eg { srv };

    let r = t.get_admin("/topics/orders").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["inputSchema"], "EventGridSchema");
    let r = t.get_admin("/topics/orders/subscriptions/to-hook").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["webhookUrl"], "http://127.0.0.1:9/hook");
    assert_eq!(v["deliveryMode"], "push");
    assert_eq!(v["filter"]["includedEventTypes"], json!(["Order.Created"]));
    assert_eq!(v["filter"]["subjectBeginsWith"], "/orders");
    assert_eq!(v["retryPolicy"]["maxDeliveryAttempts"], 7);
    assert_eq!(t.get_admin("/topics/dom").await.status, 200);
    assert_eq!(t.get_admin("/topics/dt?domain=dom").await.status, 200);
    let r = t.get_admin(&format!("/topics/jobs/subscriptions/w?namespace={NS}")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["deliveryMode"], "pull");
    assert_eq!(r.json()["maxDeliveryCount"], 3);

    // the two queued events come back in order
    let r = t.receive(NS, "jobs", "w", "maxEvents=10&maxWaitTime=1").await;
    let ids: Vec<String> = r.json()["value"].as_array().unwrap().iter().map(|v| v["event"]["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids, vec!["e1", "e2"]);
    t.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}
