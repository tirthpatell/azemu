//! Change feed and continuation token checks:
//! page size in bytes, bad dates and positions, a reader that falls behind the history, and the kept query results.

mod common;

use common::{start, H};
use serde_json::{json, Value};

async fn setup() -> H {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h
}

const INCREMENTAL: (&str, &str) = ("A-IM", "Incremental Feed");
const FULL: [(&str, &str); 2] = [("A-IM", "Full-Fidelity Feed"), ("x-ms-cosmos-changefeed-wire-format-version", "2021-09-15")];

#[tokio::test]
async fn a_change_feed_page_stops_at_four_megabytes_whatever_the_item_count() {
    let h = setup().await;
    let pad = "x".repeat(600_000);
    for i in 0..12 {
        assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": format!("d{i:02}"), "pk": "p", "pad": pad})).await.status, 201);
    }
    let mut seen: Vec<String> = Vec::new();
    let mut token: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut hdrs = vec![INCREMENTAL, ("x-ms-max-item-count", "100000")];
        let t;
        if let Some(tok) = &token {
            t = tok.clone();
            hdrs.push(("If-None-Match", &t));
        }
        let r = h.req("GET", "dbs/d/colls/c/docs", &hdrs, "").await;
        if r.status == 304 {
            break;
        }
        assert_eq!(r.status, 200, "{}", &r.text()[..200.min(r.text().len())]);
        let docs = r.json()["Documents"].as_array().unwrap().clone();
        assert!(!docs.is_empty() && docs.len() <= 7, "a page of {} documents of 600 KB is more than 4 MB", docs.len());
        assert_eq!(r.header("x-ms-item-count").unwrap().parse::<usize>().unwrap(), docs.len());
        seen.extend(docs.iter().map(|d| d["id"].as_str().unwrap().to_string()));
        token = r.header("etag").map(str::to_string);
        pages += 1;
        assert!(pages < 10);
    }
    let want: Vec<String> = (0..12).map(|i| format!("d{i:02}")).collect();
    assert_eq!(seen, want, "every document once, in order");
    assert!(pages >= 2);
}

#[tokio::test]
async fn bad_dates_and_positions_are_errors_not_a_silent_restart() {
    let h = setup().await;
    h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p"})).await;
    // a date that cannot be read
    let r = h.req("GET", "dbs/d/colls/c/docs", &[INCREMENTAL, ("If-Modified-Since", "yesterday-ish")], "").await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("If-Modified-Since"));
    let r = h.req("GET", "dbs/d/colls/c/docs", &[INCREMENTAL, ("If-Modified-Since", "Thu, 01 Jan 1970 00:00:00 GMT")], "").await;
    assert_eq!(r.status, 200);
    // a position the container has not reached: another container's token, or one from before a restart
    let r = h.req("GET", "dbs/d/colls/c/docs", &[INCREMENTAL, ("If-None-Match", "\"9999\"")], "").await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("ahead of this container"), "{}", r.text());
    let mut full = FULL.to_vec();
    full.push(("If-None-Match", "\"9999\""));
    h.req("GET", "dbs/d/colls/c/docs", &[FULL[0], FULL[1], ("If-None-Match", "*")], "").await; // starts the history
    let r = h.req("GET", "dbs/d/colls/c/docs", &full, "").await;
    assert_eq!(r.status, 400, "{}", r.text());
    // not a number
    let r = h.req("GET", "dbs/d/colls/c/docs", &[INCREMENTAL, ("If-None-Match", "\"abc\"")], "").await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn a_reader_that_falls_behind_the_bounded_history_is_told_so() {
    azemu_cosmos::CosmosService::set_change_log_record_limit(5);
    let h = setup().await;
    let r = h.req("GET", "dbs/d/colls/c/docs", &[FULL[0], FULL[1], ("If-None-Match", "*")], "").await;
    assert_eq!(r.status, 304);
    let old = r.header("etag").unwrap().to_string();
    for i in 0..12 {
        h.doc("d", "c", &json!("p"), &json!({"id": format!("d{i}"), "pk": "p"})).await;
    }
    // the history keeps the last 5 or 6 changes: a reader at the start has missed some
    let r = h.req("GET", "dbs/d/colls/c/docs", &[FULL[0], FULL[1], ("If-None-Match", &old)], "").await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("older than the retained history"), "{}", r.text());
    // a reader that kept up reads what is there
    let r = h.req("GET", "dbs/d/colls/c/docs", &[FULL[0], FULL[1], ("If-None-Match", "*")], "").await;
    let tok = r.header("etag").unwrap().to_string();
    h.doc("d", "c", &json!("p"), &json!({"id": "late", "pk": "p"})).await;
    let r = h.req("GET", "dbs/d/colls/c/docs", &[FULL[0], FULL[1], ("If-None-Match", &tok)], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["Documents"][0]["current"]["id"], "late");
    azemu_cosmos::CosmosService::set_change_log_record_limit(100_000);
}

async fn first_page(h: &H, q: &str) -> (Vec<Value>, Option<String>) {
    let r = h
        .query("d", "c", &[("x-ms-max-item-count", "2"), ("x-ms-documentdb-query-enablecrosspartition", "true")], json!({"query": q}))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    (r.json()["Documents"].as_array().unwrap().clone(), r.header("x-ms-continuation").map(str::to_string))
}

#[tokio::test]
async fn a_token_of_a_deleted_container_does_not_read_the_old_rows_of_a_new_one() {
    let h = setup().await;
    for i in 0..6 {
        h.doc("d", "c", &json!("p"), &json!({"id": format!("old{i}"), "pk": "p"})).await;
    }
    let (page, token) = first_page(&h, "SELECT VALUE c.id FROM c").await;
    assert_eq!(page, vec![json!("old0"), json!("old1")]);
    let token = token.unwrap();
    assert_eq!(h.req("DELETE", "dbs/d/colls/c", &[], "").await.status, 204);
    h.coll("d", "c", "/pk").await;
    for i in 0..6 {
        h.doc("d", "c", &json!("p"), &json!({"id": format!("new{i}"), "pk": "p"})).await;
    }
    let r = h
        .query(
            "d",
            "c",
            &[("x-ms-max-item-count", "2"), ("x-ms-documentdb-query-enablecrosspartition", "true"), ("x-ms-continuation", &token)],
            json!({"query": "SELECT VALUE c.id FROM c"}),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["Documents"], json!(["new2", "new3"]), "the kept rows of the deleted container must not be served");
}

#[tokio::test]
async fn thousands_of_paged_queries_that_are_never_read_keep_a_bounded_number_of_results() {
    let h = setup().await;
    for i in 0..6 {
        h.doc("d", "c", &json!("p"), &json!({"id": format!("d{i}"), "pk": "p", "n": i})).await;
    }
    for i in 0..700 {
        // a different query each time, so each result is kept; the first page is read, the rest never
        let (page, token) = first_page(&h, &format!("SELECT VALUE c.id FROM c WHERE c.n >= 0 AND {i} = {i}")).await;
        assert_eq!(page.len(), 2);
        assert!(token.is_some());
    }
    let kept = h.svc.kept_results();
    assert!(kept <= 256 && kept > 0, "{kept} results are kept");
}
