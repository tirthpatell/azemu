//! App Configuration state across restarts (`--data-dir`).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use azemu_appconfig::AppConfigService;
use azemu_core::persist;
use azemu_core::testing::TestServer;
use azemu_core::Service;
use common::*;
use serde_json::json;

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-appconfig-persist-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn open(d: &Path) -> T {
    let srv = TestServer::start_persistent(vec![Arc::new(AppConfigService::new())], d).await;
    let base = srv.url("azconfig", "teststore");
    T { srv, base }
}

async fn try_open(d: &Path) -> anyhow::Result<T> {
    let srv = TestServer::try_start_persistent(vec![Arc::new(AppConfigService::new())], d, |b| b).await?;
    let base = srv.url("azconfig", "teststore");
    Ok(T { srv, base })
}

async fn snapshot(t: &T, name: &str, body: serde_json::Value) {
    let r = t.call("PUT", &format!("/snapshots/{name}"), &[("Content-Type", "application/json")], &body.to_string()).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

/// Everything a client can read from every store.
async fn dump(t: &T) -> String {
    let mut out = String::new();
    for store in ["teststore", "other"] {
        let base = t.base_of(store);
        for path in ["/kv", "/revisions", "/labels", "/snapshots", "/keys"] {
            let sep = if path.contains('?') { '&' } else { '?' };
            let url = format!("{base}{path}{sep}api-version={VERSION}");
            let r = t.srv.send("GET", &url, &[AUTH], "").await;
            out.push_str(&format!("=== {store}{path}\n{}\n{}\n", r.status, r.text()));
            if path == "/snapshots" {
                for s in r.json()["items"].as_array().cloned().unwrap_or_default() {
                    let name = s["name"].as_str().unwrap();
                    for sub in [format!("/snapshots/{name}"), format!("/kv?snapshot={name}")] {
                        let sep = if sub.contains('?') { '&' } else { '?' };
                        let r = t.srv.send("GET", &format!("{base}{sub}{sep}api-version={VERSION}"), &[AUTH], "").await;
                        out.push_str(&format!("--- {sub}\n{}\n{}\n", r.status, r.text()));
                    }
                }
            }
        }
        // point in time reads and the headers of single reads
        let r = t.srv.send("GET", &format!("{base}/kv/app%2Fa?api-version={VERSION}"), &[AUTH], "").await;
        out.push_str(&format!("--- single {}\n{}\n{}\n", store, r.status, r.header("etag").unwrap_or("")));
    }
    out.replace(&format!(":{}", t.srv.port()), ":PORT")
}

#[tokio::test]
async fn key_values_labels_revisions_locks_snapshots_and_etags_survive() {
    let d = dir("rich");
    let t = open(&d).await;
    t.put("app/a", None, json!({"value": "a1", "content_type": "text/x", "tags": {"t": "1"}})).await;
    t.put("app/a", None, json!({"value": "a2", "tags": {"t": "2", "u": "3"}})).await;
    t.put("app/b", Some("prod"), json!({"value": "b1"})).await;
    t.put("app/b", Some("dev"), json!({"value": "b-dev"})).await;
    t.put("app/gone", None, json!({"value": "x"})).await;
    t.delete("app/gone", None).await;
    t.put("locked", Some("L"), json!({"value": "keep"})).await;
    assert_eq!(t.call("PUT", "/locks/locked?label=L", &[], "").await.status, 200);
    t.put("unlocked-again", None, json!({"value": "u"})).await;
    t.call("PUT", "/locks/unlocked-again", &[], "").await;
    t.call("DELETE", "/locks/unlocked-again", &[], "").await;
    t.put("日本語", Some("ラベル"), json!({"value": "unicode"})).await;
    snapshot(&t, "snap1", json!({"filters": [{"key": "app/*"}], "retention_period": 3600, "tags": {"s": "1"}})).await;
    // a setting changed after the snapshot: the snapshot keeps the old one
    t.put("app/a", None, json!({"value": "a3-after-snapshot"})).await;
    snapshot(&t, "snap2", json!({"filters": [{"key": "app/b", "label": "prod"}], "composition_type": "key"})).await;
    assert_eq!(t.call("PATCH", "/snapshots/snap2", &[("Content-Type", "application/json")], r#"{"status":"archived"}"#).await.status, 200);
    // another store
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

    let etag = t.get("/kv/app%2Fa").await.header("etag").unwrap().to_string();
    let before = dump(&t).await;
    assert!(before.contains("a3-after-snapshot") && before.contains("snap2") && before.contains("日本語"), "{before}");
    t.srv.shutdown().await.unwrap();

    let t = open(&d).await;
    // the Sync-Token counter is a thing of the run, not state: it starts again
    let first = t.get("/kv/app%2Fa").await;
    let token = first.header("sync-token").unwrap_or_default().to_string();
    assert!(token.ends_with(";sn=1"), "{token}");
    let after = dump(&t).await;
    if before != after {
        let at = before.bytes().zip(after.bytes()).position(|(x, y)| x != y).unwrap_or(before.len().min(after.len()));
        let window = |s: &str| s[at.saturating_sub(300)..(at + 500).min(s.len())].to_string();
        panic!("state differs after a restart at byte {at}:\nbefore: {}\nafter:  {}", window(&before), window(&after));
    }
    // ETags work as conditions across the restart
    let r = t.call("PUT", "/kv/app%2Fa", &[("Content-Type", "application/json"), ("If-Match", &etag)], r#"{"value":"a4"}"#).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = t.call("PUT", "/kv/app%2Fa", &[("Content-Type", "application/json"), ("If-Match", &etag)], r#"{"value":"a5"}"#).await;
    assert_eq!(r.status, 412);
    // the lock still blocks, the unlocked one does not
    assert_eq!(t.put("locked", Some("L"), json!({"value": "no"})).await.status, 409);
    assert_eq!(t.put("unlocked-again", None, json!({"value": "yes"})).await.status, 200);
    // history goes on where it was, a deleted key can be written again, snapshots are still readable
    let revs = t.get("/revisions?key=app%2Fa").await.json()["items"].as_array().unwrap().len();
    assert_eq!(revs, 4);
    assert_eq!(t.put("app/gone", None, json!({"value": "back"})).await.status, 200);
    assert_eq!(
        t.get("/kv?snapshot=snap1").await.json()["items"].as_array().unwrap().iter().find(|i| i["key"] == "app/a").unwrap()["value"],
        "a2"
    );
    t.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn unchanged_state_is_not_saved_again_and_reads_are_not_changes() {
    let d = dir("unchanged");
    let svc = Arc::new(AppConfigService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &d).await;
    let t = T { base: srv.url("azconfig", "teststore"), srv };
    t.put_value("k", None, "v").await;
    let state_dir = d.join("appconfig");
    svc.save(&state_dir).unwrap();
    let file = state_dir.join("appconfig.state");
    let first = (std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap());
    let g = svc.persist_generation();
    std::thread::sleep(Duration::from_millis(30));
    svc.save(&state_dir).unwrap();
    assert_eq!((std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap()), first);
    t.get("/kv").await;
    t.get("/kv/k").await;
    t.get("/revisions").await;
    t.get("/labels").await;
    assert_eq!(svc.persist_generation(), g, "reads are not changes");
    t.put_value("k", None, "v2").await;
    assert_ne!(svc.persist_generation(), g);
    t.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

fn flip_byte(path: &Path, at: usize) {
    let mut b = std::fs::read(path).unwrap();
    let at = at.min(b.len() - 1);
    b[at] ^= 0xff;
    std::fs::write(path, b).unwrap();
}

#[tokio::test]
async fn damaged_state_files_are_refused_with_a_message_that_names_the_file() {
    let d = dir("damage");
    let t = open(&d).await;
    t.put("k", Some("l"), json!({"value": "some text"})).await;
    t.put("k", Some("l"), json!({"value": "newer"})).await;
    snapshot(&t, "snap", json!({"filters": [{"key": "*"}]})).await;
    t.srv.shutdown().await.unwrap();
    let file = d.join("appconfig").join("appconfig.state");
    let good = std::fs::read(&file).unwrap();
    let refused = |d: PathBuf| async move {
        match try_open(&d).await {
            Ok(t) => {
                t.srv.shutdown().await.ok();
                None
            }
            Err(e) => Some(e.to_string()),
        }
    };
    for cut in [0usize, 5, 21, good.len() / 2, good.len() - 1] {
        std::fs::write(&file, &good[..cut]).unwrap();
        let e = refused(d.clone()).await.unwrap_or_else(|| panic!("cut at {cut} was accepted"));
        assert!(e.contains("appconfig.state") && e.contains("--data-discard-unreadable"), "{e}");
    }
    let mut accepted = 0;
    for at in 0..good.len() {
        std::fs::write(&file, &good).unwrap();
        flip_byte(&file, at);
        if let Some(e) = refused(d.clone()).await {
            assert!(e.contains("appconfig.state"), "flip at {at}: {e}");
        } else {
            accepted += 1;
            std::fs::write(&file, &good).unwrap();
        }
    }
    assert!(accepted < good.len() / 2, "{accepted} of {} flips were accepted", good.len());
    std::fs::write(&file, persist::encode("queue", 1, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("holds queue state, expected appconfig"));
    std::fs::write(&file, persist::encode("appconfig", 99, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("version 99"));
    std::fs::write(&file, b"AZEMU-STATE appconfig 1\n\xc1\xc1").unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("appconfig.state"));
    std::fs::write(&file, &good).unwrap();
    std::fs::write(d.join("appconfig").join("notes.txt"), "mine").unwrap();
    let t = try_open(&d).await.expect("extra files are ignored");
    assert_eq!(t.get("/kv/k?label=l").await.json()["value"], "newer");
    t.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn reset_then_save_leaves_nothing() {
    let d = dir("reset");
    let t = open(&d).await;
    t.put_value("k", None, "v").await;
    t.srv.save_now().await;
    t.srv.reset();
    t.srv.save_now().await;
    drop(t);
    let t = open(&d).await;
    assert_eq!(t.get("/kv/k").await.status, 404);
    t.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

/// While a big store is being saved, other requests are served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_does_not_make_requests_wait_for_the_encoding_and_the_writing() {
    let d = dir("lock");
    let svc = Arc::new(AppConfigService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &d).await;
    let t = T { base: srv.url("azconfig", "teststore"), srv };
    let value = "v".repeat(3000);
    for i in 0..6000 {
        let r = t.put_value(&format!("key-{i}"), None, &value).await;
        assert_eq!(r.status, 200);
    }
    let state_dir = d.join("appconfig");
    let saver = {
        let svc = svc.clone();
        let state_dir = state_dir.clone();
        tokio::task::spawn_blocking(move || {
            let t = std::time::Instant::now();
            svc.save(&state_dir).unwrap();
            t.elapsed()
        })
    };
    let mut slowest = Duration::ZERO;
    let mut served = 0;
    while !saver.is_finished() {
        let s = std::time::Instant::now();
        let r = t.get("/kv/key-3").await;
        slowest = slowest.max(s.elapsed());
        assert_eq!(r.status, 200);
        served += 1;
    }
    let took = saver.await.unwrap();
    assert!(served >= 3, "only {served} requests were served during a save of {took:?}");
    assert!(slowest < took / 2, "a request waited {slowest:?} during a save of {took:?}");
    t.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}
