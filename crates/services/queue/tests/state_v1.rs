//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/queue/queue.state` (`AZEMU-STATE queue 1`) is what `Service::save` wrote for the small fixed state built
//! by `fixed_state`. The file is checked in and is never rewritten by a test run. `version_1_files_still_load` copies it to a
//! scratch directory, loads it into a new service and checks what it holds, through the requests a client would send.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in file and do
//! not weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (reading into the same state this
//! test expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-queue --test state_v1 -- --ignored write_the_fixture`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_queue::QueueService;
use common::*;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("queue")
}

async fn start(svc: Arc<QueueService>) -> Q {
    let srv = TestServer::start(vec![svc as Arc<dyn Service>]).await;
    let base = srv.url("queue", ACCOUNT);
    Q { srv, base }
}

async fn fixed_state(svc: Arc<QueueService>) -> Q {
    let q = start(svc).await;
    assert_eq!(q.send("PUT", "/work", &[("x-ms-meta-owner", "alice")], "").await.status, 201);
    assert_eq!(q.send("PUT", "/empty", &[], "").await.status, 201);
    let acl = "<SignedIdentifiers><SignedIdentifier><Id>p1</Id><AccessPolicy><Start>2024-01-01T00:00:00Z</Start><Expiry>2099-01-01T00:00:00Z</Expiry><Permission>raup</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(q.send("PUT", "/work?comp=acl", &[], acl).await.status, 204);
    let props = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>https://example.com</AllowedOrigins><AllowedMethods>GET,POST</AllowedMethods><AllowedHeaders>x-ms-meta-*</AllowedHeaders><ExposedHeaders>x-ms-request-id</ExposedHeaders><MaxAgeInSeconds>60</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
    assert_eq!(q.send("PUT", "/?restype=service&comp=properties", &[], props).await.status, 202);
    // never expires, so the fixture does not go stale
    assert_eq!(q.put("work", "first", "messagettl=-1").await.status, 201);
    assert_eq!(q.put("work", "second", "messagettl=-1").await.status, 201);
    // one message dequeued once (hidden for a long time, still far in the future when the test reads it)
    let got = q.get("work", "numofmessages=1&visibilitytimeout=604800").await;
    assert_eq!(got.status, 200);
    q
}

#[tokio::test]
#[ignore = "writes the checked-in fixture; see the module text"]
async fn write_the_fixture() {
    let svc = Arc::new(QueueService::new());
    let _q = fixed_state(svc.clone()).await;
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    svc.save(&dir).unwrap();
}

#[tokio::test]
async fn version_1_files_still_load() {
    let dir = std::env::temp_dir().join(format!("azemu-queue-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture_dir().join("queue.state"), dir.join("queue.state")).expect("tests/state-v1/queue/queue.state is checked in");

    let svc = Arc::new(QueueService::new());
    if let Err(e) = svc.load(&dir) {
        panic!(
            "Version 1 queue state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/persist.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        );
    }
    let q = start(svc).await;
    let list = q.send("GET", "/?comp=list&include=metadata", &[], "").await.text();
    assert_eq!(tags(&list, "Name"), vec!["empty", "work"], "{list}");
    let meta = q.send("GET", "/work?comp=metadata", &[], "").await;
    assert_eq!(meta.header("x-ms-meta-owner"), Some("alice"));
    assert_eq!(meta.header("x-ms-approximate-messages-count"), Some("2"));
    // the message that was dequeued is hidden, the other one is visible with its text
    let peek = q.peek("work", "numofmessages=32").await.text();
    assert_eq!(tags(&peek, "MessageText"), vec!["second"], "{peek}");
    let acl = q.send("GET", "/work?comp=acl", &[], "").await.text();
    assert!(acl.contains("<Id>p1</Id>") && acl.contains("<Permission>raup</Permission>"), "{acl}");
    let props = q.send("GET", "/?restype=service&comp=properties", &[], "").await.text();
    assert!(props.contains("https://example.com") && props.contains("<MaxAgeInSeconds>60</MaxAgeInSeconds>"), "{props}");
    assert_eq!(q.peek("empty", "").await.status, 200);
    let _ = std::fs::remove_dir_all(&dir);
}
