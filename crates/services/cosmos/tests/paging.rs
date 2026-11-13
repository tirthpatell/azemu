//! Paging: one query run serves every page; tokens still work when the kept result is gone.

mod common;

use common::{start, H};
use serde_json::{json, Value};

async fn fill(h: &H, n: usize) {
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    for i in 0..n {
        let r =
            h.doc("d", "c", &json!(format!("p{}", i % 3)), &json!({"id": format!("{i:05}"), "pk": format!("p{}", i % 3), "n": i})).await;
        assert_eq!(r.status, 201);
    }
}

const XP: [(&str, &str); 1] = [("x-ms-documentdb-query-enablecrosspartition", "True")];

async fn page(h: &H, q: &str, max: usize, token: Option<&str>) -> (Vec<Value>, Option<String>) {
    let m = max.to_string();
    let mut hdrs: Vec<(&str, &str)> = vec![("x-ms-max-item-count", &m), XP[0], ("x-ms-documentdb-partitionkeyrangeid", "0")];
    if let Some(t) = token {
        hdrs.push(("x-ms-continuation", t));
    }
    let r = h.query("d", "c", &hdrs, json!({"query": q})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    (r.json()["Documents"].as_array().unwrap().clone(), r.header("x-ms-continuation").map(str::to_string))
}

#[tokio::test]
async fn reading_every_page_runs_the_query_once() {
    let h = start().await;
    fill(&h, 300).await;
    let before = h.svc.query_executions();
    let mut seen = Vec::new();
    let mut token: Option<String> = None;
    let mut pages = 0;
    loop {
        let (docs, next) = page(&h, "SELECT * FROM c ORDER BY c.n", 10, token.as_deref()).await;
        pages += 1;
        seen.extend(docs.iter().map(|d| d["n"].as_i64().unwrap()));
        token = next;
        if token.is_none() {
            break;
        }
    }
    assert_eq!(pages, 30);
    assert_eq!(seen, (0..300).collect::<Vec<i64>>());
    assert_eq!(h.svc.query_executions() - before, 1, "the query ran once for 30 pages");
}

#[tokio::test]
async fn a_kept_result_is_a_snapshot_and_a_token_without_one_re_runs() {
    let h = start().await;
    fill(&h, 30).await;
    let (first, t1) = page(&h, "SELECT * FROM c ORDER BY c.n", 10, None).await;
    assert_eq!(first.len(), 10);
    let t1 = t1.unwrap();
    // change the data between the pages
    let r = h
        .json(
            "PUT",
            "dbs/d/colls/c/docs/00015",
            &[("x-ms-documentdb-partitionkey", "[\"p0\"]")],
            &json!({"id": "00015", "pk": "p0", "n": 1015}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let (second, _) = page(&h, "SELECT * FROM c ORDER BY c.n", 10, Some(&t1)).await;
    let ns: Vec<i64> = second.iter().map(|d| d["n"].as_i64().unwrap()).collect();
    assert_eq!(ns, (10..20).collect::<Vec<i64>>(), "served from the kept result: the old value 15 is still there");
    // the same token with the kept result removed: the query runs again, 10 rows are skipped
    let unknown = t1.replace("#Q:", "#Q:9");
    assert_ne!(unknown, t1);
    let (second, _) = page(&h, "SELECT * FROM c ORDER BY c.n", 10, Some(&unknown)).await;
    let ns: Vec<i64> = second.iter().map(|d| d["n"].as_i64().unwrap()).collect();
    assert_eq!(ns, vec![10, 11, 12, 13, 14, 16, 17, 18, 19, 20], "re-run: the new data, 15 moved to the end");
    // a token made for another query does not read the kept result
    let (other, _) = page(&h, "SELECT * FROM c ORDER BY c.n DESC", 10, Some(&t1)).await;
    let ns: Vec<i64> = other.iter().map(|d| d["n"].as_i64().unwrap()).collect();
    assert_eq!(ns, vec![20, 19, 18, 17, 16, 14, 13, 12, 11, 10], "a token made for another query is only a skip count");
}

#[tokio::test]
async fn cross_partition_and_plain_queries_page_from_the_kept_result() {
    let h = start().await;
    fill(&h, 100).await;
    let before = h.svc.query_executions();
    // plain query across partitions: all pages come from one run, no duplicates, none missing
    let mut seen = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let (docs, next) = page(&h, "SELECT VALUE c.n FROM c WHERE c.n >= 10", 7, token.as_deref()).await;
        seen.extend(docs.iter().map(|d| d.as_i64().unwrap()));
        token = next;
        if token.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 90);
    seen.sort();
    assert_eq!(seen, (10..100).collect::<Vec<i64>>());
    assert_eq!(h.svc.query_executions() - before, 1);
}

#[tokio::test]
async fn token_of_a_reset_service_still_resumes() {
    let h = start().await;
    fill(&h, 20).await;
    let (_, t) = page(&h, "SELECT * FROM c ORDER BY c.n", 5, None).await;
    let t = t.unwrap();
    assert!(t.contains("#Q:"));
    // another request that is not a page of the query does not disturb the kept result
    let (rest, _) = page(&h, "SELECT * FROM c ORDER BY c.n", 5, Some(&t)).await;
    assert_eq!(rest.iter().map(|d| d["n"].as_i64().unwrap()).collect::<Vec<_>>(), vec![5, 6, 7, 8, 9]);
    // the old shape of a token (only a row count) is understood too
    let old = json!({"token": "+RID:~azemu#SKIP:15", "range": {"min": "", "max": "FF"}}).to_string();
    let (rest, next) = page(&h, "SELECT * FROM c ORDER BY c.n", 10, Some(&old)).await;
    assert_eq!(rest.iter().map(|d| d["n"].as_i64().unwrap()).collect::<Vec<_>>(), vec![15, 16, 17, 18, 19]);
    assert!(next.is_none());
    // junk is "from the start"
    let (first, _) = page(&h, "SELECT * FROM c ORDER BY c.n", 3, Some("not json")).await;
    assert_eq!(first.len(), 3);
    assert_eq!(first[0]["n"], 0);
}

#[tokio::test]
async fn document_feed_resumes_behind_the_last_document_even_when_data_changes() {
    let h = start().await;
    fill(&h, 30).await;
    let list = |token: Option<String>| {
        let h = &h;
        async move {
            let mut hdrs: Vec<(&str, String)> = vec![("x-ms-max-item-count", "7".into())];
            if let Some(t) = token {
                hdrs.push(("x-ms-continuation", t));
            }
            let hs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let r = h.req("GET", "dbs/d/colls/c/docs", &hs, "").await;
            assert_eq!(r.status, 200, "{}", r.text());
            let ids: Vec<String> =
                r.json()["Documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
            (ids, r.header("x-ms-continuation").map(str::to_string))
        }
    };
    let before = h.svc.query_executions();
    let (first, t) = list(None).await;
    assert_eq!(first.len(), 7);
    // delete a document that was already delivered and one still to come; add one in front of
    // the position and one behind it
    let pk = |p: &str| format!("[\"{p}\"]");
    let first_id = first[0].clone();
    let p_of = |id: &str| format!("p{}", id.parse::<usize>().unwrap() % 3);
    let r =
        h.req("DELETE", &format!("dbs/d/colls/c/docs/{first_id}"), &[("x-ms-documentdb-partitionkey", &pk(&p_of(&first_id)))], "").await;
    assert_eq!(r.status, 204);
    let r = h.doc("d", "c", &json!("p0"), &json!({"id": "00000a", "pk": "p0"})).await;
    assert_eq!(r.status, 201);
    let r = h.doc("d", "c", &json!("p2"), &json!({"id": "zzz", "pk": "p2"})).await;
    assert_eq!(r.status, 201);
    let mut all = first.clone();
    let mut token = t;
    while let Some(t) = token {
        let (ids, next) = list(Some(t)).await;
        all.extend(ids);
        token = next;
    }
    // nothing delivered twice, nothing that existed the whole time is missing
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "a document was delivered twice");
    for i in 1..30 {
        assert!(all.contains(&format!("{i:05}")), "{i} missing");
    }
    assert!(all.contains(&"zzz".to_string()), "a document added behind the position is delivered");
    assert_eq!(h.svc.query_executions() - before, 0);
}

#[tokio::test]
async fn document_feed_of_one_partition_pages_inside_the_partition() {
    let h = start().await;
    fill(&h, 30).await;
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut hdrs: Vec<(&str, String)> = vec![("x-ms-max-item-count", "4".into()), ("x-ms-documentdb-partitionkey", "[\"p1\"]".into())];
        if let Some(t) = &token {
            hdrs.push(("x-ms-continuation", t.clone()));
        }
        let hs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = h.req("GET", "dbs/d/colls/c/docs", &hs, "").await;
        assert_eq!(r.status, 200);
        all.extend(r.json()["Documents"].as_array().unwrap().iter().map(|d| d["n"].as_i64().unwrap()));
        token = r.header("x-ms-continuation").map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    assert_eq!(all, (0..30).filter(|i| i % 3 == 1).collect::<Vec<i64>>());
}

#[tokio::test]
async fn a_page_stops_at_four_megabytes() {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    let big = "x".repeat(1_500_000);
    for i in 0..5 {
        assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": format!("{i}"), "pk": "p", "b": big})).await.status, 201);
    }
    let r = h.req("GET", "dbs/d/colls/c/docs", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], "").await;
    let n = r.json()["Documents"].as_array().unwrap().len();
    assert_eq!(n, 2, "two documents of 1.5 MB fit in 4 MB; the third does not");
    assert!(r.header("x-ms-continuation").is_some());
    let r = h.query("d", "c", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], json!({"query": "SELECT * FROM c"})).await;
    assert_eq!(r.json()["Documents"].as_array().unwrap().len(), 2);
    assert!(r.header("x-ms-continuation").is_some());
}
