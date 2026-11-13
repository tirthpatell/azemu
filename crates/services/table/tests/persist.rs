//! Table state across restarts (`--data-dir`).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use azemu_core::persist;
use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_table::TableService;
use common::*;

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-table-persist-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn open(d: &Path) -> Env {
    let srv = TestServer::start_persistent(vec![Arc::new(TableService::new())], d).await;
    let base = srv.url("table", "acct");
    Env { srv, base }
}

async fn try_open(d: &Path) -> anyhow::Result<Env> {
    let srv = TestServer::try_start_persistent(vec![Arc::new(TableService::new())], d, |b| b).await?;
    let base = srv.url("table", "acct");
    Ok(Env { srv, base })
}

const FULL: (&str, &str) = ("Accept", "application/json;odata=fullmetadata");

/// Everything a client can see of the account.
async fn dump(e: &Env) -> String {
    let mut out = String::new();
    let mut add = |what: &str, r: azemu_core::testing::TestResponse| {
        let mut h: Vec<String> = r
            .headers
            .iter()
            .filter(|(k, _)| matches!(k.as_str(), "etag" | "content-type" | "location"))
            .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap_or("")))
            .collect();
        h.sort();
        out.push_str(&format!("=== {what}\n{}\n{}\n{}\n", r.status, h.join("\n"), r.text()));
    };
    let tables = e.send("GET", "/Tables", &[FULL], "").await;
    let names: Vec<String> =
        tables.json()["value"].as_array().unwrap().iter().map(|t| t["TableName"].as_str().unwrap().to_string()).collect();
    add("tables", tables);
    add("props", e.send("GET", "/?restype=service&comp=properties", &[], "").await);
    for n in names {
        add(&format!("{n} acl"), e.send("GET", &format!("/{n}?comp=acl"), &[], "").await);
        add(&format!("{n} all"), e.send("GET", &format!("/{n}()"), &[FULL], "").await);
    }
    out.replace(&format!(":{}", e.srv.port()), ":PORT")
}

#[tokio::test]
async fn tables_entities_etags_types_and_policies_survive() {
    let d = dir("rich");
    let e = open(&d).await;
    assert_eq!(e.create_table("People").await.status, 201);
    assert_eq!(e.create_table("empty").await.status, 201);
    let full = r#"{"PartitionKey":"pk 1","RowKey":"rk-1","Name":"Zoë 😀","Age":42,"Big@odata.type":"Edm.Int64","Big":"9007199254740993","Pi":3.14159,"Ok":true,
        "When@odata.type":"Edm.DateTime","When":"2024-05-06T07:08:09.1234567Z","Id@odata.type":"Edm.Guid","Id":"6ba7b810-9dad-11d1-80b4-00c04fd430c8",
        "Bin@odata.type":"Edm.Binary","Bin":"AAECAwQFBgcI/w=="}"#;
    let r = e.insert("People", full).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(e.insert("People", r#"{"PartitionKey":"pk 1","RowKey":"rk2","v":1}"#).await.status, 201);
    assert_eq!(e.insert("People", r#"{"PartitionKey":"pk2","RowKey":"a","v":1}"#).await.status, 201);
    // a merge and a replace change the timestamp (and so the ETag)
    let r = e.send("MERGE", "/People(PartitionKey='pk%201',RowKey='rk2')", &[JSON, ("If-Match", "*")], r#"{"added":"x"}"#).await;
    assert_eq!(r.status, 204, "{}", r.text());
    let r = e.send("PUT", "/People(PartitionKey='pk2',RowKey='a')", &[JSON, ("If-Match", "*")], r#"{"replaced":2}"#).await;
    assert_eq!(r.status, 204, "{}", r.text());
    // a deleted entity stays deleted
    assert_eq!(e.insert("People", r#"{"PartitionKey":"pk2","RowKey":"gone"}"#).await.status, 201);
    assert_eq!(e.send("DELETE", "/People(PartitionKey='pk2',RowKey='gone')", &[("If-Match", "*")], "").await.status, 204);
    let acl = "<SignedIdentifiers><SignedIdentifier><Id>reader</Id><AccessPolicy><Start>2024-01-01T00:00:00Z</Start><Expiry>2099-01-01T00:00:00Z</Expiry><Permission>r</Permission></AccessPolicy></SignedIdentifier><SignedIdentifier><Id>bare</Id></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(e.send("PUT", "/People?comp=acl", &[], acl).await.status, 204);
    let props = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>https://example.com</AllowedOrigins><AllowedMethods>GET,PUT</AllowedMethods><AllowedHeaders>x-ms-meta-*</AllowedHeaders><ExposedHeaders>x-ms-request-id</ExposedHeaders><MaxAgeInSeconds>60</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
    assert_eq!(e.send("PUT", "/?restype=service&comp=properties", &[], props).await.status, 202);
    // an entity group transaction
    let one = r#"{"PartitionKey":"batch","RowKey":"1","b":true}"#;
    let body = batch(&[("POST", "/People", &[], one)]);
    let r = e.send("POST", "/$batch", &[BATCH_CT], &body).await;
    assert_eq!(r.status, 202, "{}", r.text());
    let before = dump(&e).await;
    assert!(before.contains("9007199254740993") && before.contains("Zo") && before.contains("batch"));
    e.srv.shutdown().await.unwrap();

    let e = open(&d).await;
    let after = dump(&e).await;
    assert_eq!(before, after);
    // conditional writes use the ETag from before the restart
    let etag = e.get("People", "pk%201", "rk2").await.header("etag").unwrap().to_string();
    let r = e.send("MERGE", "/People(PartitionKey='pk%201',RowKey='rk2')", &[JSON, ("If-Match", &etag)], r#"{"again":1}"#).await;
    assert_eq!(r.status, 204);
    let r = e.send("MERGE", "/People(PartitionKey='pk%201',RowKey='rk2')", &[JSON, ("If-Match", &etag)], r#"{"again":2}"#).await;
    assert_eq!(r.status, 412);
    // a table that was removed stays removed
    assert_eq!(e.send("DELETE", "/Tables('empty')", &[], "").await.status, 204);
    e.srv.shutdown().await.unwrap();
    let e = open(&d).await;
    assert_eq!(e.send("GET", "/empty()", &[], "").await.status, 404);
    assert_eq!(e.get("People", "pk%201", "rk2").await.json()["again"], 1);
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn unchanged_state_is_not_saved_again_and_reads_are_not_changes() {
    let d = dir("unchanged");
    let svc = Arc::new(TableService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &d).await;
    let e = Env { base: srv.url("table", "acct"), srv };
    e.create_table("tbl").await;
    e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r"}"#).await;
    let state_dir = d.join("table");
    svc.save(&state_dir).unwrap();
    let file = state_dir.join("table.state");
    let first = (std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap());
    let g = svc.persist_generation();
    std::thread::sleep(Duration::from_millis(30));
    svc.save(&state_dir).unwrap();
    assert_eq!((std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap()), first);
    e.get("tbl", "p", "r").await;
    e.send("GET", "/tbl()", &[], "").await;
    e.send("GET", "/Tables", &[], "").await;
    assert_eq!(svc.persist_generation(), g, "reads are not changes");
    e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r2"}"#).await;
    assert_ne!(svc.persist_generation(), g);
    e.srv.shutdown().await.unwrap();
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
    let e = open(&d).await;
    e.create_table("tbl").await;
    e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r","v":"some text"}"#).await;
    e.srv.shutdown().await.unwrap();
    let file = d.join("table").join("table.state");
    let good = std::fs::read(&file).unwrap();
    let refused = |d: PathBuf| async move {
        match try_open(&d).await {
            Ok(e) => {
                e.srv.shutdown().await.ok();
                None
            }
            Err(e) => Some(e.to_string()),
        }
    };
    for cut in [0usize, 5, 21, good.len() / 2, good.len() - 1] {
        std::fs::write(&file, &good[..cut]).unwrap();
        let e = refused(d.clone()).await.unwrap_or_else(|| panic!("cut at {cut} was accepted"));
        assert!(e.contains("table.state") && e.contains("--data-discard-unreadable"), "{e}");
    }
    let mut accepted = 0;
    for at in 0..good.len() {
        std::fs::write(&file, &good).unwrap();
        flip_byte(&file, at);
        if let Some(e) = refused(d.clone()).await {
            assert!(e.contains("table.state"), "flip at {at}: {e}");
        } else {
            accepted += 1;
            std::fs::write(&file, &good).unwrap();
        }
    }
    assert!(accepted < good.len() / 2, "{accepted} of {} flips were accepted", good.len());
    std::fs::write(&file, persist::encode("queue", 1, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("holds queue state, expected table"));
    std::fs::write(&file, persist::encode("table", 99, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("version 99"));
    std::fs::write(&file, b"AZEMU-STATE table 1\n\xc1\xc1").unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("table.state"));
    std::fs::write(&file, &good).unwrap();
    std::fs::write(d.join("table").join("notes.txt"), "mine").unwrap();
    let e = try_open(&d).await.expect("extra files are ignored");
    assert_eq!(e.get("tbl", "p", "r").await.status, 200);
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn reset_then_save_leaves_nothing() {
    let d = dir("reset");
    let e = open(&d).await;
    e.create_table("tbl").await;
    e.srv.save_now().await;
    e.srv.reset();
    e.srv.save_now().await;
    drop(e);
    let e = open(&d).await;
    assert!(e.send("GET", "/Tables", &[], "").await.json()["value"].as_array().unwrap().is_empty());
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

/// While a big table is being saved, other requests are served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_does_not_make_requests_wait_for_the_encoding_and_the_writing() {
    let d = dir("lock");
    let svc = Arc::new(TableService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &d).await;
    let e = Env { base: srv.url("table", "acct"), srv };
    e.create_table("tbl").await;
    e.create_table("other").await;
    let text = "x".repeat(1500);
    for i in 0..6000 {
        let r = e.insert("tbl", &format!(r#"{{"PartitionKey":"p{}","RowKey":"{i}","text":"{text}","n":{i}}}"#, i % 10)).await;
        assert_eq!(r.status, 201);
    }
    let state_dir = d.join("table");
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
        let t = std::time::Instant::now();
        let r = e.send("GET", "/other()", &[], "").await;
        slowest = slowest.max(t.elapsed());
        assert_eq!(r.status, 200);
        served += 1;
    }
    let took = saver.await.unwrap();
    assert!(served >= 3, "only {served} requests were served during a save of {took:?}");
    assert!(slowest < took / 2, "a request waited {slowest:?} during a save of {took:?}");
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}
