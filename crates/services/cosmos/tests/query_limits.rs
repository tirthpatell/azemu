//! A query that is too expensive is refused with a clear 400, and only a few queries run at the
//! same time: the others wait without holding a thread.

mod common;

use azemu_cosmos::CosmosService;
use common::start;
use serde_json::json;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_query_over_the_memory_budget_is_a_400_that_says_so() {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    // one document with a 2000 element array: a self join keeps 4 million rows if it has to
    let arr: Vec<i32> = (0..2000).collect();
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "1", "pk": "p", "arr": arr})).await.status, 201);
    let pk = [("x-ms-documentdb-partitionkey", "[\"p\"]")];
    // ORDER BY has to keep every row, and there are 4 million of them
    let q = json!({"query": "SELECT a, b FROM c JOIN a IN c.arr JOIN b IN c.arr ORDER BY a DESC"});
    let r = h.query("d", "c", &pk, q).await;
    assert_eq!(r.status, 400, "{}", r.text());
    let text = r.text();
    assert!(text.contains("memory") && text.contains("TOP"), "the message should say what happened and what to do: {text}");
    assert_eq!(r.json()["code"], "BadRequest");
    // the same join, counted, needs nothing to be kept
    let q = json!({"query": "SELECT VALUE COUNT(1) FROM c JOIN a IN c.arr JOIN b IN c.arr"});
    let r = h.query("d", "c", &pk, q).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["Documents"][0], 4_000_000);
    // and with TOP the result is five rows after five rows of work
    let q = json!({"query": "SELECT TOP 5 VALUE a * 10000 + b FROM c JOIN a IN c.arr JOIN b IN c.arr"});
    let r = h.query("d", "c", &pk, q).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["Documents"], json!([0, 1, 2, 3, 4]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_a_few_queries_run_at_once_and_the_rest_wait() {
    let h: Arc<common::H> = Arc::new(start().await);
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    let arr: Vec<i32> = (0..400).collect();
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "1", "pk": "p", "arr": arr})).await.status, 201);
    let mut tasks = Vec::new();
    for _ in 0..24 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let q = json!({"query": "SELECT VALUE COUNT(1) FROM c JOIN a IN c.arr JOIN b IN c.arr"});
            let r = h.query("d", "c", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], q).await;
            (r.status, r.json()["Documents"][0].as_i64())
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), (200, Some(160_000)));
    }
    let svc: &CosmosService = &h.svc;
    let peak = svc.peak_running_queries();
    assert!((1..=4).contains(&peak), "peak of running queries: {peak}");
    assert_eq!(svc.query_executions(), 24);
}
