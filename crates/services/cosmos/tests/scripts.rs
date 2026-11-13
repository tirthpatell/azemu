//! Stored procedures, triggers and user-defined functions, run with the documented samples (`tests/fixtures/scripts/`,
//! each file names the page it comes from; the first line of a file is a comment, the rest is the sample as published).

mod common;

use std::sync::Arc;

use azemu_core::testing::{TestResponse, TestServer};
use azemu_cosmos::{CosmosService, ScriptSettings};
use common::H;
use serde_json::{json, Value};

macro_rules! fx {
    ($name:literal) => {
        include_str!(concat!("fixtures/scripts/", $name))
    };
}

pub async fn start_with(settings: ScriptSettings) -> H {
    let svc = Arc::new(CosmosService::with_script_settings(settings));
    let srv = TestServer::start(vec![svc.clone()]).await;
    let base = srv.url("documents", "acct1");
    H { srv, base, svc }
}

pub async fn start() -> H {
    start_with(common::settings()).await
}

fn pk(v: &str) -> String {
    json!([v]).to_string()
}

impl H {
    async fn sproc(&self, db: &str, coll: &str, id: &str, body: &str) {
        let r = self.json("POST", &format!("dbs/{db}/colls/{coll}/sprocs"), &[], &json!({ "id": id, "body": body })).await;
        assert_eq!(r.status, 201, "{}", r.text());
    }

    async fn trigger(&self, db: &str, coll: &str, id: &str, body: &str, kind: &str, op: &str) {
        let r = self
            .json(
                "POST",
                &format!("dbs/{db}/colls/{coll}/triggers"),
                &[],
                &json!({ "id": id, "body": body, "triggerType": kind, "triggerOperation": op }),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.text());
    }

    async fn udf(&self, db: &str, coll: &str, id: &str, body: &str) {
        let r = self.json("POST", &format!("dbs/{db}/colls/{coll}/udfs"), &[], &json!({ "id": id, "body": body })).await;
        assert_eq!(r.status, 201, "{}", r.text());
    }

    async fn exec(&self, db: &str, coll: &str, id: &str, partition: &str, params: Value) -> TestResponse {
        self.exec_with(db, coll, id, partition, params, &[]).await
    }

    async fn exec_with(&self, db: &str, coll: &str, id: &str, partition: &str, params: Value, extra: &[(&str, &str)]) -> TestResponse {
        let p = pk(partition);
        let mut h = vec![("x-ms-documentdb-partitionkey", p.as_str())];
        h.extend_from_slice(extra);
        self.json("POST", &format!("dbs/{db}/colls/{coll}/sprocs/{id}"), &h, &params).await
    }

    async fn docs_in(&self, db: &str, coll: &str, partition: &str) -> Vec<Value> {
        let p = pk(partition);
        let r = self.query(db, coll, &[("x-ms-documentdb-partitionkey", &p)], json!({"query": "SELECT * FROM c ORDER BY c.id"})).await;
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()["Documents"].as_array().cloned().unwrap_or_default()
    }
}

async fn setup() -> H {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h
}

#[tokio::test]
async fn hello_world_sample() {
    let h = setup().await;
    h.sproc("d", "c", "hello", fx!("hello_world.js")).await;
    let r = h.exec("d", "c", "hello", "p", json!([])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!("Hello, World"));
    assert!(r.header("x-ms-request-charge").is_some());
    assert!(r.header("x-ms-session-token").is_some());
}

#[tokio::test]
async fn create_items_sample_writes_and_counts() {
    let h = setup().await;
    h.sproc("d", "c", "create", fx!("create_todo_items.js")).await;
    let items = json!([{"pk": "p", "name": "Groceries"}, {"pk": "p", "name": "Doctor", "id": "fixed"}]);
    let r = h.exec("d", "c", "create", "p", json!([items])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!(2));
    let docs = h.docs_in("d", "c", "p").await;
    assert_eq!(docs.len(), 2);
    assert!(docs.iter().any(|d| d["id"] == "fixed"));
    assert!(docs.iter().any(|d| d["name"] == "Groceries" && d["id"].as_str().map(|s| s.len() > 10).unwrap_or(false)), "an id is generated");
}

#[tokio::test]
async fn an_exception_rolls_everything_back() {
    let h = setup().await;
    h.sproc("d", "c", "create", fx!("create_todo_items.js")).await;
    let items = json!([{"pk": "p", "id": "a"}, {"pk": "p", "id": "b"}, {"pk": "p", "id": "a"}]);
    let r = h.exec("d", "c", "create", "p", json!([items])).await;
    assert_eq!(r.status, 400, "{}", r.text());
    let msg = r.json()["message"].as_str().unwrap_or("").to_string();
    assert!(msg.contains("Encountered exception while executing function. Exception = Error:"), "{msg}");
    assert!(msg.contains("Entity with the specified id already exists"), "{msg}");
    assert!(h.docs_in("d", "c", "p").await.is_empty(), "nothing is kept");

    h.sproc("d", "c", "boom", "function () { __.createDocument(__.getSelfLink(), {id: 'x', pk: 'p'}); throw new Error('boom'); }").await;
    let r = h.exec("d", "c", "boom", "p", json!([])).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["message"].as_str().unwrap_or("").contains("Exception = Error: boom"), "{}", r.text());
    assert!(h.docs_in("d", "c", "p").await.is_empty());
}

#[tokio::test]
async fn bulk_import_stops_when_a_call_is_not_accepted_and_resumes() {
    let h = start_with(ScriptSettings { max_operations: 4, ..common::settings() }).await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h.sproc("d", "c", "import", fx!("bulk_import.js")).await;
    let items: Vec<Value> = (0..10).map(|i| json!({"id": format!("i{i}"), "pk": "p", "n": i})).collect();
    let mut done = 0usize;
    let mut calls = 0;
    while done < items.len() {
        let rest = Value::Array(items[done..].to_vec());
        let r = h.exec("d", "c", "import", "p", json!([rest])).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let n = r.json().as_u64().unwrap() as usize;
        assert!(n > 0 && n <= 4, "each call imports at most the operations it was allowed: {n}");
        done += n;
        calls += 1;
    }
    assert_eq!(calls, 3, "10 documents, 4 operations a call");
    assert_eq!(h.docs_in("d", "c", "p").await.len(), 10);
}

#[tokio::test]
async fn bulk_import_repo_sample_and_bulk_delete_sample() {
    let h = start_with(ScriptSettings { max_operations: 5, ..common::settings() }).await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h.sproc("d", "c", "import", fx!("bulk_import_repo.js")).await;
    h.sproc("d", "c", "delete", fx!("bulk_delete.js")).await;
    let docs: Vec<Value> = (0..12).map(|i| json!({"id": format!("i{i}"), "pk": "p", "keep": i % 2 == 0})).collect();
    let r = h.exec("d", "c", "import", "p", json!([docs])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.json().as_u64().unwrap() > 0);
    let mut have = h.docs_in("d", "c", "p").await.len();
    while have < 12 {
        let rest: Vec<Value> = docs[have..].to_vec();
        let r = h.exec("d", "c", "import", "p", json!([rest])).await;
        assert_eq!(r.status, 200, "{}", r.text());
        have = h.docs_in("d", "c", "p").await.len();
    }
    assert_eq!(h.doc("d", "c", &json!("q"), &json!({"id": "other", "pk": "q"})).await.status, 201);
    let mut deleted = 0u64;
    let mut rounds = 0;
    loop {
        let r = h.exec("d", "c", "delete", "p", json!(["SELECT c._self FROM c WHERE c.keep = false"])).await;
        assert_eq!(r.status, 200, "{}", r.text());
        deleted += r.json()["deleted"].as_u64().unwrap();
        rounds += 1;
        if r.json()["continuation"] == json!(false) {
            break;
        }
        assert!(rounds < 20);
    }
    assert_eq!(deleted, 6);
    assert_eq!(h.docs_in("d", "c", "p").await.len(), 6);
    assert_eq!(h.docs_in("d", "c", "q").await.len(), 1);
}

#[tokio::test]
async fn trade_players_sample_swaps_two_documents_in_one_transaction() {
    let h = setup().await;
    h.sproc("d", "c", "trade", fx!("trade_players.js")).await;
    assert_eq!(h.doc("d", "c", &json!("t"), &json!({"id": "p1", "pk": "t", "team": "red"})).await.status, 201);
    assert_eq!(h.doc("d", "c", &json!("t"), &json!({"id": "p2", "pk": "t", "team": "blue"})).await.status, 201);
    // the sample queries `Players p`: any alias works
    let r = h.exec("d", "c", "trade", "t", json!(["p1", "p2"])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let docs = h.docs_in("d", "c", "t").await;
    assert_eq!(docs[0]["team"], "blue");
    assert_eq!(docs[1]["team"], "red");
    // a missing player is a thrown string: the message carries it, nothing changes
    let r = h.exec("d", "c", "trade", "t", json!(["p1", "nobody"])).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["message"].as_str().unwrap().contains("Unable to find player 2"), "{}", r.text());
    assert_eq!(h.docs_in("d", "c", "t").await[0]["team"], "blue");
}

#[tokio::test]
async fn async_await_sample_with_promises_and_abort() {
    let h = setup().await;
    h.sproc("d", "c", "async", fx!("async_sample.js")).await;
    for i in 0..5 {
        assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": format!("d{i}"), "pk": "p"})).await.status, 201);
    }
    let r = h.exec("d", "c", "async", "p", json!([])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let docs = h.docs_in("d", "c", "p").await;
    assert_eq!(docs.len(), 5);
    assert!(docs.iter().all(|d| d["newProp"] == 1), "{docs:?}");
    // abort(err) in a promise callback rolls everything back
    h.sproc("d", "c", "abort", "function () { __.createDocument(__.getSelfLink(), {id: 'z', pk: 'p'}, function () { Promise.resolve().then(function () { getContext().abort(new Error('stop here')); }); }); }").await;
    let r = h.exec("d", "c", "abort", "p", json!([])).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.json()["message"].as_str().unwrap().contains("stop here"), "{}", r.text());
    assert_eq!(h.docs_in("d", "c", "p").await.len(), 5);
}

#[tokio::test]
async fn count_and_sum_samples_page_with_continuation_tokens() {
    let h = setup().await;
    h.sproc("d", "c", "count", fx!("count.js")).await;
    h.sproc("d", "c", "sum", fx!("sum.js")).await;
    for i in 0..60 {
        assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": format!("d{i:02}"), "pk": "p", "n": 2})).await.status, 201);
    }
    let mut total = 0u64;
    let mut token = Value::Null;
    for _ in 0..10 {
        let r = h.exec("d", "c", "count", "p", json!(["", token])).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let b = r.json();
        total += b["count"].as_u64().unwrap();
        if b["continuationToken"].is_null() || b["continuationToken"] == json!("") {
            break;
        }
        token = b["continuationToken"].clone();
    }
    assert_eq!(total, 60);
    // the sum sample reads one page of 25
    let r = h.exec("d", "c", "sum", "p", json!(["n", "", null])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // (the sample adds up `documentsProcessed` values of a page of 25 documents, each with n = 2)
    assert_eq!(r.json()["documentsProcessed"], 25, "{}", r.text());
    assert!(r.json()["sum"].as_f64().is_some(), "{}", r.text());
}

#[tokio::test]
async fn update_sample_changes_one_document() {
    let h = setup().await;
    h.sproc("d", "c", "update", fx!("update.js")).await;
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "foo", "pk": "p", "counter": 1})).await.status, 201);
    let r = h.exec("d", "c", "update", "p", json!(["foo", {"$inc": {"counter": 2}, "$set": {"message": "hi"}}])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let docs = h.docs_in("d", "c", "p").await;
    assert_eq!(docs[0]["counter"], 3);
    assert_eq!(docs[0]["message"], "hi");
    let r = h.exec("d", "c", "update", "p", json!(["missing", {"$inc": {"counter": 2}}])).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["message"].as_str().unwrap().contains("Document not found."), "{}", r.text());
}

#[tokio::test]
async fn array_parameter_sample_and_script_logging() {
    let h = setup().await;
    h.sproc("d", "c", "log", fx!("array_parameter.js")).await;
    let r = h.exec_with("d", "c", "log", "p", json!([["a", "b", {"x": 1}]]), &[("x-ms-documentdb-script-enable-logging", "true")]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let log = r.header("x-ms-documentdb-script-log-results").unwrap_or_default();
    assert_eq!(log, "a%0Ab%0A%7B%22x%22%3A1%7D", "console output is percent-encoded, one line per call");
    // not asked for: no header
    let r = h.exec("d", "c", "log", "p", json!([["a"]])).await;
    assert!(r.header("x-ms-documentdb-script-log-results").is_none());
}

#[tokio::test]
async fn the_server_side_api_reads_writes_and_scopes_to_the_partition() {
    let h = setup().await;
    assert_eq!(h.doc("d", "c", &json!("q"), &json!({"id": "other", "pk": "q"})).await.status, 201);
    h.sproc(
        "d",
        "c",
        "api",
        r#"function () {
        var coll = getContext().getCollection();
        var link = coll.getSelfLink();
        var out = {};
        out.alt = coll.getAltLink();
        out.self = link;
        out.create = coll.createDocument(link, {id: 'a', pk: 'p', n: 1}, function (err, doc, opts) {
            out.created = doc.id + ':' + doc._rid.length + ':' + (typeof doc._etag);
            coll.readDocument(doc._self, function (e, d) {
                out.read = d.n;
                coll.upsertDocument(link, {id: 'a', pk: 'p', n: 2}, function (e2, d2) {
                    out.upsert = d2.n;
                    coll.replaceDocument(coll.getAltLink() + '/docs/a', {id: 'a', pk: 'p', n: 3}, {etag: d2._etag}, function (e3, d3) {
                        out.replace = d3.n;
                        coll.replaceDocument(coll.getAltLink() + '/docs/a', {id: 'a', pk: 'p', n: 4}, {etag: d2._etag}, function (e4) {
                            out.stale = e4 && e4.number;
                            coll.readDocument(coll.getAltLink() + '/docs/other', function (e5) {
                                out.other = e5 && e5.number;
                                coll.queryDocuments(link, 'SELECT VALUE COUNT(1) FROM c', function (e6, rows) {
                                    out.count = rows[0];
                                    coll.deleteDocument(coll.getAltLink() + '/docs/a', function (e7) {
                                        out.deleted = !e7;
                                        coll.readDocuments(link, function (e8, all) {
                                            out.left = all.length;
                                            getContext().getResponse().setBody(out);
                                        });
                                    });
                                });
                            });
                        });
                    });
                });
            });
        });
    }"#,
    )
    .await;
    let r = h.exec("d", "c", "api", "p", json!([])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let b = r.json();
    assert_eq!(b["alt"], "dbs/d/colls/c");
    assert!(b["self"].as_str().unwrap().starts_with("dbs/"));
    assert_eq!(b["create"], true);
    assert_eq!(b["created"], "a:24:string");
    assert_eq!(b["read"], 1);
    assert_eq!(b["upsert"], 2);
    assert_eq!(b["replace"], 3);
    assert_eq!(b["stale"], 412);
    assert_eq!(b["other"], 404, "a document of another partition is not visible");
    assert_eq!(b["count"], 1);
    assert_eq!(b["deleted"], true);
    assert_eq!(b["left"], 0);
}

fn with_pk<'a>(p: &'a str, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut v = vec![("x-ms-documentdb-partitionkey", p)];
    v.extend_from_slice(extra);
    v
}

#[tokio::test]
async fn pre_trigger_sample_changes_the_document_that_is_created() {
    let h = setup().await;
    h.trigger("d", "c", "ts", fx!("pre_trigger_timestamp.js"), "Pre", "Create").await;
    let p = pk("p");
    let hdr = with_pk(&p, &[("x-ms-documentdb-pre-trigger-include", "ts")]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdr, &json!({"id": "a", "pk": "p"})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(r.json()["timestamp"].as_i64().unwrap() > 1_600_000_000_000i64, "{}", r.text());
    assert!(r.header("etag").is_some() && r.header("lsn").is_some());
    let docs = h.docs_in("d", "c", "p").await;
    assert_eq!(docs[0]["timestamp"], r.json()["timestamp"]);
    // a document that has the property keeps it
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdr, &json!({"id": "b", "pk": "p", "timestamp": 5})).await;
    assert_eq!(r.json()["timestamp"], 5);
    // without the header the trigger does not run
    let r = h.json("POST", "dbs/d/colls/c/docs", &with_pk(&p, &[]), &json!({"id": "c", "pk": "p"})).await;
    assert!(r.json().get("timestamp").is_none());
    // a Create trigger cannot run on a replace; a Post trigger is not a pre-trigger; an unknown name is a 400
    let r = h.json("PUT", "dbs/d/colls/c/docs/a", &hdr, &json!({"id": "a", "pk": "p"})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("cannot run on a replace"), "{}", r.text());
    h.trigger("d", "c", "post1", "function () {}", "Post", "All").await;
    let r = h
        .json(
            "POST",
            "dbs/d/colls/c/docs",
            &with_pk(&p, &[("x-ms-documentdb-pre-trigger-include", "post1")]),
            &json!({"id": "x", "pk": "p"}),
        )
        .await;
    assert_eq!(r.status, 400);
    let r = h
        .json(
            "POST",
            "dbs/d/colls/c/docs",
            &with_pk(&p, &[("x-ms-documentdb-pre-trigger-include", "nope")]),
            &json!({"id": "x", "pk": "p"}),
        )
        .await;
    assert_eq!(r.status, 400);
    assert!(h.docs_in("d", "c", "p").await.iter().all(|d| d["id"] != "x"));
}

#[tokio::test]
async fn post_trigger_sample_updates_the_metadata_document_in_the_same_transaction() {
    let h = setup().await;
    h.trigger("d", "c", "meta", fx!("post_trigger_metadata.js"), "post", "create").await;
    assert_eq!(
        h.doc("d", "c", &json!("p"), &json!({"id": "_metadata", "pk": "p", "createdItems": 0, "createdNames": ""})).await.status,
        201
    );
    let p = pk("p");
    let hdr = with_pk(&p, &[("x-ms-documentdb-post-trigger-include", "meta")]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdr, &json!({"id": "artist_profile_1023", "pk": "p"})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdr, &json!({"id": "two", "pk": "p"})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let docs = h.docs_in("d", "c", "p").await;
    let meta = docs.iter().find(|d| d["id"] == "_metadata").unwrap();
    assert_eq!(meta["createdItems"], 2);
    assert_eq!(meta["createdNames"], " artist_profile_1023 two");
    // a post-trigger that throws undoes the document too
    h.trigger("d", "c", "bad", "function () { throw new Error('no'); }", "Post", "All").await;
    let r = h
        .json(
            "POST",
            "dbs/d/colls/c/docs",
            &with_pk(&p, &[("x-ms-documentdb-post-trigger-include", "bad")]),
            &json!({"id": "never", "pk": "p"}),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("Exception = Error: no"), "{}", r.text());
    assert!(h.docs_in("d", "c", "p").await.iter().all(|d| d["id"] != "never"));
    // the metadata trigger on a missing metadata document aborts the create (the sample throws)
    let r = h
        .json(
            "POST",
            "dbs/d/colls/q/docs",
            &with_pk(&pk("zz"), &[("x-ms-documentdb-post-trigger-include", "meta")]),
            &json!({"id": "n", "pk": "zz"}),
        )
        .await;
    assert_eq!(r.status, 404);
    h.coll("d", "c2", "/pk").await;
    h.trigger("d", "c2", "meta", fx!("post_trigger_metadata.js"), "Post", "Create").await;
    let r = h
        .json(
            "POST",
            "dbs/d/colls/c2/docs",
            &with_pk(&pk("zz"), &[("x-ms-documentdb-post-trigger-include", "meta")]),
            &json!({"id": "n", "pk": "zz"}),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("Unable to find metadata document"), "{}", r.text());
    assert!(h.docs_in("d", "c2", "zz").await.is_empty());
}

#[tokio::test]
async fn triggers_run_on_replace_upsert_and_delete_and_can_change_the_response() {
    let h = setup().await;
    h.trigger(
        "d",
        "c",
        "stamp",
        "function () { var r = getContext().getRequest(); var b = r.getBody(); b.stamped = r.getOperationType(); r.setBody(b); }",
        "Pre",
        "All",
    )
    .await;
    h.trigger(
        "d",
        "c",
        "echo",
        "function () { var r = getContext().getResponse(); var b = r.getBody(); b.seen = true; r.setBody(b); }",
        "Post",
        "All",
    )
    .await;
    h.trigger(
        "d",
        "c",
        "gone",
        "function () { __.createDocument(__.getSelfLink(), {id: 'tomb-' + getContext().getRequest().getOperationType(), pk: 'p'}); }",
        "Post",
        "Delete",
    )
    .await;
    let p = pk("p");
    let both = with_pk(&p, &[("x-ms-documentdb-pre-trigger-include", "stamp"), ("x-ms-documentdb-post-trigger-include", "echo")]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &both, &json!({"id": "a", "pk": "p"})).await;
    assert_eq!((r.status, r.json()["stamped"].clone(), r.json()["seen"].clone()), (201, json!("Create"), json!(true)), "{}", r.text());
    let r = h.json("PUT", "dbs/d/colls/c/docs/a", &both, &json!({"id": "a", "pk": "p", "v": 2})).await;
    assert_eq!((r.status, r.json()["stamped"].clone()), (200, json!("Replace")), "{}", r.text());
    let mut up = both.clone();
    up.push(("x-ms-documentdb-is-upsert", "true"));
    let r = h.json("POST", "dbs/d/colls/c/docs", &up, &json!({"id": "a", "pk": "p", "v": 3})).await;
    assert_eq!((r.status, r.json()["stamped"].clone(), r.json()["v"].clone()), (200, json!("Upsert"), json!(3)), "{}", r.text());
    // the stored document is the one the trigger changed, not the one that was sent
    assert_eq!(h.docs_in("d", "c", "p").await[0]["stamped"], "Upsert");
    let r = h.req("DELETE", "dbs/d/colls/c/docs/a", &with_pk(&p, &[("x-ms-documentdb-post-trigger-include", "gone")]), "").await;
    assert_eq!(r.status, 204, "{}", r.text());
    let docs = h.docs_in("d", "c", "p").await;
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["id"], "tomb-Delete");
    // a failing operation (conflict) leaves the script's writes out too
    let r = h
        .json(
            "POST",
            "dbs/d/colls/c/docs",
            &with_pk(&p, &[("x-ms-documentdb-pre-trigger-include", "stamp")]),
            &json!({"id": "tomb-Delete", "pk": "p"}),
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.text());
}

#[tokio::test]
async fn user_defined_function_sample_in_select_where_and_order_by() {
    let h = setup().await;
    h.udf("d", "c", "Tax", fx!("udf_tax.js")).await;
    for (i, income) in [500, 5000, 70000, 100000].iter().enumerate() {
        assert_eq!(
            h.doc("d", "c", &json!("p"), &json!({"id": format!("i{i}"), "pk": "p", "name": format!("n{i}"), "income": income}))
                .await
                .status,
            201
        );
    }
    let p = pk("p");
    let hdr = [("x-ms-documentdb-partitionkey", p.as_str())];
    let r = h.query("d", "c", &hdr, json!({"query": "SELECT * FROM Incomes t WHERE udf.Tax(t.income) > 20000"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let ids: Vec<String> = r.json()["Documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids, ["i2", "i3"]);
    let r = h.query("d", "c", &hdr, json!({"query": "SELECT t.id, udf.Tax(t.income) AS tax FROM t ORDER BY t.id"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let taxes: Vec<f64> = r.json()["Documents"].as_array().unwrap().iter().map(|d| d["tax"].as_f64().unwrap()).collect();
    assert_eq!(taxes, [50.0, 1000.0, 28000.0, 40000.0]);
    // cross partition, TOP, a parameter, and an exception in the function
    let r = h
        .query(
            "d",
            "c",
            &[("x-ms-documentdb-query-enablecrosspartition", "True")],
            json!({"query": "SELECT VALUE udf.Tax(t.income) FROM t WHERE t.income > @m", "parameters": [{"name": "@m", "value": 1000}]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["Documents"].as_array().unwrap().len(), 3);
    let r = h.query("d", "c", &hdr, json!({"query": "SELECT VALUE udf.Tax(t.missing) FROM t"})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("Exception = no input"), "{}", r.text());
    // an unknown function is a 400 before anything runs
    let r = h.query("d", "c", &hdr, json!({"query": "SELECT VALUE udf.Nope(t.income) FROM t"})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("udf.Nope"), "{}", r.text());
    // the query plan request accepts it
    let r = h
        .req(
            "POST",
            "dbs/d/colls/c/docs",
            &[
                ("content-type", "application/query+json"),
                ("x-ms-documentdb-isquery", "True"),
                ("x-ms-cosmos-is-query-plan-request", "True"),
            ],
            &json!({"query": "SELECT VALUE udf.Tax(t.income) FROM t"}).to_string(),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
}

async fn run_body(h: &H, body: &str) -> TestResponse {
    h.sproc("d", "c", "t", body).await;
    let r = h.exec("d", "c", "t", "p", json!([])).await;
    let _ = h.req("DELETE", "dbs/d/colls/c/sprocs/t", &[], "").await;
    r
}

#[tokio::test]
async fn javascript_query_api_filter_map_chain_sort_pluck_flatten_unwind() {
    let h = setup().await;
    let docs = [
        json!({"id": "a", "pk": "p", "n": 3, "Tags": ["x", "y"], "pets": [{"name": "rex"}, {"name": "tom"}]}),
        json!({"id": "b", "pk": "p", "n": 1, "Tags": ["z"], "pets": []}),
        json!({"id": "c", "pk": "p", "n": 2, "message": "hi"}),
        json!({"id": "other", "pk": "q", "n": 0, "Tags": ["no"]}),
    ];
    for d in &docs {
        let k = d["pk"].clone();
        assert_eq!(h.doc("d", "c", &k, d).await.status, 201);
    }
    // no callback: the result is written to the response body
    let r = run_body(&h, "function () { __.filter(function (d) { return d.n >= 2; }); }").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let mut ids: Vec<String> = r.json().as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    ids.sort();
    assert_eq!(ids, ["a", "c"], "only the partition of the request is seen");
    let r = run_body(&h, "function () { __.map(function (d) { return {id: d.id, msg: d.message}; }); }").await;
    assert_eq!(r.json().as_array().unwrap().len(), 3);
    // the cheat sheet of the Learn page: chain, filter, sortBy, pluck, flatten, value
    let r = run_body(
        &h,
        r#"function () {
        __.chain()
          .filter(function (doc) { return doc.Tags && Array.isArray(doc.Tags); })
          .sortBy(function (doc) { return doc.n; })
          .pluck("Tags")
          .flatten()
          .value();
    }"#,
    )
    .await;
    assert_eq!(r.json(), json!(["z", "x", "y"]), "{}", r.text());
    let r = run_body(
        &h,
        "function () { __.chain().sortByDescending(function (d) { return d.n; }).map(function (d) { return d.id; }).value(); }",
    )
    .await;
    assert_eq!(r.json(), json!(["a", "c", "b"]));
    let r = run_body(&h, "function () { __.chain().filter(function (d) { return d.pets; }).unwind(function (d) { return d.pets; }, function (d, pet) { return d.id + ':' + pet.name; }).value(); }").await;
    assert_eq!(r.json(), json!(["a:rex", "a:tom"]));
    let r = run_body(&h, "function () { __.chain().filter(function (d) { return d.pets && d.pets.length; }).unwind(function (d) { return d.pets; }).map(function (t) { return t[0].id + t[1].name; }).value(); }").await;
    assert_eq!(r.json(), json!(["arex", "atom"]), "unwind makes [document, item] tuples: {}", r.text());
    let r = run_body(&h, "function () { __.chain().pluck('Tags').flatten(true).value(function (e, feed, o) { getContext().getResponse().setBody({n: feed.length, more: o.continuation}); }); }").await;
    assert_eq!(
        r.json(),
        json!({"n": 4}),
        "a document without Tags gives an undefined entry, which flatten keeps; no continuation: {}",
        r.text()
    );
    // paging with options and a callback
    let r = run_body(
        &h,
        r#"function () {
        var out = [];
        function page(token) {
            var ok = __.map(function (d) { return d.id; }, { pageSize: 2, continuation: token }, function (e, feed, o) {
                out.push(feed.length);
                if (o.continuation) page(o.continuation); else getContext().getResponse().setBody(out);
            }).isAccepted;
        }
        page();
    }"#,
    )
    .await;
    assert_eq!(r.json(), json!([2, 1]), "{}", r.text());
}
