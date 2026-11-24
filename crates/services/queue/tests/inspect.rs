//! The inspector: the shape of every node, paging, no lazy accounts, and that looking changes nothing a client can see.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_queue::QueueService;
use common::*;
use serde_json::Value;

async fn start_with_handle() -> (Q, Arc<QueueService>) {
    let svc = Arc::new(QueueService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let base = srv.url("queue", ACCOUNT);
    (Q { srv, base }, svc)
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

async fn get(q: &Q, path_and_query: &str) -> (u16, Value) {
    let r = q.srv.send("GET", &format!("{}/_azemu/inspect{}", q.srv.gateway_url(), path_and_query), &[], "").await;
    let status = r.status;
    (status, if r.body.is_empty() { Value::Null } else { r.json() })
}

/// Every node below `start`, following `children[].path` and `items[].path`, every list read page by page with `?limit=2`.
async fn walk(q: &Q, start: &str) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    let mut todo = vec![start.to_string()];
    let mut nodes = Vec::new();
    while let Some(path) = todo.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let mut marker: Option<String> = None;
        let mut pages = 0;
        loop {
            let query = match &marker {
                Some(m) => format!("?limit=2&marker={m}"),
                None => "?limit=2".to_string(),
            };
            let (status, v) = get(q, &format!("{path}{query}")).await;
            assert_eq!(status, 200, "{path}{query}");
            for k in ["path", "kind", "title", "properties", "children", "items", "next"] {
                assert!(v.get(k).is_some(), "{path}: missing {k}");
            }
            assert_eq!(decode(v["path"].as_str().unwrap()), decode(&path), "the node's path is the path asked");
            assert!(v["items"].as_array().unwrap().len() <= 2);
            for c in v["children"].as_array().unwrap() {
                todo.push(c["path"].as_str().unwrap().to_string());
            }
            for i in v["items"].as_array().unwrap() {
                if let Some(p) = i.get("path").and_then(Value::as_str) {
                    todo.push(p.to_string());
                }
            }
            nodes.push(v.clone());
            pages += 1;
            assert!(pages < 50, "{path}: paging does not end");
            match v["next"].as_str() {
                Some(m) => marker = Some(m.to_string()),
                None => break,
            }
        }
    }
    nodes
}

async fn all_rows(q: &Q, path: &str) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let query = match &marker {
            Some(m) => format!("?limit=2&marker={m}"),
            None => "?limit=2".to_string(),
        };
        let (status, v) = get(q, &format!("{path}{query}")).await;
        assert_eq!(status, 200);
        rows.extend(v["items"].as_array().unwrap().iter().cloned());
        match v["next"].as_str() {
            Some(m) => marker = Some(m.to_string()),
            None => return rows,
        }
    }
}

/// The same text with a base64 body and with a plain one, a hidden one, a delayed one, one that never expires.
async fn fill(q: &Q) {
    q.create("orders").await;
    q.create("second-q").await;
    q.create("empty-q").await;
    let r = q.send("PUT", "/second-q?comp=metadata", &[("x-ms-meta-owner", "tests")], "").await;
    assert_eq!(r.status, 204);
    assert_eq!(q.put("orders", "plain text", "").await.status, 201);
    assert_eq!(q.put("orders", "aGVsbG8gd29ybGQ=", "").await.status, 201); // "hello world"
    assert_eq!(q.put("orders", "delayed", "visibilitytimeout=3600").await.status, 201);
    assert_eq!(q.put("orders", "forever", "messagettl=-1").await.status, 201);
    assert_eq!(q.put("orders", "taken", "").await.status, 201);
    assert_eq!(q.put("second-q", "&lt;b&gt;markup&lt;/b&gt;", "").await.status, 201);
}

#[tokio::test]
async fn the_whole_tree_has_the_common_shape_and_paging_ends() {
    let (q, _) = start_with_handle().await;
    fill(&q).await;
    // one message is taken by a consumer and so hidden
    assert_eq!(q.get("orders", "visibilitytimeout=3600&numofmessages=1").await.status, 200);

    let nodes = walk(&q, "/queue").await;
    let paths: BTreeSet<String> = nodes.iter().map(|n| n["path"].as_str().unwrap().to_string()).collect();
    for p in [
        "/queue",
        "/queue/acct1",
        "/queue/acct1/queues",
        "/queue/acct1/queues/orders",
        "/queue/acct1/queues/orders/messages",
        "/queue/acct1/queues/empty-q/messages",
    ] {
        assert!(paths.contains(p), "{p} not reached: {paths:?}");
    }
    let root = nodes.iter().find(|n| n["path"] == "/queue").unwrap();
    assert_eq!(root["kind"], "service");
    assert_eq!(root["properties"]["accounts"], 1);

    let rows = all_rows(&q, "/queue/acct1/queues/orders/messages").await;
    assert_eq!(rows.len(), 5, "hidden and delayed messages are listed too");
    assert!(rows.iter().all(|r| r.get("path").is_none()));
    let by_text = |t: &str| rows.iter().find(|r| r["body"]["text"] == t).unwrap_or_else(|| panic!("no row {t}"));
    let first = &rows[0];
    assert_eq!(first["position"], 0);
    assert_eq!(first["body"]["text"], "plain text");
    assert_eq!(first["dequeueCount"], 1);
    assert_eq!(first["visible"], false, "the one a consumer took is hidden");
    assert!(first["body"].get("decoded").is_none());
    assert_eq!(by_text("aGVsbG8gd29ybGQ=")["body"]["decoded"], "hello world");
    assert_eq!(by_text("delayed")["visible"], false);
    assert_eq!(by_text("delayed")["dequeueCount"], 0);
    assert_eq!(by_text("forever")["expirationTime"], "9999-12-31T23:59:59.000Z");
    assert_eq!(by_text("taken")["visible"], true);
    for k in ["id", "insertionTime", "expirationTime", "nextVisibleTime"] {
        assert!(first[k].is_string(), "{k}");
    }
    assert_ne!(first["nextVisibleTime"], first["insertionTime"]);
    // the pop receipt is what lets a client delete the message: not shown
    assert!(!serde_json::to_string(&rows).unwrap().contains("popReceipt"));

    let (_, orders) = get(&q, "/queue/acct1/queues/orders").await;
    assert_eq!(orders["kind"], "queue");
    assert_eq!(orders["properties"]["approximateMessagesCount"], 5);
    assert_eq!(orders["properties"]["visibleMessages"], 3);
    assert_eq!(orders["properties"]["hiddenMessages"], 2);
    let (_, second) = get(&q, "/queue/acct1/queues/second-q").await;
    assert_eq!(second["properties"]["metadata"]["owner"], "tests");
    let (_, markup) = get(&q, "/queue/acct1/queues/second-q/messages").await;
    assert_eq!(markup["items"][0]["body"]["text"], "<b>markup</b>", "the text is kept as stored");
}

#[tokio::test]
async fn a_queue_list_filters_by_prefix_and_pages() {
    let (q, _) = start_with_handle().await;
    for n in ["aaa", "aab", "aac", "bbb"] {
        q.create(n).await;
    }
    let rows = all_rows(&q, "/queue/acct1/queues").await;
    assert_eq!(rows.iter().map(|r| r["name"].as_str().unwrap()).collect::<Vec<_>>(), ["aaa", "aab", "aac", "bbb"]);
    let (_, v) = get(&q, "/queue/acct1/queues?prefix=aa&limit=10").await;
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
    assert_eq!(v["parameters"], serde_json::json!(["prefix"]));
}

#[tokio::test]
async fn a_path_that_names_nothing_is_404_and_creates_nothing() {
    let (q, svc) = start_with_handle().await;
    fill(&q).await;
    let (_, before) = get(&q, "/queue").await;
    let generation = svc.persist_generation();
    for p in [
        "/queue/ghost",
        "/queue/ghost/queues",
        "/queue/ghost/queues/orders",
        "/queue/ghost/queues/orders/messages",
        "/queue/acct1/queues/ghost",
        "/queue/acct1/queues/ghost/messages",
        "/queue/acct1/other",
        "/queue/acct1/queues/orders/messages/x",
    ] {
        let (status, v) = get(&q, p).await;
        assert_eq!(status, 404, "{p}");
        assert!(v["error"]["code"].is_string());
    }
    let (_, after) = get(&q, "/queue").await;
    assert_eq!(after["properties"]["accounts"], before["properties"]["accounts"]);
    assert_eq!(after["items"], before["items"]);
    assert_eq!(svc.persist_generation(), generation);
    let names: Vec<_> = all_rows(&q, "/queue/acct1/queues").await.iter().map(|r| r["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["empty-q", "orders", "second-q"], "no queue appeared");
}

/// What a client of the queue API sees, without changing anything.
async fn client_view(q: &Q) -> (String, String, String) {
    let count = q.send("GET", "/orders?comp=metadata", &[], "").await;
    let count = count.header("x-ms-approximate-messages-count").unwrap().to_string();
    let peeked = q.peek("orders", "numofmessages=32").await.text();
    let other = q.peek("second-q", "numofmessages=32").await.text();
    (count, peeked, other)
}

#[tokio::test]
async fn looking_changes_no_counter_visibility_or_dequeue_count() {
    let (q, svc) = start_with_handle().await;
    fill(&q).await;
    // taken for an hour: stays hidden however often the inspector looks
    let taken = q.get("orders", "visibilitytimeout=3600").await;
    assert_eq!(taken.status, 200);
    let hidden_id = tag(&taken.text(), "MessageId").unwrap();
    // a short-lived message that is already past its time while the service has not noticed yet
    assert_eq!(q.put("orders", "short-lived", "messagettl=1").await.status, 201);
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;

    let rows_before = all_rows(&q, "/queue/acct1/queues/orders/messages").await;
    assert_eq!(rows_before.len(), 5, "the expired message is not shown, as a client would not get it");
    let generation = svc.persist_generation();
    let view_before = client_view(&q).await;
    assert_eq!(view_before.0, "5");

    for _ in 0..3 {
        walk(&q, "/queue").await;
    }

    assert_eq!(svc.persist_generation(), generation, "a save would see no change");
    assert_eq!(client_view(&q).await, view_before);
    let rows_after = all_rows(&q, "/queue/acct1/queues/orders/messages").await;
    assert_eq!(rows_after, rows_before, "ids, dequeue counts, visibility times, text");
    // the hidden message is still hidden and was not counted as delivered
    let hidden = rows_after.iter().find(|r| r["id"] == hidden_id.as_str()).unwrap();
    assert_eq!(hidden["dequeueCount"], 1);
    assert_eq!(hidden["visible"], false);
    assert!(!view_before.1.contains(&hidden_id));
    // the next real receive sees the untouched dequeue counts: 1 for a message the walk looked at
    let got = q.get("orders", "numofmessages=32&visibilitytimeout=5").await.text();
    assert_eq!(got.matches("<DequeueCount>1</DequeueCount>").count(), 3, "{got}");
    assert!(!got.contains("<DequeueCount>2</DequeueCount>"));
    assert!(!got.contains(&hidden_id));
}
