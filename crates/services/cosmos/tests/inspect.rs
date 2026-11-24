//! The inspector of Cosmos DB (`/_azemu/inspect/cosmos/...`).

mod common;

use azemu_core::inspect::walk::walk;
use common::*;
use serde_json::{json, Value};

async fn fill(h: &H) {
    h.db("shop").await;
    h.db("logs").await;
    let r = h
        .json(
            "POST",
            "dbs/shop/colls",
            &[],
            &json!({"id": "orders", "partitionKey": {"paths": ["/customer"], "kind": "Hash", "version": 2}, "defaultTtl": 3600,
                    "uniqueKeyPolicy": {"uniqueKeys": [{"paths": ["/ref"]}]}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    h.coll("shop", "users", "/id").await;
    for (c, id, extra) in [("ann", "o1", 1), ("ann", "o2", 2), ("bob", "o1", 3), ("cy", "o3", 4), ("cy", "o4", 5)] {
        let r = h.doc("shop", "orders", &json!(c), &json!({"id": id, "customer": c, "ref": format!("{c}-{id}"), "n": extra})).await;
        assert_eq!(r.status, 201, "{}", r.text());
    }
    // a document that has already expired: ttl of one second, written in the past is not possible, so one that never expires
    let r = h.doc("shop", "orders", &json!("dee"), &json!({"id": "keep", "customer": "dee", "ref": "d", "ttl": -1})).await;
    assert_eq!(r.status, 201);
    let r = h
        .json(
            "POST",
            "dbs/shop/colls/orders/sprocs",
            &[],
            &json!({"id": "hello", "body": "function(){ getContext().getResponse().setBody('hi'); }"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = h
        .json(
            "POST",
            "dbs/shop/colls/orders/triggers",
            &[],
            &json!({"id": "pre", "body": "function(){}", "triggerType": "Pre", "triggerOperation": "All"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = h.json("POST", "dbs/shop/colls/orders/udfs", &[], &json!({"id": "tax", "body": "function(x){ return x * 2; }"})).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

async fn insp(h: &H, p: &str) -> azemu_core::testing::TestResponse {
    h.srv.send("GET", &format!("{}/_azemu/inspect{p}", h.srv.gateway_url()), &[], "").await
}

#[tokio::test]
async fn the_whole_tree_can_be_walked_and_paging_ends() {
    let h = start().await;
    fill(&h).await;
    for limit in [1, 2, 50] {
        let nodes = walk(&h.srv, "/cosmos", limit).await;
        let kinds: Vec<&str> = nodes.iter().map(|n| n.node["kind"].as_str().unwrap()).collect();
        for k in [
            "service",
            "account",
            "databases",
            "database",
            "containers",
            "container",
            "documents",
            "document",
            "sprocs",
            "triggers",
            "udfs",
        ] {
            assert!(kinds.contains(&k), "no {k} among {kinds:?}");
        }
    }
}

#[tokio::test]
async fn a_container_shows_its_definition_and_documents_filter_by_partition_key() {
    let h = start().await;
    fill(&h).await;
    let c = insp(&h, "/cosmos/acct1/databases/shop/containers/orders").await.json();
    assert_eq!(c["properties"]["partitionKey"]["paths"], json!(["/customer"]));
    assert_eq!(c["properties"]["defaultTtl"], 3600);
    assert_eq!(c["properties"]["uniqueKeys"], json!([["/ref"]]));
    assert_eq!(c["properties"]["documents"], 6);
    assert!(c["properties"]["indexingPolicy"].is_object());
    assert_eq!(c["properties"]["changeFeed"]["latestVersionEntries"], 6);
    let all = insp(&h, "/cosmos/acct1/databases/shop/containers/orders/documents").await.json();
    assert_eq!(all["items"].as_array().unwrap().len(), 6);
    let ann = insp(&h, "/cosmos/acct1/databases/shop/containers/orders/documents?partitionKey=%5B%22ann%22%5D").await.json();
    let ids: Vec<&str> = ann["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(ids, ["o1", "o2"]);
    // a bare word works too, and a partition no document has is an empty page
    let bob = insp(&h, "/cosmos/acct1/databases/shop/containers/orders/documents?partitionKey=bob").await.json();
    assert_eq!(bob["items"].as_array().unwrap().len(), 1);
    let none = insp(&h, "/cosmos/acct1/databases/shop/containers/orders/documents?partitionKey=nobody").await.json();
    assert!(none["items"].as_array().unwrap().is_empty());
    // one document: o1 exists in two partitions, so the id alone is ambiguous
    assert_eq!(insp(&h, "/cosmos/acct1/databases/shop/containers/orders/documents/o1").await.status, 400);
    let d = insp(&h, "/cosmos/acct1/databases/shop/containers/orders/documents/o1?partitionKey=bob").await.json();
    assert_eq!(d["properties"]["document"]["n"], 3);
    let o3 = insp(&h, "/cosmos/acct1/databases/shop/containers/orders/documents/o3").await.json();
    assert_eq!(o3["properties"]["document"]["customer"], "cy");
    // scripts: names and bodies
    let sp = insp(&h, "/cosmos/acct1/databases/shop/containers/orders/sprocs").await.json();
    assert_eq!(sp["items"][0]["name"], "hello");
    // what the client stored is under `properties`
    assert!(sp["items"][0]["properties"]["body"].as_str().unwrap().contains("setBody"));
    let tr = insp(&h, "/cosmos/acct1/databases/shop/containers/orders/triggers").await.json();
    assert_eq!(tr["items"][0]["properties"]["triggerType"], "Pre");
    assert_eq!(insp(&h, "/cosmos/acct1/databases/shop/containers/orders/udfs").await.json()["items"][0]["name"], "tax");
    // the service node
    assert_eq!(insp(&h, "/cosmos").await.json()["properties"]["accounts"], 1);
}

#[tokio::test]
async fn looking_changes_nothing_and_a_missing_account_is_not_made() {
    let h = start().await;
    fill(&h).await;
    let view = || async {
        let mut v: Vec<Value> = Vec::new();
        v.push(h.req("GET", "dbs/shop/colls/orders", &[], "").await.json());
        v.push(h.req("GET", "dbs/shop/colls/orders/docs", &[("x-ms-documentdb-partitionkey", "[\"ann\"]")], "").await.json());
        let feed = h.req("GET", "dbs/shop/colls/orders/docs", &[("a-im", "Incremental feed")], "").await;
        v.push(json!(feed.header("etag")));
        v
    };
    let before = view().await;
    let lsn_before = insp(&h, "/cosmos/acct1/databases/shop/containers/orders").await.json()["properties"]["changeFeed"].clone();
    walk(&h.srv, "", 1).await;
    walk(&h.srv, "/cosmos", 3).await;
    assert_eq!(before, view().await);
    assert_eq!(lsn_before, insp(&h, "/cosmos/acct1/databases/shop/containers/orders").await.json()["properties"]["changeFeed"]);
    for p in [
        "/cosmos/ghost",
        "/cosmos/ghost/databases",
        "/cosmos/acct1/databases/nope",
        "/cosmos/acct1/databases/shop/containers/nope",
        "/cosmos/acct1/databases/shop/containers/orders/documents/missing",
        "/cosmos/acct1/databases/shop/containers/orders/other",
    ] {
        assert_eq!(insp(&h, p).await.status, 404, "{p}");
    }
    assert_eq!(insp(&h, "/cosmos").await.json()["properties"]["accounts"], 1);
}

#[tokio::test]
async fn a_document_past_its_time_to_live_is_left_out_and_not_removed() {
    let h = start().await;
    fill(&h).await;
    let r = h.doc("shop", "orders", &json!("short"), &json!({"id": "brief", "customer": "short", "ref": "s", "ttl": 1})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    let list = insp(&h, "/cosmos/acct1/databases/shop/containers/orders/documents").await.json();
    let ids: Vec<&str> = list["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(!ids.contains(&"brief"), "{ids:?}");
    // the document is still stored: only a request of the service removes it
    let c = insp(&h, "/cosmos/acct1/databases/shop/containers/orders").await.json();
    assert_eq!(c["properties"]["documents"], 7);
    assert_eq!(insp(&h, "/cosmos/acct1/databases/shop/containers/orders/documents/brief").await.status, 404);
}

/// A client may store any property on a stored procedure, a trigger or a function. The page builds a link from the `path` of a row and the title
/// from its `name`: what the client stored is under `properties` and cannot be mistaken for either.
#[tokio::test]
async fn what_a_script_resource_stores_is_a_value_and_cannot_set_the_path_or_name_of_its_row() {
    let h = start().await;
    fill(&h).await;
    let r = h
        .json(
            "POST",
            "dbs/shop/colls/orders/sprocs",
            &[],
            &json!({"id": "evil", "body": "function(){}", "path": "/blob/other/containers", "name": "innocent", "rid": "x", "timestamp": "never"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = insp(&h, "/cosmos/acct1/databases/shop/containers/orders/sprocs").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let rows = r.json()["items"].as_array().unwrap().clone();
    let evil = rows.iter().find(|row| row["name"] == "evil").unwrap_or_else(|| panic!("{rows:?}"));
    assert!(evil.get("path").is_none(), "{evil}");
    assert_eq!(evil["properties"]["path"], "/blob/other/containers");
    assert_eq!(evil["properties"]["name"], "innocent");
    assert_eq!(evil["properties"]["body"], "function(){}");
    assert_ne!(evil["timestamp"], "never");
    // the other rows have the same shape
    for row in &rows {
        assert!(row["properties"].is_object() && row["rid"].is_string() && row["name"].is_string(), "{row}");
    }
}
