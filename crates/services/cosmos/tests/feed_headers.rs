//! Headers a client reads from every feed answer, 200 and 304.
//!
//! The .NET SDK (`NetworkAttachedDocumentContainer`) does `int.Parse(Headers.ItemCount)` on every change feed
//! and read feed answer and builds the continuation from `etag` without a null check, so a 304 without
//! `x-ms-item-count` throws `ArgumentNullException` in the client (see `docs/sdk-notes.md`, .NET, Cosmos DB).

mod common;

use common::{start, H};
use serde_json::json;

async fn setup() -> H {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h
}

/// What a container-scoped feed answer (200 or 304) must carry.
fn check_common(r: &azemu_core::testing::TestResponse, what: &str) {
    for name in ["x-ms-activity-id", "x-ms-request-charge", "x-ms-session-token", "lsn", "x-ms-global-committed-lsn", "x-ms-item-count"] {
        assert!(r.header(name).is_some(), "{what} (status {}): header {name} is missing; headers: {:?}", r.status, r.headers);
    }
    let session = r.header("x-ms-session-token").unwrap();
    assert!(session.starts_with("0:1#"), "{what}: session token {session}");
    r.header("x-ms-item-count").unwrap().parse::<usize>().unwrap_or_else(|_| panic!("{what}: item count is not a number"));
    r.header("x-ms-request-charge").unwrap().parse::<f64>().unwrap_or_else(|_| panic!("{what}: charge is not a number"));
}

#[tokio::test]
async fn latest_version_change_feed_has_the_headers_on_200_and_304() {
    let h = setup().await;
    h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p"})).await;
    let aim = [("A-IM", "Incremental Feed")];
    let r = h.req("GET", "dbs/d/colls/c/docs", &aim, "").await;
    assert_eq!(r.status, 200);
    check_common(&r, "change feed 200");
    assert_eq!(r.header("x-ms-item-count"), Some("1"));
    let etag = r.header("etag").expect("etag on 200").to_string();

    let r = h.req("GET", "dbs/d/colls/c/docs", &[("A-IM", "Incremental Feed"), ("If-None-Match", &etag)], "").await;
    assert_eq!(r.status, 304);
    check_common(&r, "change feed 304");
    assert_eq!(r.header("x-ms-item-count"), Some("0"), "nothing new is an item count of 0");
    assert_eq!(r.header("etag"), Some(etag.as_str()), "the continuation stays where it was");

    // from now: 304 with a position
    let r = h.req("GET", "dbs/d/colls/c/docs", &[("A-IM", "Incremental Feed"), ("If-None-Match", "*")], "").await;
    assert_eq!(r.status, 304);
    check_common(&r, "change feed from now");
    assert!(r.header("etag").is_some());

    // a partition key and an empty container
    h.coll("d", "empty", "/pk").await;
    let r = h.req("GET", "dbs/d/colls/empty/docs", &aim, "").await;
    assert_eq!(r.status, 304);
    check_common(&r, "change feed of an empty container");
    assert!(r.header("etag").is_some());
}

#[tokio::test]
async fn all_versions_and_deletes_change_feed_has_the_headers_on_200_and_304() {
    let h = setup().await;
    let hdr = |tok: &'static str| {
        [("A-IM", "Full-Fidelity Feed"), ("x-ms-cosmos-changefeed-wire-format-version", "2021-09-15"), ("If-None-Match", tok)]
    };
    let r = h.req("GET", "dbs/d/colls/c/docs", &hdr("*"), "").await;
    assert_eq!(r.status, 304);
    check_common(&r, "all versions from now");
    assert_eq!(r.header("x-ms-item-count"), Some("0"));
    let etag = r.header("etag").unwrap().to_string();
    h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p"})).await;
    let r = h
        .req(
            "GET",
            "dbs/d/colls/c/docs",
            &[("A-IM", "Full-Fidelity Feed"), ("x-ms-cosmos-changefeed-wire-format-version", "2021-09-15"), ("If-None-Match", &etag)],
            "",
        )
        .await;
    assert_eq!(r.status, 200);
    check_common(&r, "all versions 200");
    assert_eq!(r.header("x-ms-item-count"), Some("1"));
    let etag = r.header("etag").unwrap().to_string();
    let r = h
        .req(
            "GET",
            "dbs/d/colls/c/docs",
            &[("A-IM", "Full-Fidelity Feed"), ("x-ms-cosmos-changefeed-wire-format-version", "2021-09-15"), ("If-None-Match", &etag)],
            "",
        )
        .await;
    assert_eq!(r.status, 304);
    check_common(&r, "all versions 304");
    assert_eq!(r.header("x-ms-item-count"), Some("0"));
}

#[tokio::test]
async fn read_feeds_and_queries_have_an_item_count_even_when_empty() {
    let h = setup().await;
    // the document feed of an empty container
    let r = h.req("GET", "dbs/d/colls/c/docs", &[], "").await;
    assert_eq!(r.status, 200);
    check_common(&r, "document feed");
    assert_eq!(r.header("x-ms-item-count"), Some("0"));
    // a query with no result
    let r = h.query("d", "c", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], json!({"query": "SELECT * FROM c"})).await;
    assert_eq!(r.status, 200);
    check_common(&r, "query page");
    assert_eq!(r.header("x-ms-item-count"), Some("0"));
    // metadata feeds
    for path in ["dbs", "dbs/d/colls", "dbs/d/users", "offers", "dbs/d/colls/c/sprocs", "dbs/d/colls/c/pkranges"] {
        let r = h.req("GET", path, &[], "").await;
        assert_eq!(r.status, 200, "{path}: {}", r.text());
        assert!(r.header("x-ms-item-count").is_some(), "{path}: x-ms-item-count missing");
        assert!(r.header("x-ms-request-charge").is_some() && r.header("x-ms-activity-id").is_some(), "{path}");
    }
}

#[tokio::test]
async fn partition_key_ranges_304_has_the_headers() {
    let h = setup().await;
    let r = h.req("GET", "dbs/d/colls/c/pkranges", &[], "").await;
    assert_eq!(r.status, 200);
    check_common_pk(&r);
    let etag = r.header("etag").expect("etag").to_string();
    let r = h.req("GET", "dbs/d/colls/c/pkranges", &[("If-None-Match", &etag)], "").await;
    assert_eq!(r.status, 304);
    check_common_pk(&r);
    assert_eq!(r.header("x-ms-item-count"), Some("0"));
    assert_eq!(r.header("etag"), Some(etag.as_str()));
}

fn check_common_pk(r: &azemu_core::testing::TestResponse) {
    for name in ["x-ms-activity-id", "x-ms-request-charge", "x-ms-item-count", "etag", "x-ms-session-token", "lsn"] {
        assert!(r.header(name).is_some(), "pkranges (status {}): header {name} is missing", r.status);
    }
}
