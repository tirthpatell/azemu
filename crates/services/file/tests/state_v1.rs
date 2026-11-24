//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/file/file.state` (`AZEMU-STATE file 1`) is what `Service::save` wrote for the small fixed state built by
//! `fixed_state`. The file is checked in and is never rewritten by a test run. `version_1_files_still_load` copies it to a scratch
//! directory, loads it into a new service and checks what it holds, through the requests a client would send.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in file and do not
//! weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (reading into the same state this test
//! expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-file --test state_v1 -- --ignored write_the_fixture`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_file::FileService;
use common::*;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("file")
}

async fn start(svc: Arc<FileService>) -> F {
    let srv = TestServer::start(vec![svc as Arc<dyn Service>]).await;
    let base = srv.url("file.core", ACCOUNT);
    F { srv, base }
}

async fn fixed_state(svc: Arc<FileService>) -> F {
    let f = start(svc).await;
    f.send("PUT", "/work?restype=share", &[("x-ms-meta-owner", "alice"), ("x-ms-share-quota", "9")], "").await;
    f.dir("work/docs").await;
    f.send(
        "PUT",
        "/work/docs/a.txt",
        &[("x-ms-type", "file"), ("x-ms-content-length", "700"), ("x-ms-content-type", "text/plain"), ("x-ms-meta-m", "1")],
        "",
    )
    .await;
    f.write("work/docs/a.txt", 0, "hello").await;
    f.write("work/docs/a.txt", 600, "tail").await;
    f.send("PUT", "/work?restype=share&comp=snapshot", &[], "").await;
    let acl = "<SignedIdentifiers><SignedIdentifier><Id>p1</Id><AccessPolicy><Permission>rl</Permission><Expiry>2099-01-01T00:00:00Z</Expiry></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    f.send("PUT", "/work?restype=share&comp=acl", &[], acl).await;
    f.send("PUT", "/?restype=service&comp=properties", &[], "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>https://example.com</AllowedOrigins><AllowedMethods>GET</AllowedMethods><AllowedHeaders>x-ms-meta-*</AllowedHeaders><ExposedHeaders>x-ms-request-id</ExposedHeaders><MaxAgeInSeconds>60</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>").await;
    f
}

#[tokio::test]
#[ignore = "writes the fixture; run it by hand when the state itself changes"]
async fn write_the_fixture() {
    let svc = Arc::new(FileService::new());
    let _f = fixed_state(svc.clone()).await;
    std::fs::create_dir_all(fixture_dir()).unwrap();
    svc.save(&fixture_dir()).unwrap();
}

#[tokio::test]
async fn version_1_files_still_load() {
    let scratch = std::env::temp_dir().join(format!("azemu-file-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::copy(fixture_dir().join("file.state"), scratch.join("file.state")).unwrap();
    let svc = Arc::new(FileService::new());
    svc.load(&scratch).unwrap();
    let f = start(svc).await;
    let p = f.send("GET", "/work?restype=share", &[], "").await;
    assert_eq!((p.header("x-ms-share-quota"), p.header("x-ms-meta-owner")), (Some("9"), Some("alice")));
    let h = f.send("HEAD", "/work/docs/a.txt", &[], "").await;
    assert_eq!(
        (h.header("content-length"), h.header("content-type"), h.header("x-ms-meta-m")),
        (Some("700"), Some("text/plain"), Some("1"))
    );
    let text = f.read("work/docs/a.txt").await;
    assert!(text.starts_with("hello") && &text[600..604] == "tail");
    // two pages that touch are one range, cut at the size
    let ranges = f.send("GET", "/work/docs/a.txt?comp=rangelist", &[], "").await.text();
    assert_eq!((all(&ranges, "Start"), all(&ranges, "End")), (vec!["0".to_string()], vec!["699".to_string()]));
    assert_eq!(all(&f.send("GET", "/?comp=list&include=snapshots", &[], "").await.text(), "Name").len(), 2);
    assert!(f.send("GET", "/work?restype=share&comp=acl", &[], "").await.text().contains("<Id>p1</Id>"));
    assert!(f.send("GET", "/?restype=service&comp=properties", &[], "").await.text().contains("https://example.com"));
    let _ = std::fs::remove_dir_all(&scratch);
}
