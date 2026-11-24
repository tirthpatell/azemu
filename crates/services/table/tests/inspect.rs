//! The inspector: the shape of every node, paging, the partition filter, property types, no lazy accounts, and that looking changes nothing.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use azemu_core::serde_json::{json, Value};
use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_table::TableService;
use common::*;

async fn start_with_handle() -> (Env, Arc<TableService>) {
    let svc = Arc::new(TableService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let base = srv.url("table", "acct");
    (Env { srv, base }, svc)
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

async fn get(e: &Env, path_and_query: &str) -> (u16, Value) {
    let r = e.srv.send("GET", &format!("{}/_azemu/inspect{}", e.srv.gateway_url(), path_and_query), &[], "").await;
    let status = r.status;
    (status, if r.body.is_empty() { Value::Null } else { r.json() })
}

/// Every node below `start`, following `children[].path` and `items[].path`, every list read page by page with `?limit=2`.
async fn walk(e: &Env, start: &str) -> Vec<Value> {
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
            let (status, v) = get(e, &format!("{path}{query}")).await;
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

/// All rows of a list, two at a time (`extra` is more query, like `partition=p1`).
async fn all_rows(e: &Env, path: &str, extra: &str) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let mut query = format!("?limit=2&{extra}");
        if let Some(m) = &marker {
            query.push_str(&format!("&marker={m}"));
        }
        let (status, v) = get(e, &format!("{path}{query}")).await;
        assert_eq!(status, 200, "{path}{query}");
        rows.extend(v["items"].as_array().unwrap().iter().cloned());
        match v["next"].as_str() {
            Some(m) => marker = Some(m.to_string()),
            None => return rows,
        }
    }
}

async fn fill(e: &Env) {
    assert_eq!(e.create_table("Orders").await.status, 201);
    assert_eq!(e.create_table("items").await.status, 201);
    e.create_table("emptytable").await;
    for pk in ["p1", "p2", "p 3"] {
        for rk in ["a", "b", "c"] {
            let body = json!({"PartitionKey": pk, "RowKey": rk, "n": 1, "name": format!("{pk}/{rk}")}).to_string();
            assert_eq!(e.insert("Orders", &body).await.status, 201);
        }
    }
    let typed = r#"{"PartitionKey":"t","RowKey":"typed","s":"x","i":5,"f":2.5,"b":true,"big@odata.type":"Edm.Int64","big":"9007199254740993","d@odata.type":"Edm.DateTime","d":"2020-01-02T03:04:05.1234567Z","g@odata.type":"Edm.Guid","g":"C9DA6455-213D-42C9-9A79-3E9149A57833","bin@odata.type":"Edm.Binary","bin":"YmluYXJ5","nan@odata.type":"Edm.Double","nan":"NaN"}"#;
    assert_eq!(e.insert("items", typed).await.status, 201);
    let acl = "<SignedIdentifiers><SignedIdentifier><Id>reader</Id><AccessPolicy><Start>2021-06-08T02:05:09Z</Start><Expiry>2031-06-08T03:10:09Z</Expiry><Permission>r</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(e.send("PUT", "/items?comp=acl", &[("Content-Type", "application/xml")], acl).await.status, 204);
}

#[tokio::test]
async fn the_whole_tree_has_the_common_shape_and_paging_ends() {
    let (e, _) = start_with_handle().await;
    fill(&e).await;
    let nodes = walk(&e, "/table").await;
    let paths: BTreeSet<String> = nodes.iter().map(|n| n["path"].as_str().unwrap().to_string()).collect();
    for p in [
        "/table",
        "/table/acct",
        "/table/acct/tables",
        "/table/acct/tables/orders",
        "/table/acct/tables/orders/entities",
        "/table/acct/tables/items/entities",
        "/table/acct/tables/emptytable/entities",
    ] {
        assert!(paths.contains(p), "{p} not reached: {paths:?}");
    }
    let root = nodes.iter().find(|n| n["path"] == "/table").unwrap();
    assert_eq!((root["kind"].as_str(), root["properties"]["accounts"].as_u64()), (Some("service"), Some(1)));

    let tables = all_rows(&e, "/table/acct/tables", "").await;
    assert_eq!(tables.iter().map(|t| t["name"].as_str().unwrap()).collect::<Vec<_>>(), ["emptytable", "items", "Orders"]);
    assert_eq!(tables[2]["entities"], 9);
    let (_, orders) = get(&e, "/table/acct/tables/Orders").await;
    assert_eq!(orders["title"], "Orders", "the name as it was created; the path takes any case");
    assert_eq!(orders["properties"]["entities"], 9);
    let (_, items) = get(&e, "/table/acct/tables/items").await;
    assert_eq!(items["properties"]["accessPolicies"][0]["id"], "reader");
    assert_eq!(items["properties"]["accessPolicies"][0]["permission"], "r");
}

#[tokio::test]
async fn entities_come_in_key_order_with_every_property_and_its_type() {
    let (e, _) = start_with_handle().await;
    fill(&e).await;
    let all = all_rows(&e, "/table/acct/tables/orders/entities", "").await;
    let keys: Vec<String> =
        all.iter().map(|r| format!("{}/{}", r["partitionKey"].as_str().unwrap(), r["rowKey"].as_str().unwrap())).collect();
    assert_eq!(keys, ["p 3/a", "p 3/b", "p 3/c", "p1/a", "p1/b", "p1/c", "p2/a", "p2/b", "p2/c"]);

    let rows = all_rows(&e, "/table/acct/tables/items/entities", "").await;
    assert_eq!(rows.len(), 1);
    let p = &rows[0]["properties"];
    assert_eq!(p["s"], json!({"type": "Edm.String", "value": "x"}));
    assert_eq!(p["i"], json!({"type": "Edm.Int32", "value": 5}));
    assert_eq!(p["f"], json!({"type": "Edm.Double", "value": 2.5}));
    assert_eq!(p["b"], json!({"type": "Edm.Boolean", "value": true}));
    assert_eq!(p["big"], json!({"type": "Edm.Int64", "value": "9007199254740993"}));
    assert_eq!(p["d"], json!({"type": "Edm.DateTime", "value": "2020-01-02T03:04:05.1234567Z"}));
    assert_eq!(p["g"], json!({"type": "Edm.Guid", "value": "c9da6455-213d-42c9-9a79-3e9149a57833"}));
    assert_eq!(p["bin"], json!({"type": "Edm.Binary", "value": "YmluYXJ5"}));
    assert_eq!(p["nan"], json!({"type": "Edm.Double", "value": "NaN"}));
    assert!(rows[0]["etag"].as_str().unwrap().starts_with("W/\"datetime'"));
    assert!(rows[0]["timestamp"].is_string() && rows[0]["size"].as_u64().unwrap() > 0);
    // the etag is the one a client reads
    let r = e.get("items", "t", "typed").await;
    assert_eq!(r.header("etag"), rows[0]["etag"].as_str());
}

#[tokio::test]
async fn the_partition_filter_and_paging_work_together() {
    let (e, _) = start_with_handle().await;
    fill(&e).await;
    let rows = all_rows(&e, "/table/acct/tables/orders/entities", "partition=p1").await;
    assert_eq!(rows.iter().map(|r| r["rowKey"].as_str().unwrap()).collect::<Vec<_>>(), ["a", "b", "c"]);
    assert!(rows.iter().all(|r| r["partitionKey"] == "p1"));
    let rows = all_rows(&e, "/table/acct/tables/orders/entities", "partition=p%203").await;
    assert_eq!(rows.len(), 3);
    assert!(all_rows(&e, "/table/acct/tables/orders/entities", "partition=nothing").await.is_empty());
    let (_, v) = get(&e, "/table/acct/tables/orders/entities?partition=p2&limit=2").await;
    assert_eq!(v["parameters"], json!(["partition"]));
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    assert!(v["next"].is_string());
    let (status, _) = get(&e, "/table/acct/tables/orders/entities?marker=bm90LW91cnM").await;
    assert_eq!(status, 400, "a marker this node did not give out");
}

#[tokio::test]
async fn a_path_that_names_nothing_is_404_and_creates_nothing() {
    let (e, svc) = start_with_handle().await;
    fill(&e).await;
    let (_, before) = get(&e, "/table").await;
    let generation = svc.persist_generation();
    for p in [
        "/table/ghost",
        "/table/ghost/tables",
        "/table/ghost/tables/orders",
        "/table/ghost/tables/orders/entities?partition=p1",
        "/table/acct/tables/ghost",
        "/table/acct/tables/ghost/entities",
        "/table/acct/other",
        "/table/acct/tables/orders/entities/x",
    ] {
        let (status, v) = get(&e, p).await;
        assert_eq!(status, 404, "{p}");
        assert!(v["error"]["code"].is_string());
    }
    let (_, after) = get(&e, "/table").await;
    assert_eq!(after["properties"]["accounts"], before["properties"]["accounts"]);
    assert_eq!(after["items"], before["items"]);
    assert_eq!(svc.persist_generation(), generation);
    let tables = all_rows(&e, "/table/acct/tables", "").await;
    assert_eq!(tables.len(), 3, "no table appeared");
}

#[tokio::test]
async fn looking_changes_nothing_a_client_can_see() {
    let (e, svc) = start_with_handle().await;
    fill(&e).await;
    let read = |t: &'static str| {
        let e = &e;
        async move { e.send("GET", &format!("/{t}()"), &[("Accept", "application/json;odata=fullmetadata")], "").await.text() }
    };
    let before = (read("Orders").await, read("items").await, svc.persist_generation());
    let rows_before = all_rows(&e, "/table/acct/tables/orders/entities", "").await;
    for _ in 0..3 {
        walk(&e, "/table").await;
    }
    assert_eq!((read("Orders").await, read("items").await, svc.persist_generation()), before);
    assert_eq!(all_rows(&e, "/table/acct/tables/orders/entities", "").await, rows_before);
}

/// An entity may hold 252 properties of 64 KiB (1 MiB). A page of them was copied whole under the table lock and sent whole: 100 entities of
/// 480 KB made a 48 MB answer. A row shows each long value cut, with how long the whole was, and a page stays inside the page limit.
#[tokio::test]
async fn a_page_of_big_entities_is_cut_in_every_row_and_in_the_page() {
    let (e, _svc) = start_with_handle().await;
    assert_eq!(e.create_table("big").await.status, 201);
    // a string property holds up to 32,768 characters
    let text = "x".repeat(30_000);
    for i in 0..100 {
        let mut body = json!({"PartitionKey": "p", "RowKey": format!("r{i:03}")});
        for k in 0..15 {
            body[format!("s{k:02}")] = json!(text);
        }
        let r = e.insert("big", &body.to_string()).await;
        assert_eq!(r.status, 201, "{i}: {}", r.text());
    }
    let started = std::time::Instant::now();
    let r = e.srv.send("GET", &format!("{}/_azemu/inspect/table/acct/tables/big/entities?limit=100", e.srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200, "{}", &r.text()[..200.min(r.text().len())]);
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(r.body.len() <= azemu_core::inspect::PAGE_LIMIT + 64 * 1024, "the answer is {} bytes", r.body.len());
    let v = r.json();
    let rows = v["items"].as_array().unwrap();
    assert!(!rows.is_empty() && rows.len() < 100, "a page that is too long ends early: {} rows", rows.len());
    assert!(v["next"].is_string(), "and says where it goes on");
    for row in rows {
        let s0 = &row["properties"]["s00"];
        assert_eq!(s0["cut"], true, "{s0}");
        assert_eq!(s0["bytes"], 30_000);
        assert!(s0["value"].as_str().unwrap().len() <= 8 * 1024);
        assert!(row["size"].as_u64().unwrap() > 500_000, "the row says how big the entity is");
        assert!(row.to_string().len() <= azemu_core::inspect::ROW_LIMIT, "a row is {} bytes", row.to_string().len());
    }
    // a small value is shown whole, and the pages together are every entity once
    assert_eq!(e.insert("big", &json!({"PartitionKey": "q", "RowKey": "small", "name": "short"}).to_string()).await.status, 201);
    let mut seen = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let q = marker.as_ref().map(|m| format!("&marker={m}")).unwrap_or_default();
        let r = e
            .srv
            .send("GET", &format!("{}/_azemu/inspect/table/acct/tables/big/entities?limit=100{q}", e.srv.gateway_url()), &[], "")
            .await;
        let v = r.json();
        for row in v["items"].as_array().unwrap() {
            seen.push(format!("{}/{}", row["partitionKey"].as_str().unwrap(), row["rowKey"].as_str().unwrap()));
            if row["rowKey"] == "small" {
                assert_eq!(row["properties"]["name"], json!({"type": "Edm.String", "value": "short"}));
            }
        }
        match v["next"].as_str() {
            Some(m) => marker = Some(m.to_string()),
            None => break,
        }
    }
    assert_eq!(seen.len(), 101, "{seen:?}");
}
