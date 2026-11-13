//! Saving to a data directory and starting again from it (`docs/services/persistence.md`).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_cosmos::CosmosService;
use common::{settings, H};
use serde_json::{json, Value};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-cosmos-persist-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

async fn start_on(data: &Path) -> H {
    let svc = Arc::new(CosmosService::with_script_settings(settings()));
    let srv = TestServer::start_persistent(vec![svc.clone()], data).await;
    let base = srv.url("documents", "acct1");
    H { srv, base, svc }
}

async fn try_start_on(data: &Path) -> anyhow::Result<TestServer> {
    let svc = Arc::new(CosmosService::with_script_settings(settings()));
    TestServer::try_start_persistent(vec![svc], data, |b| b).await
}

fn pkh(p: &str) -> String {
    format!("[\"{p}\"]")
}

fn files(data: &Path) -> Vec<(String, std::time::SystemTime, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut v: Vec<_> = std::fs::read_dir(data.join("cosmos"))
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            (e.file_name().to_string_lossy().into_owned(), m.modified().unwrap(), m.ino())
        })
        .collect();
    v.sort();
    v
}

/// Everything a client can read about the state, as JSON, in a fixed order.
async fn look(h: &H) -> Value {
    let mut out = serde_json::Map::new();
    let mut paths = vec![
        "dbs".to_string(),
        "dbs/d".to_string(),
        "dbs/d/colls".to_string(),
        "dbs/d/colls/c".to_string(),
        "dbs/d/colls/c/docs".to_string(),
        "dbs/d/colls/c/sprocs".to_string(),
        "dbs/d/colls/c/triggers".to_string(),
        "dbs/d/colls/c/udfs".to_string(),
        "dbs/d/colls/c/pkranges".to_string(),
        "dbs/d/users".to_string(),
        "dbs/d/users/u1/permissions".to_string(),
        "dbs/d/users/u1/permissions/p1".to_string(),
        "offers".to_string(),
        "dbs/d/colls/other".to_string(),
    ];
    for id in ["a", "b", "ttl1"] {
        paths.push(format!("dbs/d/colls/c/docs/{id}"));
    }
    for p in paths {
        let hdrs: Vec<(String, String)> = if p.ends_with("/a") || p.ends_with("/b") || p.ends_with("/ttl1") {
            vec![("x-ms-documentdb-partitionkey".into(), pkh(if p.ends_with("/b") { "q" } else { "p" }))]
        } else {
            vec![]
        };
        let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let r = h.req("GET", &p, &hdrs, "").await;
        let mut v = if r.status == 200 { r.json() } else { json!({ "status": r.status }) };
        if let Some(t) = r.header("x-ms-session-token") {
            v["__session"] = json!(t);
        }
        out.insert(p, v);
    }
    Value::Object(out)
}

async fn setup_everything(h: &H) {
    let r = h.json("POST", "dbs", &[("x-ms-offer-throughput", "1000")], &json!({"id": "d"})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let body = json!({
        "id": "c",
        "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2},
        "indexingPolicy": {"indexingMode": "consistent", "automatic": true, "includedPaths": [{"path": "/*"}], "excludedPaths": [{"path": "/big/*"}, {"path": "/\"_etag\"/?"}]},
        "uniqueKeyPolicy": {"uniqueKeys": [{"paths": ["/email"]}]},
        "defaultTtl": 3600,
        "conflictResolutionPolicy": {"mode": "LastWriterWins", "conflictResolutionPath": "/_ts"},
        "analyticalStorageTtl": -1,
    });
    let r = h.json("POST", "dbs/d/colls", &[("x-ms-offer-throughput", "700")], &body).await;
    assert_eq!(r.status, 201, "{}", r.text());
    h.coll("d", "other", "/id").await;
    for (id, pk, v) in [("a", "p", 1), ("b", "q", 1), ("c3", "p", 3), ("c4", "p", 4)] {
        let r = h
            .doc(
                "d",
                "c",
                &json!(pk),
                &json!({"id": id, "pk": pk, "v": v, "email": format!("{id}@x"), "nested": {"list": [1, 2.5, null, "s"]}}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.text());
    }
    let r = h.doc("d", "c", &json!("p"), &json!({"id": "ttl1", "pk": "p", "ttl": 100000})).await;
    assert_eq!(r.status, 201);
    // a replace (new etag, new lsn) and a delete (the lsn moves, the document is gone)
    let r = h
        .json(
            "PUT",
            "dbs/d/colls/c/docs/a",
            &[("x-ms-documentdb-partitionkey", &pkh("p"))],
            &json!({"id": "a", "pk": "p", "v": 2, "email": "a@x"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(h.req("DELETE", "dbs/d/colls/c/docs/c3", &[("x-ms-documentdb-partitionkey", &pkh("p"))], "").await.status, 204);
    // scripts
    let r = h
        .json("POST", "dbs/d/colls/c/sprocs", &[], &json!({"id": "sp", "body": "function(){ getContext().getResponse().setBody(7); }"}))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = h
        .json(
            "POST",
            "dbs/d/colls/c/triggers",
            &[],
            &json!({"id": "tr", "body": "function(){}", "triggerType": "Pre", "triggerOperation": "All"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = h.json("POST", "dbs/d/colls/c/udfs", &[], &json!({"id": "fn", "body": "function(x){ return x + 1; }"})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    // users and permissions
    assert_eq!(h.json("POST", "dbs/d/users", &[], &json!({"id": "u1"})).await.status, 201);
    let r = h
        .json("POST", "dbs/d/users/u1/permissions", &[], &json!({"id": "p1", "permissionMode": "read", "resource": "dbs/d/colls/c"}))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn everything_a_client_can_see_survives_a_restart() {
    let data = dir("all");
    let h = start_on(&data).await;
    setup_everything(&h).await;
    // the change feed: read it all, note where it ended, then write two more
    let r = h.req("GET", "dbs/d/colls/c/docs", &[("A-IM", "Incremental Feed")], "").await;
    assert_eq!(r.status, 200);
    let first_feed = r.json()["Documents"].as_array().unwrap().len();
    let token = r.header("etag").unwrap().to_string();
    h.doc("d", "c", &json!("p"), &json!({"id": "n1", "pk": "p", "email": "n1@x"})).await;
    h.doc("d", "c", &json!("q"), &json!({"id": "n2", "pk": "q", "email": "n2@x"})).await;
    // a query with a continuation token
    let r = h
        .query(
            "d",
            "c",
            &[("x-ms-max-item-count", "2"), ("x-ms-documentdb-partitionkey", "[\"p\"]")],
            json!({"query": "SELECT c.id FROM c"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let continuation = r.header("x-ms-continuation").expect("a second page").to_string();
    let page1: Vec<Value> = r.json()["Documents"].as_array().unwrap().clone();
    let r = h
        .query(
            "d",
            "c",
            &[("x-ms-max-item-count", "2"), ("x-ms-documentdb-partitionkey", "[\"p\"]"), ("x-ms-continuation", &continuation)],
            json!({"query": "SELECT c.id FROM c"}),
        )
        .await;
    let page2_before: Vec<Value> = r.json()["Documents"].as_array().unwrap().clone();

    let feed_before = h.req("GET", "dbs/d/colls/c/docs", &[("A-IM", "Incremental Feed"), ("If-None-Match", &token)], "").await;
    assert_eq!(feed_before.json()["Documents"].as_array().unwrap().len(), 2);
    let old_rids: Vec<String> = h.req("GET", "dbs/d/colls/c/docs", &[], "").await.json()["Documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["_rid"].as_str().unwrap().to_string())
        .collect();
    // a write and a delete move the LSN past the last document's
    let old_lsn = h
        .doc("d", "c", &json!("p"), &json!({"id": "probe", "pk": "p", "email": "probe@x"}))
        .await
        .header("lsn")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    h.req("DELETE", "dbs/d/colls/c/docs/probe", &[("x-ms-documentdb-partitionkey", &pkh("p"))], "").await;
    let before = look(&h).await;
    h.srv.shutdown().await.unwrap();

    // ---- a second instance on the same directory ----
    let h2 = start_on(&data).await;
    let after = look(&h2).await;
    assert_eq!(before, after, "every resource reads back as it was, session tokens included");
    // the change feed continues from the token issued before the restart
    let r = h2.req("GET", "dbs/d/colls/c/docs", &[("A-IM", "Incremental Feed"), ("If-None-Match", &token)], "").await;
    assert_eq!(r.json()["Documents"], feed_before.json()["Documents"]);
    assert!(first_feed >= 4);
    // the query continuation from before the restart: the cache is gone, the query runs again and skips what was delivered
    let r = h2
        .query(
            "d",
            "c",
            &[("x-ms-max-item-count", "2"), ("x-ms-documentdb-partitionkey", "[\"p\"]"), ("x-ms-continuation", &continuation)],
            json!({"query": "SELECT c.id FROM c"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["Documents"].as_array().unwrap(), &page2_before);
    assert_eq!(page1.len(), 2);
    // new ids never repeat, and the LSN goes on from where it was
    let r = h2.doc("d", "c", &json!("p"), &json!({"id": "fresh", "pk": "p", "email": "fresh@x"})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let rid = r.json()["_rid"].as_str().unwrap().to_string();
    assert!(!old_rids.contains(&rid), "{rid} was used before");
    assert!(r.header("lsn").unwrap().parse::<u64>().unwrap() > old_lsn);
    let r = h2.json("POST", "dbs/d/colls", &[], &json!({"id": "later", "partitionKey": {"paths": ["/id"], "kind": "Hash"}})).await;
    let later = r.json();
    for old in ["c", "other"] {
        let o = h2.req("GET", &format!("dbs/d/colls/{old}"), &[], "").await.json();
        assert_ne!(o["_rid"], later["_rid"]);
    }
    // the stored procedure still runs, the unique key still holds, the TTL still applies
    let r = h2.json("POST", "dbs/d/colls/c/sprocs/sp", &[("x-ms-documentdb-partitionkey", &pkh("p"))], &json!([])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!(7));
    let r = h2.doc("d", "c", &json!("p"), &json!({"id": "dup", "pk": "p", "email": "a@x"})).await;
    assert_eq!(r.status, 409, "{}", r.text());
    h2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_all_versions_and_deletes_history_survives() {
    let data = dir("log");
    let h = start_on(&data).await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    let wire = ("x-ms-cosmos-changefeed-wire-format-version", "2021-09-15");
    let aim = ("A-IM", "Full-Fidelity Feed");
    let r = h.req("GET", "dbs/d/colls/c/docs", &[aim, wire, ("If-None-Match", "*")], "").await;
    let token = r.header("etag").unwrap().to_string();
    let p = pkh("p");
    h.doc("d", "c", &json!("p"), &json!({"id": "x", "pk": "p", "v": 1})).await;
    h.json("PUT", "dbs/d/colls/c/docs/x", &[("x-ms-documentdb-partitionkey", &p)], &json!({"id": "x", "pk": "p", "v": 2})).await;
    h.req("DELETE", "dbs/d/colls/c/docs/x", &[("x-ms-documentdb-partitionkey", &p)], "").await;
    let r = h.req("GET", "dbs/d/colls/c/docs", &[aim, wire, ("If-None-Match", &token)], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let before = r.json()["Documents"].clone();
    assert_eq!(before.as_array().unwrap().len(), 3);
    h.srv.shutdown().await.unwrap();
    let h2 = start_on(&data).await;
    let r = h2.req("GET", "dbs/d/colls/c/docs", &[aim, wire, ("If-None-Match", &token)], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["Documents"], before);
    h2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_with_nothing_changed_writes_nothing_and_a_change_writes_only_its_container() {
    let data = dir("quiet");
    let h = start_on(&data).await;
    h.db("d").await;
    h.coll("d", "one", "/pk").await;
    h.coll("d", "two", "/pk").await;
    for i in 0..20 {
        h.doc("d", "one", &json!("p"), &json!({"id": format!("i{i}"), "pk": "p"})).await;
        h.doc("d", "two", &json!("p"), &json!({"id": format!("i{i}"), "pk": "p"})).await;
    }
    h.srv.save_now().await;
    let a = files(&data);
    assert_eq!(a.iter().filter(|f| f.0.starts_with("docs-")).count(), 2, "{a:?}");
    assert!(a.iter().any(|f| f.0 == "cosmos.state"));
    // a query and a read are not changes
    h.req("GET", "dbs/d/colls/one/docs/i1", &[("x-ms-documentdb-partitionkey", &pkh("p"))], "").await;
    h.query("d", "one", &[], json!({"query": "SELECT * FROM c"})).await;
    h.srv.save_now().await;
    h.srv.save_now().await;
    assert_eq!(files(&data), a, "nothing was written");
    // one change in one container
    h.doc("d", "one", &json!("p"), &json!({"id": "more", "pk": "p"})).await;
    h.srv.save_now().await;
    let b = files(&data);
    let changed: Vec<_> = a.iter().zip(&b).filter(|(x, y)| x != y).map(|(x, _)| x.0.clone()).collect();
    assert_eq!(changed.len(), 1, "only one container's file is written again: {changed:?}");
    assert!(changed[0].starts_with("docs-"));
    // a container that is deleted loses its file
    assert_eq!(h.req("DELETE", "dbs/d/colls/two", &[], "").await.status, 204);
    h.srv.save_now().await;
    assert_eq!(files(&data).iter().filter(|f| f.0.starts_with("docs-")).count(), 1);
    // files are private
    use std::os::unix::fs::PermissionsExt;
    for (name, _, _) in files(&data) {
        let mode = std::fs::metadata(data.join("cosmos").join(&name)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    h.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reset_is_saved_too() {
    let data = dir("reset");
    let h = start_on(&data).await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h.doc("d", "c", &json!("p"), &json!({"id": "x", "pk": "p"})).await;
    h.srv.save_now().await;
    let before = h.svc.persist_generation();
    h.srv.reset();
    assert_ne!(h.svc.persist_generation(), before);
    h.srv.save_now().await;
    h.srv.shutdown().await.unwrap();
    let h2 = start_on(&data).await;
    assert_eq!(h2.req("GET", "dbs/d", &[], "").await.status, 404);
    assert_eq!(
        std::fs::read_dir(data.join("cosmos"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("docs-"))
            .count(),
        0
    );
    h2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_does_not_hold_the_lock_while_it_encodes_and_writes() {
    let data = dir("lock");
    let h = Arc::new(start_on(&data).await);
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    // 6,000 documents of 2 KB: a few ms to copy, far more to encode and write
    let filler = "x".repeat(2000);
    for chunk in 0..60 {
        let mut set = tokio::task::JoinSet::new();
        for i in 0..100 {
            let h = h.clone();
            let f = filler.clone();
            set.spawn(async move {
                h.doc("d", "c", &json!("p"), &json!({"id": format!("d{}", chunk * 100 + i), "pk": "p", "f": f})).await.status
            });
        }
        while let Some(s) = set.join_next().await {
            assert_eq!(s.unwrap(), 201);
        }
    }
    let saver = h.clone();
    let save = tokio::spawn(async move {
        let t = Instant::now();
        saver.srv.save_now().await;
        t.elapsed()
    });
    let mut worst = Duration::ZERO;
    let mut n = 0;
    while !save.is_finished() {
        let t = Instant::now();
        let r = h.req("GET", "dbs/d/colls/c/docs/d1", &[("x-ms-documentdb-partitionkey", &pkh("p"))], "").await;
        assert_eq!(r.status, 200);
        worst = worst.max(t.elapsed());
        n += 1;
    }
    let took = save.await.unwrap();
    assert!(n >= 5, "only {n} requests ran during a save of {took:?}");
    assert!(worst < took / 2 || worst < Duration::from_millis(60), "a request waited {worst:?} while the save took {took:?}");
    let h = Arc::try_unwrap(h).ok().expect("no other owner");
    h.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

// ---- files that are not to be trusted ----

async fn make_saved(name: &str) -> PathBuf {
    let data = dir(name);
    let h = start_on(&data).await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h.doc("d", "c", &json!("p"), &json!({"id": "x", "pk": "p", "v": 1})).await;
    h.srv.shutdown().await.unwrap();
    data
}

fn docs_file(data: &Path) -> PathBuf {
    std::fs::read_dir(data.join("cosmos"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("docs-"))
        .unwrap()
}

async fn start_error(data: &Path) -> String {
    match try_start_on(data).await {
        Ok(_) => panic!("the emulator started on a damaged directory"),
        Err(e) => format!("{e:#}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_files_stop_the_start_with_a_message_that_names_the_file() {
    // truncated
    let data = make_saved("trunc").await;
    let f = docs_file(&data);
    let bytes = std::fs::read(&f).unwrap();
    std::fs::write(&f, &bytes[..bytes.len() - 5]).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains(&f.file_name().unwrap().to_string_lossy().to_string()), "{e}");
    let _ = std::fs::remove_dir_all(&data);

    // flipped bytes: every position of the definitions file either loads or is an error, never a panic or a hang
    let data = make_saved("flip").await;
    let defs = data.join("cosmos").join("cosmos.state");
    let original = std::fs::read(&defs).unwrap();
    let mut errors = 0;
    for i in 0..original.len() {
        let mut b = original.clone();
        b[i] ^= 0xff;
        std::fs::write(&defs, &b).unwrap();
        match try_start_on(&data).await {
            Ok(s) => s.shutdown().await.map(|_| ()).unwrap_or(()),
            Err(e) => {
                errors += 1;
                assert!(format!("{e:#}").contains("cosmos"), "{e:#}");
            }
        }
        // the shutdown above saved again; put the damaged file back for the next position
        let _ = std::fs::remove_dir_all(data.join("cosmos"));
        std::fs::create_dir_all(data.join("cosmos")).unwrap();
        std::fs::write(&defs, &original).unwrap();
    }
    assert!(errors > 10, "{errors} of {} flipped bytes were noticed", original.len());
    let _ = std::fs::remove_dir_all(&data);

    // a file of another kind, and one written by a newer version
    let data = make_saved("kind").await;
    let f = docs_file(&data);
    let mut b = std::fs::read(&f).unwrap();
    let defs = std::fs::read(data.join("cosmos").join("cosmos.state")).unwrap();
    std::fs::write(&f, &defs).unwrap();
    let e = start_error(&data).await;
    assert!(
        e.contains("holds cosmos state, expected cosmos-docs") && e.contains(&f.file_name().unwrap().to_string_lossy().to_string()),
        "{e}"
    );
    let i = b.iter().position(|c| *c == b'\n').unwrap();
    let header = String::from_utf8(b[..i].to_vec()).unwrap().replace(" 1", " 99");
    b.splice(..i, header.into_bytes());
    std::fs::write(&f, &b).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains("version 99") && e.contains("newer azemu"), "{e}");
    let _ = std::fs::remove_dir_all(&data);

    // garbage
    let data = make_saved("junk").await;
    std::fs::write(data.join("cosmos").join("cosmos.state"), b"hello").unwrap();
    assert!(start_error(&data).await.contains("cosmos.state"));
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_and_extra_files_and_other_containers_documents() {
    // a container's file is missing: the container is there and empty
    let data = make_saved("missing").await;
    std::fs::remove_file(docs_file(&data)).unwrap();
    let h = start_on(&data).await;
    assert_eq!(h.req("GET", "dbs/d/colls/c", &[], "").await.status, 200);
    let r = h.req("GET", "dbs/d/colls/c/docs", &[], "").await;
    assert_eq!(r.json()["_count"], 0);
    h.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);

    // extra files are ignored; an unreferenced document file is removed by the next save
    let data = make_saved("extra").await;
    let orphan = data.join("cosmos").join("docs-0000000000000000.state");
    std::fs::copy(docs_file(&data), &orphan).unwrap();
    std::fs::write(data.join("cosmos").join("notes.txt"), "mine").unwrap();
    let h = start_on(&data).await;
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/x", &[("x-ms-documentdb-partitionkey", &pkh("p"))], "").await.status, 200);
    h.doc("d", "c", &json!("p"), &json!({"id": "y", "pk": "p"})).await;
    h.srv.save_now().await;
    assert!(!orphan.exists());
    assert!(data.join("cosmos").join("notes.txt").exists());
    h.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);

    // a file that holds another container's documents is refused
    let data = make_saved("swap").await;
    {
        let h = start_on(&data).await;
        h.coll("d", "c2", "/pk").await;
        h.doc("d", "c2", &json!("p"), &json!({"id": "z", "pk": "p"})).await;
        h.srv.shutdown().await.unwrap();
    }
    let all: Vec<PathBuf> = std::fs::read_dir(data.join("cosmos"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("docs-"))
        .collect();
    assert_eq!(all.len(), 2);
    let a = std::fs::read(&all[0]).unwrap();
    let b = std::fs::read(&all[1]).unwrap();
    std::fs::write(&all[0], b).unwrap();
    std::fs::write(&all[1], a).unwrap();
    assert!(start_error(&data).await.contains("another container"));
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deeply_nested_and_huge_claims_in_a_file_are_errors_not_crashes() {
    let data = make_saved("hostile").await;
    let defs = data.join("cosmos").join("cosmos.state");
    // nesting far past any limit, inside a field that holds an arbitrary value (built as bytes: a value this deep
    // cannot even be built and dropped safely)
    let mut b = b"AZEMU-STATE cosmos 1\n".to_vec();
    b.extend_from_slice(&[0x81, 0xa8]);
    b.extend_from_slice(b"accounts");
    b.extend_from_slice(&[0x91]);
    b.extend(std::iter::repeat_n(0x91, 100_000));
    b.push(0xc0);
    std::fs::write(&defs, b).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains("cosmos.state"), "{e}");
    // an array that claims four billion entries and has none
    let mut b = b"AZEMU-STATE cosmos 1\n".to_vec();
    b.extend_from_slice(&[0x81, 0xa8]);
    b.extend_from_slice(b"accounts");
    b.extend_from_slice(&[0xdd, 0xff, 0xff, 0xff, 0xff]);
    std::fs::write(&defs, b).unwrap();
    assert!(start_error(&data).await.contains("cosmos.state"));
    let _ = std::fs::remove_dir_all(&data);
}

/// Cost of a save after one small change next to about 100 MB of other state, and of a restart. Not part of the
/// normal run (it writes 100 MB): `AZEMU_PERSIST_BENCH_DIR=<dir not on /tmp> cargo test -p azemu-cosmos --test persist -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cost_of_a_small_save_next_to_100_mb() {
    let base = std::env::var("AZEMU_PERSIST_BENCH_DIR").expect("AZEMU_PERSIST_BENCH_DIR");
    let data = PathBuf::from(base).join("cosmos-bench");
    let _ = std::fs::remove_dir_all(&data);
    let h = Arc::new(start_on(&data).await);
    h.db("d").await;
    h.coll("d", "big", "/pk").await;
    h.coll("d", "small", "/pk").await;
    let filler = "x".repeat(2000);
    for chunk in 0..50 {
        let mut set = tokio::task::JoinSet::new();
        for i in 0..1000 {
            let (h, f) = (h.clone(), filler.clone());
            set.spawn(async move {
                h.doc(
                    "d",
                    "big",
                    &json!(format!("p{}", i % 50)),
                    &json!({"id": format!("d{}", chunk * 1000 + i), "pk": format!("p{}", i % 50), "f": f}),
                )
                .await
                .status
            });
        }
        while let Some(s) = set.join_next().await {
            assert_eq!(s.unwrap(), 201);
        }
    }
    let t = Instant::now();
    h.srv.save_now().await;
    eprintln!("BENCH first save of 50,000 documents (about 100 MB): {:?}", t.elapsed());
    let size: u64 = std::fs::read_dir(data.join("cosmos")).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
    eprintln!("BENCH bytes on disk: {size}");
    h.doc("d", "small", &json!("p"), &json!({"id": "one", "pk": "p"})).await;
    let t = Instant::now();
    h.srv.save_now().await;
    eprintln!("BENCH save after one small change: {:?}", t.elapsed());
    let t = Instant::now();
    h.srv.save_now().await;
    eprintln!("BENCH save with nothing changed: {:?}", t.elapsed());
    let h = Arc::try_unwrap(h).ok().unwrap();
    h.srv.shutdown().await.unwrap();
    let t = Instant::now();
    let h2 = start_on(&data).await;
    eprintln!("BENCH restart (load) of 50,000 documents: {:?}", t.elapsed());
    assert_eq!(h2.req("GET", "dbs/d/colls/big/docs/d77", &[("x-ms-documentdb-partitionkey", "[\"p27\"]")], "").await.status, 200);
    h2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}
