//! Change feed in "all versions and deletes" mode (`A-IM: Full-Fidelity Feed`).
//! Expected shapes: Microsoft Learn, "Change feed modes" ("Parse the response object").

mod common;

use common::{start, H};
use serde_json::{json, Value};

const AIM: (&str, &str) = ("A-IM", "Full-Fidelity Feed");
const WIRE: (&str, &str) = ("x-ms-cosmos-changefeed-wire-format-version", "2021-09-15");

async fn feed(h: &H, token: &str, extra: &[(&str, &str)]) -> (u16, Vec<Value>, Option<String>) {
    let mut hdrs = vec![AIM, WIRE, ("If-None-Match", token)];
    hdrs.extend_from_slice(extra);
    let r = h.req("GET", "dbs/d/colls/c/docs", &hdrs, "").await;
    let items = if r.status == 200 { r.json()["Documents"].as_array().unwrap().clone() } else { vec![] };
    (r.status, items, r.header("etag").map(str::to_string))
}

async fn setup() -> H {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h
}

fn pkh(p: &str) -> String {
    format!("[\"{p}\"]")
}

#[tokio::test]
async fn every_version_and_the_delete_come_in_order_with_the_documented_shapes() {
    let h = setup().await;
    h.doc("d", "c", &json!("p"), &json!({"id": "before", "pk": "p"})).await;
    let (st, items, tok) = feed(&h, "*", &[]).await;
    assert_eq!(st, 304, "starting from now returns nothing");
    assert!(items.is_empty());
    let tok = tok.expect("etag on 304");

    let h1 = pkh("p");
    let pk = [("x-ms-documentdb-partitionkey", h1.as_str())];
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p", "v": 1})).await.status, 201);
    assert_eq!(h.json("PUT", "dbs/d/colls/c/docs/a", &pk, &json!({"id": "a", "pk": "p", "v": 2})).await.status, 200);
    assert_eq!(h.json("PUT", "dbs/d/colls/c/docs/a", &pk, &json!({"id": "a", "pk": "p", "v": 3})).await.status, 200);
    assert_eq!(h.req("DELETE", "dbs/d/colls/c/docs/a", &pk, "").await.status, 204);

    let (st, items, next) = feed(&h, &tok, &[]).await;
    assert_eq!(st, 200);
    assert_eq!(items.len(), 4, "create, two replaces and the delete: {items:?}");
    let ops: Vec<&str> = items.iter().map(|i| i["metadata"]["operationType"].as_str().unwrap()).collect();
    assert_eq!(ops, ["create", "replace", "replace", "delete"]);
    let vs: Vec<i64> = items[..3].iter().map(|i| i["current"]["v"].as_i64().unwrap()).collect();
    assert_eq!(vs, [1, 2, 3], "every intermediate version, not only the latest");
    // create: no previousImageLSN; others point at the change before
    assert!(items[0]["metadata"].get("previousImageLSN").is_none());
    let lsns: Vec<u64> = items.iter().map(|i| i["metadata"]["lsn"].as_u64().unwrap()).collect();
    assert!(lsns.windows(2).all(|w| w[0] < w[1]));
    for i in 1..4 {
        assert_eq!(items[i]["metadata"]["previousImageLSN"], lsns[i - 1]);
    }
    assert!(items[0]["metadata"]["crts"].as_i64().unwrap() > 1_000_000_000);
    assert_eq!(items[0]["current"]["id"], "a");
    assert!(items[0]["current"]["_ts"].is_number());
    // the delete has no current; id and partition key are in the metadata
    assert!(items[3].get("current").is_none());
    assert_eq!(items[3]["metadata"]["id"], "a");
    assert_eq!(items[3]["metadata"]["partitionKey"], json!({"pk": "p"}));
    assert_eq!(items[3]["metadata"]["timeToLiveExpired"], false);
    assert!(items.iter().all(|i| i.get("previous").is_none()), "Azure gives no previous image");

    // nothing new: 304; the token moves on
    let (st, items, _) = feed(&h, &next.unwrap(), &[]).await;
    assert_eq!((st, items.len()), (304, 0));
    // the document written before the start is not in it
    let (_, items, _) = feed(&h, &tok, &[]).await;
    assert!(items.iter().all(|i| i["current"]["id"] != "before"));
}

#[tokio::test]
async fn latest_version_mode_still_hides_intermediate_versions_and_deletes() {
    let h = setup().await;
    let h1 = pkh("p");
    let pk = [("x-ms-documentdb-partitionkey", h1.as_str())];
    h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p", "v": 1})).await;
    h.json("PUT", "dbs/d/colls/c/docs/a", &pk, &json!({"id": "a", "pk": "p", "v": 2})).await;
    h.doc("d", "c", &json!("p"), &json!({"id": "b", "pk": "p"})).await;
    h.req("DELETE", "dbs/d/colls/c/docs/b", &pk, "").await;
    let r = h.req("GET", "dbs/d/colls/c/docs", &[("A-IM", "Incremental Feed")], "").await;
    let docs = r.json()["Documents"].as_array().unwrap().clone();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["v"], 2);
}

#[tokio::test]
async fn it_cannot_start_from_the_beginning_or_a_time() {
    let h = setup().await;
    let r = h.req("GET", "dbs/d/colls/c/docs", &[AIM, WIRE], "").await;
    assert_eq!(r.status, 400, "{}", r.text());
    let r = h.req("GET", "dbs/d/colls/c/docs", &[AIM, WIRE, ("If-Modified-Since", "Wed, 11 Nov 2026 12:00:00 GMT")], "").await;
    assert_eq!(r.status, 400);
    let (st, _, _) = feed(&h, "not a number", &[]).await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn pages_follow_max_item_count_and_the_token() {
    let h = setup().await;
    let (_, _, tok) = feed(&h, "*", &[]).await;
    for i in 0..7 {
        h.doc("d", "c", &json!("p"), &json!({"id": format!("i{i}"), "pk": "p"})).await;
    }
    let mut token = tok.unwrap();
    let mut seen = Vec::new();
    for _ in 0..10 {
        let (st, items, next) = feed(&h, &token, &[("x-ms-max-item-count", "3")]).await;
        if st == 304 {
            break;
        }
        assert!(items.len() <= 3);
        seen.extend(items.iter().map(|i| i["current"]["id"].as_str().unwrap().to_string()));
        token = next.unwrap();
    }
    assert_eq!(seen, ["i0", "i1", "i2", "i3", "i4", "i5", "i6"]);
}

#[tokio::test]
async fn partition_key_scopes_the_feed() {
    let h = setup().await;
    let (_, _, tok) = feed(&h, "*", &[]).await;
    h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p"})).await;
    h.doc("d", "c", &json!("q"), &json!({"id": "b", "pk": "q"})).await;
    let h1 = pkh("q");
    let (st, items, _) = feed(&h, &tok.unwrap(), &[("x-ms-documentdb-partitionkey", h1.as_str())]).await;
    assert_eq!(st, 200);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["current"]["id"], "b");
}

#[tokio::test]
async fn ttl_expiry_is_a_delete_marked_as_such() {
    let h = setup().await;
    let r = h
        .json(
            "POST",
            "dbs/d/colls",
            &[],
            &json!({"id": "t", "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}, "defaultTtl": -1}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let url = |p: &str| format!("dbs/d/colls/t/{p}");
    let get = |tok: String| {
        let h = &h;
        async move {
            let r = h.req("GET", &url("docs"), &[AIM, WIRE, ("If-None-Match", &tok)], "").await;
            (
                r.status,
                if r.status == 200 { r.json()["Documents"].as_array().unwrap().clone() } else { vec![] },
                r.header("etag").map(str::to_string),
            )
        }
    };
    let (_, _, tok) = get("*".into()).await;
    let r = h.json("POST", &url("docs"), &[("x-ms-documentdb-partitionkey", "[\"p\"]")], &json!({"id": "x", "pk": "p", "ttl": 1})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    let (st, items, _) = get(tok.unwrap()).await;
    assert_eq!(st, 200);
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0]["metadata"]["operationType"], "create");
    assert_eq!(items[1]["metadata"]["operationType"], "delete");
    assert_eq!(items[1]["metadata"]["timeToLiveExpired"], true);
    assert_eq!(items[1]["metadata"]["id"], "x");
}

#[tokio::test]
async fn an_undone_batch_leaves_nothing_in_the_feed() {
    let h = setup().await;
    let (_, _, tok) = feed(&h, "*", &[]).await;
    let ops = json!([
        {"operationType": "Create", "resourceBody": {"id": "ok", "pk": "p"}},
        {"operationType": "Create", "resourceBody": {"id": "ok", "pk": "p"}}
    ]);
    let r = h
        .req(
            "POST",
            "dbs/d/colls/c/docs",
            &[("content-type", "application/json"), ("x-ms-cosmos-is-batch-request", "True"), ("x-ms-documentdb-partitionkey", "[\"p\"]")],
            &ops.to_string(),
        )
        .await;
    assert_eq!(r.status, 207, "{}", r.text());
    let (st, items, _) = feed(&h, &tok.unwrap(), &[]).await;
    assert_eq!((st, items.len()), (304, 0), "{items:?}");
    // and a good batch is in it, in order
    let (_, _, tok) = feed(&h, "*", &[]).await;
    let ops = json!([
        {"operationType": "Create", "resourceBody": {"id": "b1", "pk": "p"}},
        {"operationType": "Create", "resourceBody": {"id": "b2", "pk": "p"}}
    ]);
    let r = h
        .req(
            "POST",
            "dbs/d/colls/c/docs",
            &[("content-type", "application/json"), ("x-ms-cosmos-is-batch-request", "True"), ("x-ms-documentdb-partitionkey", "[\"p\"]")],
            &ops.to_string(),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let (_, items, _) = feed(&h, &tok.unwrap(), &[]).await;
    assert_eq!(items.iter().map(|i| i["current"]["id"].as_str().unwrap()).collect::<Vec<_>>(), ["b1", "b2"]);
}

#[tokio::test]
async fn a_token_from_before_the_history_started_means_now() {
    // e.g. after the emulator was restarted
    let h = setup().await;
    h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p"})).await;
    let (st, items, tok) = feed(&h, "\"1\"", &[]).await;
    assert_eq!((st, items.len()), (304, 0));
    assert!(tok.is_some());
}

#[tokio::test]
async fn nested_partition_key_path_gives_a_nested_object() {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/addr/zip").await;
    let (_, _, tok) = feed(&h, "*", &[]).await;
    let r = h
        .json(
            "POST",
            "dbs/d/colls/c/docs",
            &[("x-ms-documentdb-partitionkey", "[\"98052\"]")],
            &json!({"id": "a", "addr": {"zip": "98052"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = h.req("DELETE", "dbs/d/colls/c/docs/a", &[("x-ms-documentdb-partitionkey", "[\"98052\"]")], "").await;
    assert_eq!(r.status, 204);
    let (_, items, _) = feed(&h, &tok.unwrap(), &[]).await;
    assert_eq!(items[1]["metadata"]["partitionKey"], json!({"addr": {"zip": "98052"}}));
}
