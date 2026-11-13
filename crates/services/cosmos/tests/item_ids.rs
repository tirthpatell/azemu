//! Item ids and partition key values. Azure's service takes every character except `/` and `\` in an item
//! id, up to 1,023 bytes; databases and containers keep 255 characters and refuse `?` and `#`; a partition key value is at
//! most 2,048 bytes ("Service quotas and default limits", per-item limits).

mod common;

use common::{settings, start, H};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

const PK: &str = "x-ms-documentdb-partitionkey";

async fn setup() -> H {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h
}

fn pkh(p: &str) -> String {
    json!([p]).to_string()
}

/// The path form of an id: every byte outside the unreserved characters percent-encoded, as the SDKs that do not check
/// the id on the client write it.
fn enc(id: &str) -> String {
    let mut out = String::new();
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

const AWKWARD: [&str; 7] = ["USER#123", "a?b=c", "#", "?", "100% sure", "tab\there", "naïve ☃ id"];

#[tokio::test]
async fn an_item_id_may_hold_hash_question_mark_percent_and_other_characters() {
    let h = setup().await;
    for id in AWKWARD {
        let r = h.doc("d", "c", &json!("p"), &json!({"id": id, "pk": "p", "v": 1})).await;
        assert_eq!(r.status, 201, "{id:?}: {}", r.text());
        assert_eq!(r.json()["id"], id);
        let rid = r.json()["_rid"].as_str().unwrap().to_string();
        let selflink = r.json()["_self"].as_str().unwrap().to_string();
        assert!(selflink.contains(&rid) && !selflink.contains('#') && !selflink.contains('?'), "_self is built from the rid: {selflink}");

        let path = format!("dbs/d/colls/c/docs/{}", enc(id));
        // read
        let r = h.req("GET", &path, &[(PK, &pkh("p"))], "").await;
        assert_eq!(r.status, 200, "{id:?}: {}", r.text());
        assert_eq!(r.json()["id"], id);
        // by _rid, and by the _self link's last segment
        assert_eq!(h.req("GET", &format!("dbs/d/colls/c/docs/{rid}"), &[(PK, &pkh("p"))], "").await.status, 200);
        // replace
        let r = h.json("PUT", &path, &[(PK, &pkh("p"))], &json!({"id": id, "pk": "p", "v": 2})).await;
        assert_eq!(r.status, 200, "{id:?}: {}", r.text());
        // patch
        let r = h.json("PATCH", &path, &[(PK, &pkh("p"))], &json!({"operations": [{"op": "incr", "path": "/v", "value": 5}]})).await;
        assert_eq!(r.status, 200, "{id:?}: {}", r.text());
        assert_eq!(r.json()["v"], 7);
        // the same id again is a conflict, an upsert replaces
        assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": id, "pk": "p"})).await.status, 409);
        let up = h
            .json(
                "POST",
                "dbs/d/colls/c/docs",
                &[(PK, &pkh("p")), ("x-ms-documentdb-is-upsert", "True")],
                &json!({"id": id, "pk": "p", "v": 9}),
            )
            .await;
        assert_eq!(up.status, 200, "{id:?}: {}", up.text());
    }
    // a query finds them by id, with the id as a parameter
    let q = h
        .query(
            "d",
            "c",
            &[(PK, &pkh("p"))],
            json!({"query": "SELECT c.id FROM c WHERE c.id = @i", "parameters": [{"name": "@i", "value": "USER#123"}]}),
        )
        .await;
    assert_eq!(q.json()["Documents"], json!([{"id": "USER#123"}]));
    // delete
    for id in AWKWARD {
        let path = format!("dbs/d/colls/c/docs/{}", enc(id));
        assert_eq!(h.req("DELETE", &path, &[(PK, &pkh("p"))], "").await.status, 204, "{id:?}");
        assert_eq!(h.req("GET", &path, &[(PK, &pkh("p"))], "").await.status, 404, "{id:?}");
    }
}

#[tokio::test]
async fn the_change_feeds_show_an_id_with_hash_and_question_mark() {
    let h = setup().await;
    let all = [("A-IM", "Full-Fidelity Feed"), ("x-ms-cosmos-changefeed-wire-format-version", "2021-09-15")];
    // all versions and deletes: start from now, then write and delete
    let mut hdrs = all.to_vec();
    hdrs.push(("If-None-Match", "*"));
    let r = h.req("GET", "dbs/d/colls/c/docs", &hdrs, "").await;
    assert_eq!(r.status, 304);
    let token = r.header("etag").unwrap().to_string();
    h.doc("d", "c", &json!("p"), &json!({"id": "USER#1?x", "pk": "p"})).await;
    // the incremental feed, from the beginning
    let r = h.req("GET", "dbs/d/colls/c/docs", &[("A-IM", "Incremental Feed")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["Documents"][0]["id"], "USER#1?x");
    assert_eq!(h.req("DELETE", &format!("dbs/d/colls/c/docs/{}", enc("USER#1?x")), &[(PK, &pkh("p"))], "").await.status, 204);
    let mut hdrs = all.to_vec();
    hdrs.push(("If-None-Match", &token));
    let r = h.req("GET", "dbs/d/colls/c/docs", &hdrs, "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let items = r.json()["Documents"].as_array().unwrap().clone();
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0]["current"]["id"], "USER#1?x");
    assert_eq!(items[1]["metadata"]["id"], "USER#1?x");
    assert_eq!(items[1]["metadata"]["operationType"], "delete");
}

#[tokio::test]
async fn batch_and_bulk_take_ids_with_hash_and_question_mark() {
    let h = setup().await;
    // transactional batch
    let hdrs = [(PK, "[\"p\"]"), ("x-ms-cosmos-is-batch-request", "True"), ("x-ms-cosmos-batch-atomic", "True")];
    let ops = json!([
        {"operationType": "Create", "resourceBody": {"id": "B#1", "pk": "p"}},
        {"operationType": "Create", "resourceBody": {"id": "B?2", "pk": "p"}},
        {"operationType": "Read", "id": "B#1"},
        {"operationType": "Patch", "id": "B?2", "resourceBody": {"operations": [{"op": "add", "path": "/x", "value": 1}]}},
        {"operationType": "Replace", "id": "B#1", "resourceBody": {"id": "B#1", "pk": "p", "n": 2}},
        {"operationType": "Delete", "id": "B?2"},
    ]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdrs, &ops).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let codes: Vec<i64> = r.json().as_array().unwrap().iter().map(|x| x["statusCode"].as_i64().unwrap()).collect();
    assert_eq!(codes, [201, 201, 200, 200, 200, 204]);
    assert_eq!(r.json()[2]["resourceBody"]["id"], "B#1");
    // bulk (non atomic, each operation names its partition)
    let hdrs =
        [("x-ms-cosmos-is-batch-request", "True"), ("x-ms-cosmos-batch-atomic", "False"), ("x-ms-cosmos-batch-continue-on-error", "True")];
    let ops = json!([
        {"operationType": "Create", "partitionKey": "[\"q\"]", "resourceBody": {"id": "U#9", "pk": "q"}},
        {"operationType": "Read", "id": "U#9", "partitionKey": "[\"q\"]"},
        {"operationType": "Delete", "id": "U#9", "partitionKey": "[\"q\"]"},
    ]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdrs, &ops).await;
    let codes: Vec<i64> = r.json().as_array().unwrap().iter().map(|x| x["statusCode"].as_i64().unwrap()).collect();
    assert_eq!(codes, [201, 200, 204], "{}", r.text());
}

#[tokio::test]
async fn a_stored_procedure_reads_and_writes_an_id_with_hash_and_question_mark() {
    let h = setup().await;
    let body = r#"function (id) {
        var c = getContext().getCollection();
        var made = c.createDocument(c.getSelfLink(), {id: id, pk: 'p'}, function (e, d) { if (e) throw e; });
        var link = c.getAltLink() + '/docs/' + encodeURIComponent(id);
        var seen = c.readDocument(link, {}, function (e, d) { if (e) throw e; getContext().getResponse().setBody(d.id); });
        if (!made || !seen) throw new Error('not accepted');
    }"#;
    let r = h.json("POST", "dbs/d/colls/c/sprocs", &[], &json!({"id": "s", "body": body})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = h.json("POST", "dbs/d/colls/c/sprocs/s", &[(PK, &pkh("p"))], &json!(["S#1?z"])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!("S#1?z"));
    assert_eq!(h.req("GET", &format!("dbs/d/colls/c/docs/{}", enc("S#1?z")), &[(PK, &pkh("p"))], "").await.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ids_with_hash_and_question_mark_survive_a_restart() {
    let data: PathBuf = std::env::temp_dir().join(format!("azemu-cosmos-itemids-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let start_on = |data: PathBuf| async move {
        let svc = Arc::new(azemu_cosmos::CosmosService::with_script_settings(settings()));
        let srv = azemu_core::testing::TestServer::start_persistent(vec![svc.clone()], &data).await;
        let base = srv.url("documents", "acct1");
        H { srv, base, svc }
    };
    let h = start_on(data.clone()).await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    for id in AWKWARD {
        assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": id, "pk": "p"})).await.status, 201);
    }
    h.srv.shutdown().await.unwrap();
    let h2 = start_on(data.clone()).await;
    for id in AWKWARD {
        let r = h2.req("GET", &format!("dbs/d/colls/c/docs/{}", enc(id)), &[(PK, &pkh("p"))], "").await;
        assert_eq!(r.status, 200, "{id:?}: {}", r.text());
        assert_eq!(r.json()["id"], id);
    }
    h2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test]
async fn the_item_id_limits_are_1023_bytes_and_no_slash_or_backslash() {
    let h = setup().await;
    let post = |id: String| {
        let h = &h;
        async move { h.doc("d", "c", &json!("p"), &json!({"id": id, "pk": "p"})).await }
    };
    // 1,023 bytes pass (more than the 255 characters of a database or container name), 1,024 do not
    assert_eq!(post("a".repeat(1023)).await.status, 201);
    assert_eq!(post("a".repeat(256)).await.status, 201);
    let r = post("a".repeat(1024)).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("1023 bytes"), "{}", r.text());
    // the limit counts bytes: 341 characters of three bytes are 1,023, 342 are 1,026
    assert_eq!(post("€".repeat(341)).await.status, 201);
    assert_eq!(post("€".repeat(342)).await.status, 400);
    // `/` and `\` are refused, and so is an id that ends in a space or is empty
    for bad in ["a/b", "a\\b", "x ", ""] {
        let r = post(bad.to_string()).await;
        assert_eq!(r.status, 400, "{bad:?}: {}", r.text());
    }
    // the same limit applies to a replace and to a batch
    let r = h.json("PUT", "dbs/d/colls/c/docs/a", &[(PK, &pkh("p"))], &json!({"id": "a".repeat(1024), "pk": "p"})).await;
    assert_eq!(r.status, 400);
    let hdrs = [(PK, "[\"p\"]"), ("x-ms-cosmos-is-batch-request", "True"), ("x-ms-cosmos-batch-atomic", "True")];
    let r = h
        .json(
            "POST",
            "dbs/d/colls/c/docs",
            &hdrs,
            &json!([{"operationType": "Create", "resourceBody": {"id": "a".repeat(1024), "pk": "p"}}]),
        )
        .await;
    assert_ne!(r.status, 200, "{}", r.text());
}

#[tokio::test]
async fn database_and_container_names_keep_255_characters_and_refuse_hash_and_question_mark() {
    let h = start().await;
    assert_eq!(h.json("POST", "dbs", &[], &json!({"id": "d".repeat(255)})).await.status, 201);
    assert_eq!(h.json("POST", "dbs", &[], &json!({"id": "d".repeat(256)})).await.status, 400);
    for bad in ["a#b", "a?b", "a/b", "a\\b"] {
        assert_eq!(h.json("POST", "dbs", &[], &json!({"id": bad})).await.status, 400, "{bad}");
    }
    h.db("d").await;
    let pk = json!({"paths": ["/pk"], "kind": "Hash", "version": 2});
    assert_eq!(h.json("POST", "dbs/d/colls", &[], &json!({"id": "c".repeat(255), "partitionKey": pk})).await.status, 201);
    assert_eq!(h.json("POST", "dbs/d/colls", &[], &json!({"id": "c".repeat(256), "partitionKey": pk})).await.status, 400);
    for bad in ["a#b", "a?b"] {
        assert_eq!(h.json("POST", "dbs/d/colls", &[], &json!({"id": bad, "partitionKey": pk})).await.status, 400, "{bad}");
    }
}

#[tokio::test]
async fn a_partition_key_value_is_at_most_2048_bytes() {
    let h = setup().await;
    let ok = "k".repeat(2048);
    let long = "k".repeat(2049);
    // in the document
    assert_eq!(h.doc("d", "c", &json!(ok), &json!({"id": "1", "pk": ok})).await.status, 201);
    let r = h.doc("d", "c", &json!("p"), &json!({"id": "2", "pk": long})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("2048 bytes"), "{}", r.text());
    // in the header: a read of the stored one works, a read with the longer value is refused
    let r = h.req("GET", "dbs/d/colls/c/docs/1", &[(PK, &pkh(&ok))], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/1", &[(PK, &pkh(&long))], "").await.status, 400);
    // the limit is in bytes: 682 characters of three bytes are 2,046, 683 are 2,049
    let euro_ok = "€".repeat(682);
    let euro_long = "€".repeat(683);
    assert_eq!(h.doc("d", "c", &json!(euro_ok), &json!({"id": "3", "pk": euro_ok})).await.status, 201);
    assert_eq!(h.doc("d", "c", &json!(euro_long), &json!({"id": "4", "pk": euro_long})).await.status, 400);
    // a batch operation with a long key of its own
    let hdrs =
        [("x-ms-cosmos-is-batch-request", "True"), ("x-ms-cosmos-batch-atomic", "False"), ("x-ms-cosmos-batch-continue-on-error", "True")];
    let ops = json!([{"operationType": "Create", "partitionKey": json!([long]).to_string(), "resourceBody": {"id": "5", "pk": long}}]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdrs, &ops).await;
    let codes: Vec<Value> = r.json().as_array().cloned().unwrap_or_default().iter().map(|x| x["statusCode"].clone()).collect();
    assert!(r.status == 400 || codes == [json!(400)], "{} {}", r.status, r.text());
}

#[tokio::test]
async fn hierarchical_keys_are_limited_per_component() {
    let h = start().await;
    h.db("d").await;
    let r = h
        .json("POST", "dbs/d/colls", &[], &json!({"id": "h", "partitionKey": {"paths": ["/a", "/b"], "kind": "MultiHash", "version": 2}}))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let k1 = "x".repeat(2048);
    let hdr = json!([k1, "y"]).to_string();
    let r = h.json("POST", "dbs/d/colls/h/docs", &[(PK, &hdr)], &json!({"id": "1", "a": k1, "b": "y"})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let long = "x".repeat(2049);
    let r = h.json("POST", "dbs/d/colls/h/docs", &[], &json!({"id": "2", "a": "x", "b": long})).await;
    assert_eq!(r.status, 400, "{}", r.text());
}
