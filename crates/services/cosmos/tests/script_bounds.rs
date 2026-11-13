//! Limits on scripts, through the REST API: nothing a script does may hang the emulator, use unbounded memory or crash it.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::testing::{TestResponse, TestServer};
use azemu_cosmos::{CosmosService, ScriptSettings};
use common::H;
use serde_json::json;

async fn start_with(settings: ScriptSettings) -> H {
    let svc = Arc::new(CosmosService::with_script_settings(settings));
    let srv = TestServer::start(vec![svc.clone()]).await;
    let base = srv.url("documents", "acct1");
    let h = H { srv, base, svc };
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h.coll("d", "other", "/pk").await;
    h
}

fn quick() -> ScriptSettings {
    ScriptSettings { timeout: Duration::from_millis(2500), memory_growth: 128 * 1024 * 1024, ..common::settings() }
}

async fn run(h: &H, body: &str) -> TestResponse {
    run_in(h, "c", body).await
}

async fn run_in(h: &H, coll: &str, body: &str) -> TestResponse {
    let r = h.json("POST", &format!("dbs/d/colls/{coll}/sprocs"), &[], &json!({ "id": "t", "body": body })).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = h.json("POST", &format!("dbs/d/colls/{coll}/sprocs/t"), &[("x-ms-documentdb-partitionkey", "[\"p\"]")], &json!([])).await;
    let _ = h.req("DELETE", &format!("dbs/d/colls/{coll}/sprocs/t"), &[], "").await;
    r
}

fn message(r: &TestResponse) -> String {
    r.json()["message"].as_str().unwrap_or("").to_string()
}

#[tokio::test]
async fn an_infinite_loop_is_stopped() {
    // the loop limit of a function call
    let h = start_with(ScriptSettings { loop_iterations: 300_000, timeout: Duration::from_secs(8), ..quick() }).await;
    let r = run(&h, "function () { while (true) {} }").await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(message(&r).contains("too many loop iterations"), "{}", message(&r));
    // with no loop limit the clock stops it: 408
    let h = start_with(ScriptSettings { loop_iterations: u64::MAX, ..quick() }).await;
    let t = Instant::now();
    let r = run(&h, "function () { while (true) {} }").await;
    assert_eq!(r.status, 408, "{}", r.text());
    assert!(message(&r).contains("ran longer than"), "{}", message(&r));
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[tokio::test]
async fn deep_recursion_is_an_error_not_a_crash() {
    let h = start_with(quick()).await;
    let r = run(&h, "function () { function f(n) { return f(n + 1) + 1; } f(0); }").await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(message(&r).contains("nested its calls too deeply"), "{}", message(&r));
    let r = run(&h, "function () { function f() { return [0].map(f); } f(); }").await;
    assert_eq!(r.status, 400, "{}", r.text());
    // a sensible recursion is fine
    let r =
        run(&h, "function () { function f(n) { return n == 0 ? 0 : 1 + f(n - 1); } getContext().getResponse().setBody(f(300)); }").await;
    assert_eq!((r.status, r.json()), (200, json!(300)), "{}", r.text());
}

#[tokio::test]
async fn a_string_doubled_forty_times_is_stopped_and_other_requests_go_on() {
    let h = start_with(quick()).await;
    let r =
        run(&h, "function () { var s = 'x'; for (var i = 0; i < 40; i++) { s = s + s; } getContext().getResponse().setBody(s.length); }")
            .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(message(&r).contains("used more than 128 MB of memory"), "{}", message(&r));
    let r = run(&h, "function () { getContext().getResponse().setBody('x'.repeat(1e10)); }").await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(message(&r).contains("RangeError"), "{}", message(&r));
    // arrays
    let r = run(&h, "function () { new Array(1e9).fill(0); }").await;
    assert_eq!(r.status, 400, "{}", r.text());
    let r = run(&h, "function () { var a = []; while (true) { a.push({x: 1}); } }").await;
    assert!(r.status == 400 || r.status == 408, "{}", r.text());
    // the service is still fine
    let r = run(&h, "function () { getContext().getResponse().setBody('ok'); }").await;
    assert_eq!((r.status, r.json()), (200, json!("ok")));
}

#[tokio::test]
async fn work_that_never_ends_but_has_no_loop_is_stopped_by_the_clock() {
    let h = start_with(quick()).await;
    let r = run(&h, "function () { function f(n) { return n < 2 ? n : f(n - 1) + f(n - 2); } f(60); }").await;
    assert_eq!(r.status, 408, "{}", r.text());
    // a callback chain that never ends: each collection call is accepted until the time is nearly up, then it answers false
    let r = run(
        &h,
        "function () { var n = 0; function again() { n++; var ok = __.readDocuments(__.getSelfLink(), again); if (!ok) getContext().getResponse().setBody({stopped: n > 1}); } again(); }",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!({"stopped": true}));
}

#[tokio::test]
async fn a_script_that_never_calls_back_or_finishes_its_promise_just_ends() {
    let h = start_with(quick()).await;
    let r = run(&h, "function () { new Promise(function () {}); __.readDocuments(__.getSelfLink(), function () {}); }").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.body.is_empty(), "no body was set");
}

#[tokio::test]
async fn the_response_body_is_limited_to_2_mb() {
    let h = start_with(quick()).await;
    let r = run(&h, "function () { getContext().getResponse().setBody('x'.repeat(2 * 1024 * 1024 + 10)); }").await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(message(&r).contains("larger than 2 MB"), "{}", message(&r));
    let r = run(&h, "function () { getContext().getResponse().setBody('x'.repeat(1024 * 1024)); }").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json().as_str().map(str::len), Some(1024 * 1024));
    // appendBody builds a body too
    let r = run(&h, "function () { var r = getContext().getResponse(); r.appendBody('ab'); r.appendBody('cd'); }").await;
    assert_eq!(r.json(), json!("abcd"));
}

#[tokio::test]
async fn text_that_the_parser_cannot_take_is_refused_before_it_runs() {
    let h = start_with(quick()).await;
    let deep = format!("function () {{ return {}1{}; }}", "(".repeat(2000), ")".repeat(2000));
    let r = run(&h, &deep).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(message(&r).contains("nests brackets"), "{}", message(&r));
    let r = run(&h, "function () { eval('1'); }").await;
    assert_eq!(r.status, 400);
    assert!(message(&r).contains("eval and the Function constructor are not supported"), "{}", message(&r));
    let r = run(&h, "function () { this is not javascript").await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(message(&r).contains("SyntaxError"), "{}", message(&r));
}

#[tokio::test]
async fn a_panic_in_the_engine_is_a_500_for_that_request_only() {
    let h = start_with(quick()).await;
    h.svc.inject_script_panic();
    let r = run(&h, "function () { getContext().getResponse().setBody(1); }").await;
    assert_eq!(r.status, 500, "{}", r.text());
    let r = run(&h, "function () { getContext().getResponse().setBody(2); }").await;
    assert_eq!((r.status, r.json()), (200, json!(2)), "{}", r.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_script_does_not_hold_up_other_containers_or_its_own() {
    let h = Arc::new(start_with(ScriptSettings { timeout: Duration::from_secs(4), ..common::settings() }).await);
    assert_eq!(h.doc("d", "other", &json!("p"), &json!({"id": "x", "pk": "p"})).await.status, 201);
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "y", "pk": "p"})).await.status, 201);
    let h2 = h.clone();
    let slow = tokio::spawn(async move {
        run(&h2, "function () { var t = Date.now(); while (Date.now() - t < 1500) {} getContext().getResponse().setBody('slow'); }")
            .await
            .status
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    for _ in 0..20 {
        for (coll, id) in [("other", "x"), ("c", "y")] {
            let r = h.req("GET", &format!("dbs/d/colls/{coll}/docs/{id}"), &[("x-ms-documentdb-partitionkey", "[\"p\"]")], "").await;
            assert_eq!(r.status, 200);
        }
        let r = h.doc("d", "other", &json!("p"), &json!({"id": format!("w{}", t.elapsed().as_nanos()), "pk": "p"})).await;
        assert_eq!(r.status, 201);
    }
    assert!(t.elapsed() < Duration::from_millis(1000), "reads and writes took {:?} while a script ran for 1.5 s", t.elapsed());
    assert_eq!(slow.await.unwrap(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scripts_on_the_same_partition_are_atomic_and_see_each_others_results() {
    let h = Arc::new(start_with(ScriptSettings { timeout: Duration::from_secs(8), ..common::settings() }).await);
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "ctr", "pk": "p", "n": 0})).await.status, 201);
    // read, think for 150 ms, write: without isolation concurrent runs would lose increments
    let body = "function () { __.readDocument(__.getAltLink() + '/docs/ctr', function (e, d) { var t = Date.now(); while (Date.now() - t < 150) {} d.n += 1; __.replaceDocument(d._self, d, function (e2) { if (e2) throw e2; }); }); }";
    let r = h.json("POST", "dbs/d/colls/c/sprocs", &[], &json!({"id": "inc", "body": body})).await;
    assert_eq!(r.status, 201);
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            h.json("POST", "dbs/d/colls/c/sprocs/inc", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], &json!([])).await.status
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    let r = h.req("GET", "dbs/d/colls/c/docs/ctr", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], "").await;
    assert_eq!(r.json()["n"], 4, "no increment is lost: {}", r.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_script_that_loses_the_race_against_plain_writes_gives_449_after_five_tries() {
    let h = Arc::new(start_with(ScriptSettings { timeout: Duration::from_secs(8), ..common::settings() }).await);
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "ctr", "pk": "p", "n": 0})).await.status, 201);
    let body = "function () { __.readDocument(__.getAltLink() + '/docs/ctr', function (e, d) { var t = Date.now(); while (Date.now() - t < 120) {} d.n = -1; __.replaceDocument(d._self, d, function () {}); }); }";
    assert_eq!(h.json("POST", "dbs/d/colls/c/sprocs", &[], &json!({"id": "slow", "body": body})).await.status, 201);
    let h2 = h.clone();
    let script = tokio::spawn(async move {
        h2.json("POST", "dbs/d/colls/c/sprocs/slow", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], &json!([])).await
    });
    // a writer that never stops: every run of the script sees the partition change before it can commit
    let mut n = 1;
    while !script.is_finished() {
        let r = h
            .json("PUT", "dbs/d/colls/c/docs/ctr", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], &json!({"id": "ctr", "pk": "p", "n": n}))
            .await;
        assert_eq!(r.status, 200);
        n += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let r = script.await.unwrap();
    assert_eq!(r.status, 449, "{}", r.text());
    let g = h.req("GET", "dbs/d/colls/c/docs/ctr", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], "").await;
    assert_ne!(g.json()["n"], -1, "the script's write was not applied");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_native_call_that_does_not_look_at_the_clock_is_killed_at_the_time_limit_and_frees_its_slot() {
    // a regular expression match is one native call: the engine's clock cannot reach it, the emulator's kill can
    let h = start_with(ScriptSettings { timeout: Duration::from_millis(500), max_running: 1, ..common::settings() }).await;
    let t = Instant::now();
    let r = run(&h, "function () { /(a+)+b/.test('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'); }").await;
    assert_eq!(r.status, 408, "{}", r.text());
    assert!(t.elapsed() >= Duration::from_millis(500), "{:?}", t.elapsed());
    assert!(t.elapsed() < Duration::from_millis(1500), "the worker is killed at the limit plus a short grace: {:?}", t.elapsed());
    // the only script slot is free again at once
    let r = run(&h, "function () { getContext().getResponse().setBody(1); }").await;
    assert_eq!(r.status, 200, "{}", r.text());
}

#[tokio::test]
async fn a_panic_in_a_udf_query_is_a_500_for_that_request_only() {
    let h = start_with(quick()).await;
    let r = h.json("POST", "dbs/d/colls/c/udfs", &[], &json!({"id": "f", "body": "function (x) { return x; }"})).await;
    assert_eq!(r.status, 201);
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p", "n": 1})).await.status, 201);
    let q = json!({"query": "SELECT VALUE udf.f(c.n) FROM c"});
    h.svc.inject_script_panic();
    let r = h.query("d", "c", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], q.clone()).await;
    assert_eq!(r.status, 500, "{}", r.text());
    let r = h.query("d", "c", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], q).await;
    assert_eq!((r.status, r.json()["Documents"].clone()), (200, json!([1])), "{}", r.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_udf_that_never_ends_stops_the_query_and_a_udf_cannot_reach_documents() {
    let h = start_with(quick()).await;
    for (id, body) in [
        ("spin", "function (x) { function f(n) { return n < 2 ? n : f(n - 1) + f(n - 2); } return f(60); }"),
        (
            "peek",
            "function (x) { var ok = getContext().getCollection().createDocument('dbs/d/colls/c', {id: 'sneaky', pk: 'p'}); return ok; }",
        ),
    ] {
        assert_eq!(h.json("POST", "dbs/d/colls/c/udfs", &[], &json!({"id": id, "body": body})).await.status, 201);
    }
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p", "n": 1})).await.status, 201);
    let hdr = [("x-ms-documentdb-partitionkey", "[\"p\"]")];
    let r = h.query("d", "c", &hdr, json!({"query": "SELECT VALUE udf.spin(c.n) FROM c"})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("ran out of time"), "{}", r.text());
    // a function is pure: the collection API of a function answers "not accepted" and writes nothing
    let r = h.query("d", "c", &hdr, json!({"query": "SELECT VALUE udf.peek(c.n) FROM c"})).await;
    assert_eq!((r.status, r.json()["Documents"].clone()), (200, json!([false])), "{}", r.text());
    assert_eq!(h.query("d", "c", &hdr, json!({"query": "SELECT VALUE c.id FROM c"})).await.json()["Documents"], json!(["a"]));
}
