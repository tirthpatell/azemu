//! ORDER BY and documents that lack the sort property. Azure serves ORDER BY from the index:
//! under the default policy (`/*`) such documents are left out; with the path indexed
//! explicitly, or with a composite index (needed for several fields), they come back, and
//! undefined sorts before null. Source: Microsoft Learn, "Indexing policies".

mod common;

use common::{start, H};
use serde_json::{json, Value};

async fn container(h: &H, policy: Option<Value>) {
    h.db("d").await;
    let mut body = json!({"id": "c", "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}});
    if let Some(p) = policy {
        body["indexingPolicy"] = p;
    }
    let r = h.json("POST", "dbs/d/colls", &[], &body).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let docs = [
        json!({"id": "undef", "pk": "p"}),
        json!({"id": "null", "pk": "p", "n": null}),
        json!({"id": "false", "pk": "p", "n": false}),
        json!({"id": "true", "pk": "p", "n": true}),
        json!({"id": "one", "pk": "p", "n": 1}),
        json!({"id": "two", "pk": "p", "n": 2}),
        json!({"id": "str", "pk": "p", "n": "a"}),
    ];
    for d in docs {
        assert_eq!(h.doc("d", "c", &json!("p"), &d).await.status, 201);
    }
}

async fn ids(h: &H, q: &str) -> Vec<String> {
    let r = h.query("d", "c", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], json!({"query": q})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["Documents"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn default_policy_leaves_out_documents_without_the_property() {
    let h = start().await;
    container(&h, None).await;
    assert_eq!(ids(&h, "SELECT VALUE c.id FROM c ORDER BY c.n").await, ["null", "false", "true", "one", "two", "str"]);
    assert_eq!(ids(&h, "SELECT VALUE c.id FROM c ORDER BY c.n DESC").await, ["str", "two", "one", "true", "false", "null"]);
}

#[tokio::test]
async fn an_explicitly_indexed_path_returns_them_first_ascending_and_last_descending() {
    let h = start().await;
    container(
        &h,
        Some(json!({"indexingMode": "consistent", "includedPaths": [{"path": "/n/?"}, {"path": "/pk/?"}], "excludedPaths": [{"path": "/*"}]})),
    )
    .await;
    assert_eq!(ids(&h, "SELECT VALUE c.id FROM c ORDER BY c.n").await, ["undef", "null", "false", "true", "one", "two", "str"]);
    assert_eq!(ids(&h, "SELECT VALUE c.id FROM c ORDER BY c.n DESC").await, ["str", "two", "one", "true", "false", "null", "undef"]);
}

#[tokio::test]
async fn several_fields_need_a_composite_index_and_then_documents_without_a_field_are_returned() {
    let h = start().await;
    container(
        &h,
        Some(json!({"indexingMode": "consistent", "includedPaths": [{"path": "/*"}],
                    "compositeIndexes": [[{"path": "/n", "order": "ascending"}, {"path": "/id", "order": "ascending"}]]})),
    )
    .await;
    assert_eq!(ids(&h, "SELECT VALUE c.id FROM c ORDER BY c.n, c.id").await, ["undef", "null", "false", "true", "one", "two", "str"]);
    // the same query without a composite index is refused
    let h2 = start().await;
    container(&h2, None).await;
    let r =
        h2.query("d", "c", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], json!({"query": "SELECT * FROM c ORDER BY c.n, c.id"})).await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn the_rewritten_query_of_the_sdk_follows_the_same_rule() {
    // what the SDK sends to the partition after the query plan
    let h = start().await;
    container(&h, Some(json!({"indexingMode": "consistent", "includedPaths": [{"path": "/n/?"}], "excludedPaths": [{"path": "/*"}]})))
        .await;
    let q = "SELECT c._rid, [{\"item\": c.n}] AS orderByItems, c AS payload FROM c WHERE (true) ORDER BY c.n";
    let r = h
        .query(
            "d",
            "c",
            &[("x-ms-documentdb-partitionkeyrangeid", "0"), ("x-ms-documentdb-query-enablecrosspartition", "True")],
            json!({"query": q}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let docs = r.json()["Documents"].as_array().unwrap().clone();
    assert_eq!(docs.len(), 7);
    assert_eq!(docs[0]["payload"]["id"], "undef");
    assert!(docs[0]["orderByItems"][0].get("item").is_none());
}
