//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/blob/` holds what `Service::save` wrote for the small fixed state built by `fixed_state`:
//! `blob.state` (`AZEMU-STATE blob 1`) and the data pieces under `pieces/`. The files are checked in and are never
//! rewritten by a test run. `version_1_files_still_load` copies them to a scratch directory, loads them into a new
//! service and checks what they hold, through the same requests a client would send.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in files
//! and do not weaken the test: raise `VERSION` in `src/disk.rs`, keep a reader for version 1 (reading into the same
//! state this test expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-blob --test state_v1 -- --ignored write_the_fixture`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::testing::{TestResponse, TestServer};
use azemu_core::Service;

const AUTH: (&str, &str) = ("Authorization", "SharedKey fixture:c2ln");

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("blob")
}

fn copy_dir(from: &Path, to: &Path) -> usize {
    std::fs::create_dir_all(to).unwrap();
    let mut n = 0;
    for e in std::fs::read_dir(from).expect("tests/state-v1/blob is checked in") {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            n += copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
            n += 1;
        }
    }
    n
}

struct Rig {
    srv: TestServer,
    blob: String,
    dfs: String,
}

impl Rig {
    async fn start(svc: Arc<BlobService>) -> Rig {
        let lake = svc.datalake();
        let srv = TestServer::start(vec![svc as Arc<dyn Service>, Arc::new(lake)]).await;
        let (blob, dfs) = (srv.url("blob", "fixture"), srv.url("dfs", "fixture"));
        Rig { srv, blob, dfs }
    }

    async fn b(&self, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> TestResponse {
        let mut h = vec![AUTH];
        h.extend_from_slice(extra);
        self.srv.send(method, &format!("{}{path}", self.blob), &h, body.to_string()).await
    }

    async fn ok(&self, status: u16, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> TestResponse {
        let r = self.b(method, path, extra, body).await;
        assert_eq!(r.status, status, "{method} {path}: {}", r.text());
        r
    }
}

/// Builds the fixed state. Everything a client sets that the state file has to keep: properties, metadata, tags, versions,
/// snapshots, an append blob, a legal hold, an immutability policy, a hierarchical namespace file system with a directory.
async fn fixed_state(svc: Arc<BlobService>) -> Rig {
    svc.set_versioning_enabled("fixture", true);
    svc.set_versioning_enabled("plain", false);
    let r = Rig::start(svc).await;
    r.ok(201, "PUT", "/docs?restype=container", &[("x-ms-meta-owner", "team-a")], "").await;
    let block = [("x-ms-blob-type", "BlockBlob")];
    r.ok(
        201,
        "PUT",
        "/docs/hello.txt",
        &[
            block[0],
            ("Content-Type", "text/plain"),
            ("x-ms-blob-cache-control", "no-cache"),
            ("x-ms-meta-kind", "greeting"),
            ("x-ms-tags", "env=test&team=a"),
        ],
        "hello world",
    )
    .await;
    r.ok(201, "PUT", "/docs/hello.txt", &[block[0], ("Content-Type", "text/plain")], "hello again").await;
    r.ok(201, "PUT", "/docs/snap.txt", &block, "before").await;
    r.ok(201, "PUT", "/docs/snap.txt?comp=snapshot", &[], "").await;
    r.ok(201, "PUT", "/docs/log", &[("x-ms-blob-type", "AppendBlob")], "").await;
    r.ok(201, "PUT", "/docs/log?comp=appendblock", &[], "line one\n").await;
    r.ok(201, "PUT", "/docs/log?comp=appendblock", &[], "line two\n").await;
    r.ok(201, "PUT", "/docs/held", &block, "kept").await;
    r.ok(200, "PUT", "/docs/held?comp=legalhold", &[("x-ms-legal-hold", "true")], "").await;
    r.ok(201, "PUT", "/docs/locked", &block, "locked").await;
    r.ok(
        200,
        "PUT",
        "/docs/locked?comp=immutabilityPolicies",
        &[("x-ms-immutability-policy-until-date", "Thu, 04 Apr 2075 00:00:00 GMT"), ("x-ms-immutability-policy-mode", "unlocked")],
        "",
    )
    .await;
    let d = [AUTH];
    assert_eq!(r.srv.send("PUT", &format!("{}/lake?resource=filesystem", r.dfs), &d, "").await.status, 201);
    assert_eq!(r.srv.send("PUT", &format!("{}/lake/dir?resource=directory", r.dfs), &d, "").await.status, 201);
    assert_eq!(r.srv.send("PUT", &format!("{}/lake/dir/file.txt?resource=file", r.dfs), &d, "").await.status, 201);
    r
}

#[tokio::test]
#[ignore = "writes the checked-in fixture; see the module text"]
async fn write_the_fixture() {
    let svc = Arc::new(BlobService::new());
    let _rig = fixed_state(svc.clone()).await;
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    svc.save(&dir).unwrap();
}

#[tokio::test]
async fn version_1_files_still_load() {
    let dir = std::env::temp_dir().join(format!("azemu-blob-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let copied = copy_dir(&fixture_dir(), &dir);
    assert!(copied >= 2, "the fixture has blob.state and at least one piece file ({copied} files found)");

    let svc = Arc::new(BlobService::new());
    if let Err(e) = svc.load(&dir) {
        panic!(
            "Version 1 blob state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/disk.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        );
    }
    // the per-account versioning switch came back
    assert!(svc.versioning_enabled("fixture"));
    assert!(!svc.versioning_enabled("plain"));
    let r = Rig {
        srv: TestServer::start(vec![svc.clone() as Arc<dyn Service>, Arc::new(svc.datalake())]).await,
        blob: String::new(),
        dfs: String::new(),
    };
    let r = Rig { blob: r.srv.url("blob", "fixture"), dfs: r.srv.url("dfs", "fixture"), srv: r.srv };

    // properties, metadata, tags and content of the current version
    let g = r.ok(200, "GET", "/docs/hello.txt", &[], "").await;
    assert_eq!(g.text(), "hello again");
    let g = r.ok(200, "HEAD", "/docs?restype=container", &[], "").await;
    assert_eq!(g.header("x-ms-meta-owner"), Some("team-a"));
    // the first write became a version, with its own content, properties, metadata and tags
    let list = r
        .ok(200, "GET", "/docs?restype=container&comp=list&include=versions,snapshots,metadata,tags,legalhold,immutabilitypolicy", &[], "")
        .await
        .text();
    let versions = tags_of(&list, "VersionId");
    assert!(versions.len() >= 2, "the current version and the old one: {list}");
    let mut found_old = false;
    for v in &versions {
        let g = r.b("GET", &format!("/docs/hello.txt?versionid={v}"), &[], "").await;
        if g.status == 200 && g.text() == "hello world" {
            found_old = true;
            assert_eq!(g.header("content-type"), Some("text/plain"));
            assert_eq!(g.header("cache-control"), Some("no-cache"));
            assert_eq!(g.header("x-ms-meta-kind"), Some("greeting"));
            assert_eq!(g.header("x-ms-tag-count"), Some("2"));
        }
    }
    assert!(found_old, "the old version of hello.txt came back with its content");
    // the snapshot
    let snap = tags_of(&list, "Snapshot");
    assert_eq!(snap.len(), 1, "{list}");
    let g = r.ok(200, "GET", &format!("/docs/snap.txt?snapshot={}", snap[0]), &[], "").await;
    assert_eq!(g.text(), "before");
    // the append blob keeps its blocks
    let g = r.ok(200, "GET", "/docs/log", &[], "").await;
    assert_eq!(g.text(), "line one\nline two\n");
    assert_eq!(g.header("x-ms-blob-committed-block-count"), Some("2"));
    // hold and policy still protect
    assert_eq!(r.b("DELETE", "/docs/held", &[], "").await.header("x-ms-error-code"), Some("BlobImmutableDueToLegalHold"));
    assert_eq!(r.b("DELETE", "/docs/locked", &[], "").await.header("x-ms-error-code"), Some("BlobImmutableDueToPolicy"));
    // the Data Lake side
    let a = [AUTH];
    let p = r.srv.send("HEAD", &format!("{}/lake/dir?action=getStatus", r.dfs), &a, "").await;
    assert_eq!((p.status, p.header("x-ms-resource-type")), (200, Some("directory")));
    let p = r.srv.send("HEAD", &format!("{}/lake/dir/file.txt", r.dfs), &a, "").await;
    assert_eq!((p.status, p.header("x-ms-resource-type")), (200, Some("file")));
    // an account that was only switched
    let _ = std::fs::remove_dir_all(&dir);
}

fn tags_of(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(after[..j].to_string());
        rest = &after[j + close.len()..];
    }
    out
}
