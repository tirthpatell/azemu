//! Fault rules through the admin API and the HTTP path of the server (`docs/faults.md`).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::resp::Resp;
use azemu_core::testing::TestServer;
use azemu_core::{async_trait, Emulator, FaultAnswer, Mount, Request, Response, Service};
use common::*;

/// A service that words a throttle in its own way: 503 with a body of its own and a header of its own.
struct Worded;

#[async_trait]
impl Service for Worded {
    fn name(&self) -> &'static str {
        "worded"
    }
    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("worded")]
    }
    async fn handle(&self, _req: Request) -> Response {
        Resp::new(200u16).text("fine").build()
    }
    fn reset(&self) {}
    fn fault_response(&self, _req: &Request, a: &FaultAnswer) -> Response {
        let r = Resp::new(a.status_or(503)).text(format!("{} {}", a.code_or("Busy"), a.message_or("slow down")));
        match a.retry_after_ms() {
            Some(ms) => r.header("x-wait-ms", ms),
            None => r,
        }
        .build()
    }
}

async fn server() -> TestServer {
    TestServer::start(vec![
        Arc::new(Probe::new("alpha", vec![Mount::AccountHost("alpha")])) as Arc<dyn Service>,
        Arc::new(Probe::new("beta", vec![Mount::AccountHost("beta")])) as Arc<dyn Service>,
        Arc::new(Worded) as Arc<dyn Service>,
    ])
    .await
}

async fn add(srv: &TestServer, rule: serde_json::Value) -> serde_json::Value {
    let r = srv.send("POST", &format!("{}/_azemu/faults", srv.gateway_url()), JSON, rule.to_string()).await;
    assert_eq!(r.status, 201, "{}", r.text());
    r.json()
}

async fn get(srv: &TestServer, path: &str) -> azemu_core::testing::TestResponse {
    srv.send("GET", &format!("{}{path}", srv.url("alpha", "acct")), &[], "").await
}

#[tokio::test]
async fn nothing_changes_without_a_rule() {
    let srv = server().await;
    let r = get(&srv, "/x").await;
    assert_eq!(r.status, 200);
    assert!(r.header("x-azemu-fault").is_none());
    let r = srv.send("GET", &format!("{}/_azemu/faults", srv.gateway_url()), &[], "").await;
    assert_eq!((r.status, r.json()), (200, serde_json::json!({"rules": []})));
}

#[tokio::test]
async fn post_get_delete_one_and_all() {
    let srv = server().await;
    let base = format!("{}/_azemu/faults", srv.gateway_url());
    let one = add(&srv, serde_json::json!({"service":"alpha","fault":{"kind":"throttle","retryAfterSeconds":3}})).await;
    assert_eq!(one["id"], 1);
    assert_eq!(one["fired"], 0);
    let two = add(&srv, serde_json::json!({"service":"beta","fault":{"kind":"drop"}})).await;
    assert_eq!(two["id"], 2);

    let r = get(&srv, "/x").await;
    assert_eq!(r.status, 429, "the generic answer of a service with no shape of its own");
    assert_eq!(r.header("x-azemu-fault"), Some("throttle"));
    assert_eq!(r.header("retry-after"), Some("3"));
    assert_eq!(r.json()["error"]["code"], "TooManyRequests");
    let list = srv.send("GET", &base, &[], "").await.json();
    assert_eq!(list["rules"][0]["fired"], 1);
    assert_eq!(list["rules"][1]["fired"], 0);

    let r = srv.send("DELETE", &format!("{base}/1"), &[], "").await;
    assert_eq!((r.status, r.json()), (200, serde_json::json!({})));
    assert_eq!(get(&srv, "/x").await.status, 200);
    let r = srv.send("DELETE", &format!("{base}/1"), &[], "").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["code"], "FaultRuleNotFound");
    let r = srv.send("DELETE", &format!("{base}/abc"), &[], "").await;
    assert_eq!(r.status, 400);

    let r = srv.send("DELETE", &base, &[], "").await;
    assert_eq!((r.status, r.json()), (200, serde_json::json!({"removed": 1})));
    assert_eq!(srv.send("GET", &base, &[], "").await.json(), serde_json::json!({"rules": []}));
}

#[tokio::test]
async fn put_replaces_the_list_and_checks_every_rule_before_changing_anything() {
    let srv = server().await;
    let base = format!("{}/_azemu/faults", srv.gateway_url());
    add(&srv, serde_json::json!({"fault":{"kind":"drop"}})).await;
    let r = srv
        .send(
            "PUT",
            &base,
            JSON,
            r#"{"rules":[{"service":"alpha","fault":{"kind":"throttle"}},{"service":"beta","fault":{"kind":"throttle"}}]}"#,
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let ids: Vec<u64> = r.json()["rules"].as_array().unwrap().iter().map(|x| x["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, vec![1, 2], "the new list starts again at 1");
    // one bad rule: nothing is replaced
    let r = srv.send("PUT", &base, JSON, r#"{"rules":[{"fault":{"kind":"drop"}},{"fault":{"kind":"error","status":200}}]}"#).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("rules[1]") && r.text().contains("400 to 599"), "{}", r.text());
    assert_eq!(srv.send("GET", &base, &[], "").await.json()["rules"].as_array().unwrap().len(), 2);
    // an empty list is the same as DELETE
    let r = srv.send("PUT", &base, JSON, r#"{"rules":[]}"#).await;
    assert_eq!(r.json(), serde_json::json!({"rules": []}));
}

#[tokio::test]
async fn bad_requests_say_what_is_wrong() {
    let srv = server().await;
    let base = format!("{}/_azemu/faults", srv.gateway_url());
    for (method, body, want) in [
        ("POST", r#"{"fault":{"kind":"throttle"},"servce":"alpha"}"#, "unknown field `servce`"),
        ("POST", r#"{"service":"gamma","fault":{"kind":"drop"}}"#, "no such service is mounted. Mounted: alpha, beta, worded"),
        ("POST", r#"{"probability":2,"fault":{"kind":"drop"}}"#, "between 0 and 1"),
        ("POST", r#"{"fault":{"kind":"error","status":302}}"#, "400 to 599"),
        ("POST", r#"{"fault":{"kind":"slow"}}"#, "unknown variant `slow`"),
        ("POST", r#"{"rules":[]}"#, "POST adds one rule"),
        ("POST", "not json", "not valid JSON"),
        ("POST", "", "not valid JSON"),
        ("PUT", r#"{"rule":[]}"#, "unknown field `rule`"),
        ("PUT", r#"[]"#, "The body is"),
        ("PUT", r#"{"rules":{}}"#, "'rules' must be a list"),
    ] {
        let r = srv.send(method, &base, JSON, body).await;
        assert_eq!(r.status, 400, "{body}: {}", r.text());
        assert_eq!(r.json()["error"]["code"], "InvalidFaultRule");
        assert!(r.text().contains(want), "{body}: wanted '{want}' in {}", r.text());
    }
    let r = srv.send("PATCH", &base, JSON, "{}").await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET, HEAD, POST, PUT, DELETE"));
    // nothing was added by any of that
    assert_eq!(srv.send("GET", &base, &[], "").await.json()["rules"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_service_words_its_own_answer_and_the_rule_can_add_headers() {
    let srv = server().await;
    add(&srv, serde_json::json!({"service":"worded","times":1,"fault":{"kind":"throttle","retryAfterMs":1500}})).await;
    add(&srv, serde_json::json!({"service":"worded","fault":{"kind":"error","status":500,"code":"Boom","message":"it broke","headers":{"x-extra":"yes","x-azemu-fault":"mine"}}})).await;
    let url = format!("{}/x", srv.url("worded", "a"));
    let r = srv.send("GET", &url, &[], "").await;
    assert_eq!((r.status, r.text().as_str()), (503, "Busy slow down"));
    assert_eq!(r.header("x-wait-ms"), Some("1500"));
    assert_eq!(r.header("x-azemu-fault"), Some("throttle"));
    // the first rule is spent; the second is an error the rule spelled out
    let r = srv.send("GET", &url, &[], "").await;
    assert_eq!((r.status, r.text().as_str()), (500, "Boom it broke"));
    assert_eq!(r.header("x-extra"), Some("yes"));
    assert_eq!(r.header("x-azemu-fault"), Some("error"), "the kind is the emulator's to say");
}

#[tokio::test]
async fn skip_and_times_over_http() {
    let srv = server().await;
    add(
        &srv,
        serde_json::json!({"service":"alpha","method":"GET","path":"/orders/*","skip":1,"times":2,"fault":{"kind":"error","status":503}}),
    )
    .await;
    let mut seen = Vec::new();
    for p in ["/orders/1", "/other", "/orders/2", "/orders/3", "/orders/4"] {
        seen.push(get(&srv, p).await.status);
    }
    assert_eq!(seen, vec![200, 200, 503, 503, 200]);
    let r = get(&srv, "/orders/5").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn latency_waits_then_answers_normally() {
    let srv = server().await;
    add(&srv, serde_json::json!({"service":"alpha","fault":{"kind":"latency","ms":300}})).await;
    let t = Instant::now();
    let r = get(&srv, "/x").await;
    let took = t.elapsed();
    assert_eq!(r.status, 200);
    assert!(r.header("x-azemu-fault").is_none(), "the answer is the service's own");
    assert!(took >= Duration::from_millis(290) && took < Duration::from_secs(5), "{took:?}");
    // the other service is not slowed
    let t = Instant::now();
    srv.send("GET", &format!("{}/x", srv.url("beta", "acct")), &[], "").await;
    assert!(t.elapsed() < Duration::from_millis(250));
}

#[tokio::test]
async fn drop_closes_the_connection_with_no_answer() {
    let srv = server().await;
    add(&srv, serde_json::json!({"service":"alpha","times":1,"fault":{"kind":"drop"}})).await;
    let url = format!("{}/x", srv.url("alpha", "acct"));
    let e = srv.client().try_send("GET", &url, &[], "").await.expect_err("no answer");
    let text = format!("{e:#}");
    assert!(text.contains("connection") || text.contains("closed") || text.contains("reset"), "{text}");
    // the rule is spent; the emulator is fine
    assert_eq!(srv.send("GET", &url, &[], "").await.status, 200);
    assert_eq!(srv.send("GET", &format!("{}/_azemu/faults", srv.gateway_url()), &[], "").await.json()["rules"][0]["fired"], 1);
}

#[tokio::test]
async fn timeout_holds_the_request_then_closes() {
    let srv = server().await;
    add(&srv, serde_json::json!({"service":"alpha","times":1,"fault":{"kind":"timeout","ms":400}})).await;
    let url = format!("{}/x", srv.url("alpha", "acct"));
    let t = Instant::now();
    let e = srv.client().try_send("GET", &url, &[], "").await.expect_err("no answer");
    assert!(t.elapsed() >= Duration::from_millis(390), "{:?} {e:#}", t.elapsed());
    assert_eq!(srv.send("GET", &url, &[], "").await.status, 200);
    // with no ms the request is held until the client gives up
    add(&srv, serde_json::json!({"service":"alpha","times":1,"fault":{"kind":"timeout"}})).await;
    let r = tokio::time::timeout(Duration::from_millis(500), srv.client().try_send("GET", &url, &[], "")).await;
    assert!(r.is_err(), "still waiting after half a second");
    assert_eq!(srv.send("GET", &url, &[], "").await.status, 200, "the emulator took other requests meanwhile");
}

#[tokio::test]
async fn admin_paths_are_never_faulted() {
    let srv = server().await;
    add(&srv, serde_json::json!({"fault":{"kind":"drop"}})).await;
    add(&srv, serde_json::json!({"fault":{"kind":"latency","ms":2000}})).await;
    let t = Instant::now();
    for p in ["/_azemu/health", "/_azemu/config", "/_azemu/faults", "/_azemu/"] {
        let r = srv.send("GET", &format!("{}{p}", srv.gateway_url()), &[], "").await;
        assert_eq!(r.status, 200, "{p}");
    }
    // also on a service's host
    let r = srv.send("GET", &format!("{}/_azemu/health", srv.url("alpha", "a")), &[], "").await;
    assert_eq!(r.status, 200);
    assert!(t.elapsed() < Duration::from_secs(1));
    assert_eq!(srv.send("DELETE", &format!("{}/_azemu/faults", srv.gateway_url()), &[], "").await.status, 200);
}

#[tokio::test]
async fn full_reset_removes_api_rules_but_a_partial_reset_does_not() {
    let srv = server().await;
    let base = srv.gateway_url();
    add(&srv, serde_json::json!({"service":"alpha","fault":{"kind":"error","status":500}})).await;
    srv.send("POST", &format!("{base}/_azemu/reset?service=alpha"), &[], "").await;
    assert_eq!(get(&srv, "/x").await.status, 500, "a reset of one service leaves the rules");
    srv.send("POST", &format!("{base}/_azemu/reset?account=acct"), &[], "").await;
    assert_eq!(get(&srv, "/x").await.status, 500);
    srv.send("POST", &format!("{base}/_azemu/reset"), &[], "").await;
    assert_eq!(get(&srv, "/x").await.status, 200);
    add(&srv, serde_json::json!({"service":"alpha","fault":{"kind":"error","status":500}})).await;
    srv.reset();
    assert_eq!(get(&srv, "/x").await.status, 200, "the reset of the test API too");
}

#[tokio::test]
async fn index_and_config_list_the_endpoints() {
    let srv = server().await;
    let r = srv.send("GET", &format!("{}/_azemu/", srv.gateway_url()), &[], "").await;
    let paths: Vec<String> = r.json()["endpoints"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap().to_string()).collect();
    assert!(paths.contains(&"/_azemu/faults".to_string()), "{paths:?}");
    add(&srv, serde_json::json!({"fault":{"kind":"drop"}})).await;
    let cfg = srv.send("GET", &format!("{}/_azemu/config", srv.gateway_url()), &[], "").await.json();
    assert_eq!(cfg["faultRules"], 1);
    assert!(cfg["faultsFile"].is_null());
}

/// What an admin request with a body must say (`docs/faults.md`).
const JSON: &[(&str, &str)] = &[("content-type", "application/json")];

fn write_file(tag: &str, text: &str) -> std::path::PathBuf {
    let dir = temp_dir(tag);
    let f = dir.join("faults.json");
    std::fs::write(&f, text).unwrap();
    f
}

#[tokio::test]
async fn a_faults_file_exists_from_the_start_and_comes_back_after_a_reset() {
    let file = write_file(
        "faults-file",
        r#"{"rules":[{"service":"alpha","path":"/slow/*","fault":{"kind":"error","status":503,"code":"FromFile"}}]}"#,
    );
    let dir = temp_dir("faults-file-cert");
    let emu = Emulator::builder()
        .gateway(loopback0())
        .cert_dir(dir)
        .faults_file(&file)
        .service(Arc::new(Probe::new("alpha", vec![Mount::AccountHost("alpha")])))
        .start()
        .await
        .unwrap();
    let port = gw_port(&emu);
    let client = azemu_core::testing::Client::new();
    let url = |p: &str| format!("http://acct.alpha.localhost:{port}{p}");
    let admin = |p: &str| format!("http://127.0.0.1:{port}{p}");
    assert_eq!(client.send("GET", &url("/slow/1"), &[], "").await.status, 503);
    assert_eq!(client.send("GET", &url("/fast"), &[], "").await.status, 200);
    // an API rule is added, then everything is reset: the file's rule is back with its id, the API rule is gone
    let r = client.send("POST", &admin("/_azemu/faults"), JSON, r#"{"fault":{"kind":"error","status":500}}"#).await;
    assert_eq!(r.json()["id"], 2);
    assert_eq!(client.send("GET", &url("/fast"), &[], "").await.status, 500);
    client.send("POST", &admin("/_azemu/reset"), &[], "").await;
    assert_eq!(client.send("GET", &url("/fast"), &[], "").await.status, 200);
    assert_eq!(client.send("GET", &url("/slow/1"), &[], "").await.status, 503);
    let cfg = client.send("GET", &admin("/_azemu/config"), &[], "").await.json();
    assert_eq!(cfg["faultRules"], 1);
    assert!(cfg["faultsFile"].as_str().unwrap().ends_with("faults.json"));
    // DELETE removes the file's rule too, until the next full reset
    client.send("DELETE", &admin("/_azemu/faults"), &[], "").await;
    assert_eq!(client.send("GET", &url("/slow/1"), &[], "").await.status, 200);
    client.send("POST", &admin("/_azemu/reset"), &[], "").await;
    assert_eq!(client.send("GET", &url("/slow/1"), &[], "").await.status, 503);
    emu.shutdown().await;
}

#[tokio::test]
async fn a_faults_file_that_is_wrong_stops_the_start_and_names_the_problem() {
    let start = |file: std::path::PathBuf| async move {
        Emulator::builder()
            .gateway(loopback0())
            .cert_dir(temp_dir("faults-bad"))
            .faults_file(file)
            .service(Arc::new(Probe::new("alpha", vec![Mount::AccountHost("alpha")])))
            .start()
            .await
            .err()
            .map(|e| format!("{e:#}"))
    };
    let missing = std::env::temp_dir().join("azemu-faults-no-such-file.json");
    let e = start(missing).await.expect("must fail");
    assert!(e.contains("cannot be read") && e.contains("azemu-faults-no-such-file.json"), "{e}");
    let e = start(write_file("f-json", "{ not json")).await.expect("must fail");
    assert!(e.contains("is not valid JSON"), "{e}");
    let e = start(write_file("f-field", r#"{"rules":[{"fault":{"kind":"drop"}},{"servce":"alpha","fault":{"kind":"drop"}}]}"#))
        .await
        .expect("must fail");
    assert!(e.contains("rules[1]") && e.contains("unknown field `servce`") && e.contains("faults.json"), "{e}");
    let e = start(write_file("f-top", r#"{"rule":[]}"#)).await.expect("must fail");
    assert!(e.contains("unknown field `rule`"), "{e}");
    let e = start(write_file("f-svc", r#"{"rules":[{"service":"blob","fault":{"kind":"drop"}}]}"#)).await.expect("must fail");
    assert!(e.contains("no such service is mounted"), "{e}");
    let e = start(temp_dir("f-dir")).await.expect("must fail");
    assert!(e.contains("is a directory"), "{e}");
}

#[tokio::test]
async fn probability_with_a_seed_is_repeatable() {
    let count = |seed: u64| async move {
        let dir = temp_dir("faults-seed");
        let emu = Emulator::builder()
            .gateway(loopback0())
            .cert_dir(dir)
            .faults_seed(seed)
            .service(Arc::new(Probe::new("alpha", vec![Mount::AccountHost("alpha")])))
            .start()
            .await
            .unwrap();
        let port = gw_port(&emu);
        let client = azemu_core::testing::Client::new();
        client
            .send(
                "POST",
                &format!("http://127.0.0.1:{port}/_azemu/faults"),
                JSON,
                r#"{"probability":0.5,"fault":{"kind":"error","status":500}}"#,
            )
            .await;
        let mut pattern = String::new();
        for _ in 0..40 {
            let s = client.send("GET", &format!("http://a.alpha.localhost:{port}/x"), &[], "").await.status;
            pattern.push(if s == 500 { 'x' } else { '.' });
        }
        emu.shutdown().await;
        pattern
    };
    let a = count(11).await;
    assert_eq!(a, count(11).await);
    assert!(a.contains('x') && a.contains('.'), "{a}");
}
