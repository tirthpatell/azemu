//! A long query runs without the service lock, so point reads and writes are not held up.

mod common;

use std::time::{Duration, Instant};

use common::start;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_query_does_not_block_point_reads_or_writes() {
    let h = std::sync::Arc::new(start().await);
    h.db("d").await;
    h.coll("d", "big", "/pk").await;
    h.coll("d", "small", "/pk").await;
    let arr: Vec<i32> = (0..170).collect();
    assert_eq!(h.doc("d", "big", &json!("p"), &json!({"id": "1", "pk": "p", "arr": arr})).await.status, 201);
    assert_eq!(h.doc("d", "small", &json!("p"), &json!({"id": "1", "pk": "p"})).await.status, 201);

    let q = json!({"query": "SELECT VALUE COUNT(1) FROM c JOIN a IN c.arr JOIN b IN c.arr JOIN d IN c.arr"});
    let hdrs = [("x-ms-documentdb-partitionkey", "[\"p\"]")];
    // how long the query takes alone
    let t = Instant::now();
    let r = h.query("d", "big", &hdrs, q.clone()).await;
    let alone = t.elapsed();
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["Documents"][0], 170 * 170 * 170);
    assert!(alone > Duration::from_millis(200), "the query is too quick to show anything: {alone:?}");

    let h2 = h.clone();
    let q2 = q.clone();
    let running = tokio::spawn(async move { h2.query("d", "big", &hdrs, q2).await.status });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t = Instant::now();
    for i in 0..20 {
        let r = h.req("GET", "dbs/d/colls/small/docs/1", &hdrs, "").await;
        assert_eq!(r.status, 200);
        let r = h.doc("d", "small", &json!("p"), &json!({"id": format!("w{i}"), "pk": "p"})).await;
        assert_eq!(r.status, 201);
    }
    let during = t.elapsed();
    assert!(!running.is_finished(), "the query ended before the reads: {during:?} vs {alone:?}");
    assert!(during < alone / 2, "40 quick requests took {during:?} while the query takes {alone:?}");
    assert_eq!(running.await.unwrap(), 200);
}
