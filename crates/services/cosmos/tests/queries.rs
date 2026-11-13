mod common;

use common::{start, H};
use serde_json::{json, Value};

async fn seed(h: &H) {
    h.db("d").await;
    h.coll("d", "c", "/cat").await;
    for (i, cat, n) in [("a", "x", 3), ("b", "y", 1), ("c", "x", 2), ("d", "z", 5), ("e", "y", 4)] {
        let r = h.doc("d", "c", &json!(cat), &json!({"id": i, "cat": cat, "n": n})).await;
        assert_eq!(r.status, 201);
    }
}

const CROSS: (&str, &str) = ("x-ms-documentdb-query-enablecrosspartition", "True");

#[tokio::test]
async fn single_and_cross_partition_queries() {
    let h = start().await;
    seed(&h).await;
    let r = h.query("d", "c", &[("x-ms-documentdb-partitionkey", "[\"x\"]")], json!({"query": "SELECT c.id FROM c ORDER BY c.n"})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["Documents"], json!([{"id": "c"}, {"id": "a"}]));
    assert_eq!(r.header("x-ms-item-count"), Some("2"));
    // cross partition, simple: served directly
    let q = json!({"query": "SELECT c.id FROM c WHERE c.n > @m", "parameters": [{"name": "@m", "value": 2}]});
    let r = h.query("d", "c", &[CROSS], q).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["_count"], 3);
    // without the flag and without a key: 400
    let r = h.query("d", "c", &[], json!({"query": "SELECT * FROM c"})).await;
    assert_eq!(r.status, 400);
    // syntax errors come back as 400 with the engine's error document
    let r = h.query("d", "c", &[CROSS], json!({"query": "SELEKT * FROM c"})).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["message"].as_str().unwrap().contains("SC1001"));
}

#[tokio::test]
async fn cross_partition_query_plan_round_trip() {
    let h = start().await;
    seed(&h).await;
    let q = json!({"query": "SELECT c.id FROM c ORDER BY c.n DESC"});
    // 1. the first attempt is refused with sub-status 1004 and carries the plan
    let r = h.query("d", "c", &[CROSS], q.clone()).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.header("x-ms-substatus"), Some("1004"));
    let info: Value = serde_json::from_str(r.json()["additionalErrorInfo"].as_str().unwrap()).unwrap();
    assert_eq!(info["queryInfo"]["orderBy"], json!(["Descending"]));
    // 2. the explicit query plan request
    let r = h
        .req(
            "POST",
            "dbs/d/colls/c/docs",
            &[("content-type", "application/query+json"), ("x-ms-cosmos-is-query-plan-request", "True")],
            &q.to_string(),
        )
        .await;
    assert_eq!(r.status, 200);
    let plan = r.json();
    assert_eq!(plan["queryRanges"][0]["max"], "FF");
    let rewritten = plan["queryInfo"]["rewrittenQuery"].as_str().unwrap().to_string();
    // 3. the partition range request with the rewritten query
    let rq = json!({"query": rewritten.replace("{documentdb-formattableorderbyquery-filter}", "true")});
    let r = h.query("d", "c", &[CROSS, ("x-ms-documentdb-partitionkeyrangeid", "0")], rq).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let docs = r.json()["Documents"].clone();
    let order: Vec<i64> = docs.as_array().unwrap().iter().map(|d| d["orderByItems"][0]["item"].as_i64().unwrap()).collect();
    assert_eq!(order, [5, 4, 3, 2, 1]);
    assert_eq!(docs[0]["payload"], json!({"id": "d"}));
    // the partition key ranges
    let r = h.req("GET", "dbs/d/colls/c/pkranges", &[], "").await;
    assert_eq!(r.json()["PartitionKeyRanges"][0]["minInclusive"], "");
    let etag = r.header("etag").unwrap().to_string();
    assert_eq!(h.req("GET", "dbs/d/colls/c/pkranges", &[("if-none-match", &etag)], "").await.status, 304);
}

/// MIN and MAX are answered by the partitions in the form the asking SDK reads: the JavaScript SDK
/// (query version 1.4, composite aggregates declared) wants `{"min", "count"}`, the Python SDK (1.0) the bare value.
async fn plan_of(h: &common::H, q: &Value, extra: &[(&str, &str)]) -> Value {
    let mut hdrs = vec![("content-type", "application/query+json"), ("x-ms-cosmos-is-query-plan-request", "True")];
    hdrs.extend_from_slice(extra);
    h.req("POST", "dbs/d/colls/c/docs", &hdrs, &q.to_string()).await.json()["queryInfo"].clone()
}

#[tokio::test]
async fn min_and_max_plan_follows_the_sdk_that_asks() {
    let h = start().await;
    seed(&h).await;
    let q = json!({"query": "SELECT VALUE MIN(c.n) FROM c"});
    let features = "Aggregate,CompositeAggregate,Distinct,MultipleOrderBy,OffsetAndLimit,OrderBy,Top,GroupBy,MultipleAggregates";
    let js = plan_of(&h, &q, &[("x-ms-cosmos-query-version", "1.4"), ("x-ms-cosmos-supported-query-features", features)]).await;
    assert_eq!(js["aggregates"], json!(["Min"]));
    assert!(js["rewrittenQuery"].as_str().unwrap().contains("\"min\": MIN(c.n), \"count\": COUNT(c.n)"), "{js}");
    let py = plan_of(
        &h,
        &q,
        &[("x-ms-cosmos-query-version", "1.0"), ("x-ms-cosmos-supported-query-features", "Aggregate,CompositeAggregate,Distinct")],
    )
    .await;
    assert_eq!(py["rewrittenQuery"], "SELECT VALUE [{\"item\": MIN(c.n)}]\nFROM c");
    let none = plan_of(&h, &q, &[]).await;
    assert_eq!(none["rewrittenQuery"], py["rewrittenQuery"]);
    // the 1004 answer to a cross partition query carries the same plan
    let r = h
        .query("d", "c", &[CROSS, ("x-ms-cosmos-query-version", "1.4"), ("x-ms-cosmos-supported-query-features", features)], q.clone())
        .await;
    let info: Value = serde_json::from_str(r.json()["additionalErrorInfo"].as_str().unwrap()).unwrap();
    assert!(info["queryInfo"]["rewrittenQuery"].as_str().unwrap().contains("\"min\":"));
    // and the rewritten text runs on the partition range
    let rq = json!({"query": js["rewrittenQuery"].as_str().unwrap()});
    let r = h.query("d", "c", &[CROSS, ("x-ms-documentdb-partitionkeyrangeid", "0")], rq).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["Documents"], json!([[{"item": {"min": 1, "count": 5}}]]));
}

#[tokio::test]
async fn paging_with_continuation_tokens() {
    let h = start().await;
    seed(&h).await;
    let q = json!({"query": "SELECT VALUE c.id FROM c"});
    let mut got = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..10 {
        let mut hdrs = vec![CROSS, ("x-ms-max-item-count", "2")];
        let t;
        if let Some(tok) = &token {
            t = tok.clone();
            hdrs.push(("x-ms-continuation", &t));
        }
        let r = h.query("d", "c", &hdrs, q.clone()).await;
        assert_eq!(r.status, 200);
        got.extend(r.json()["Documents"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()));
        token = r.header("x-ms-continuation").map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    got.sort();
    assert_eq!(got, ["a", "b", "c", "d", "e"]);
}

#[tokio::test]
async fn query_on_resource_types() {
    let h = start().await;
    h.db("one").await;
    h.db("two").await;
    let q = json!({"query": "SELECT * FROM root r WHERE r.id=@id", "parameters": [{"name": "@id", "value": "two"}]});
    let r = h.req("POST", "dbs", &[("content-type", "application/query+json"), ("x-ms-documentdb-isquery", "True")], &q.to_string()).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["Databases"].as_array().unwrap().len(), 1);
    let r = h.req("GET", "dbs", &[("x-ms-max-item-count", "1")], "").await;
    assert_eq!(r.json()["_count"], 1);
    assert!(r.header("x-ms-continuation").is_some());
}
