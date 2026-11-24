//! Fault rules (`docs/faults.md`) against the whole stack: what Blob, Cosmos DB and Key Vault answer for each kind of rule, the
//! shape of the answers of the other HTTP services, a connection that is dropped, a delay that is measured, and the log line.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use azemu_core::serde_json::{self, json, Value};
use azemu_core::testing::{TestResponse, TestServer};

async fn server() -> TestServer {
    let azemu::Stack { services, raw } = azemu::full_stack();
    TestServer::start_with(services, raw).await
}

async fn rule(srv: &TestServer, r: Value) -> Value {
    let resp =
        srv.send("POST", &format!("{}/_azemu/faults", srv.gateway_url()), &[("content-type", "application/json")], r.to_string()).await;
    assert_eq!(resp.status, 201, "{}", resp.text());
    resp.json()
}

async fn call(srv: &TestServer, method: &str, label: &str, account: &str, path: &str, headers: &[(&str, &str)]) -> TestResponse {
    let mut r = srv.request(method, &format!("{}{path}", srv.url(label, account))).header("authorization", "Bearer test");
    for (k, v) in headers {
        r = r.header(k, v);
    }
    r.send().await
}

async fn blob(srv: &TestServer, method: &str, path: &str) -> TestResponse {
    call(srv, method, "blob", "acct", path, &[]).await
}

const COSMOS: &[(&str, &str)] = &[("x-ms-version", "2018-12-31"), ("authorization", "type=master&ver=1.0&sig=x")];

async fn cosmos(srv: &TestServer, method: &str, path: &str, body: Value, extra: &[(&str, &str)]) -> TestResponse {
    let mut headers: Vec<(&str, &str)> = COSMOS.to_vec();
    headers.extend_from_slice(extra);
    let mut r = srv.request(method, &format!("{}{path}", srv.url("documents", "acct")));
    for (k, v) in headers {
        r = r.header(k, v);
    }
    r.header("content-type", "application/json").body(body.to_string()).send().await
}

// ---- Blob --------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn blob_throttle_is_server_busy_in_xml() {
    let srv = server().await;
    let r = blob(&srv, "PUT", "/cont1?restype=container").await;
    assert_eq!(r.status, 201, "{}", r.text());
    rule(&srv, json!({"service":"blob","account":"acct","fault":{"kind":"throttle","retryAfterSeconds":2},"times":1})).await;
    let r = blob(&srv, "GET", "/cont1?restype=container").await;
    assert_eq!(r.status, 503, "{}", r.text());
    assert_eq!(r.header("x-ms-error-code"), Some("ServerBusy"));
    assert_eq!(r.header("retry-after"), Some("2"));
    assert_eq!(r.header("x-azemu-fault"), Some("throttle"));
    assert!(r.header("content-type").unwrap().starts_with("application/xml"));
    assert!(r.header("x-ms-request-id").is_some() && r.header("x-ms-version").is_some(), "the common headers of a storage answer");
    let body = r.text();
    assert!(body.contains("<Code>ServerBusy</Code>") && body.contains("The server is currently unable to receive requests"), "{body}");
    // the rule was for one request: the next is the real answer
    assert_eq!(blob(&srv, "GET", "/cont1?restype=container").await.status, 200);
    // another account is not touched by a rule for `acct`
    assert_eq!(call(&srv, "PUT", "blob", "other", "/cont1?restype=container", &[]).await.status, 201);
}

#[tokio::test]
async fn blob_error_is_the_answer_the_rule_spells_out() {
    let srv = server().await;
    rule(
        &srv,
        json!({"service":"blob","method":"PUT","path":"/cont1/*","fault":{"kind":"error","status":500,"code":"OperationTimedOut",
            "message":"made up","headers":{"x-custom":"1"}}}),
    )
    .await;
    assert_eq!(blob(&srv, "PUT", "/cont1?restype=container").await.status, 201, "the path of the rule is /cont1/*");
    let r = blob(&srv, "PUT", "/cont1/b1").await;
    assert_eq!(r.status, 500);
    assert_eq!(r.header("x-ms-error-code"), Some("OperationTimedOut"));
    assert_eq!(r.header("x-custom"), Some("1"));
    assert_eq!(r.header("x-azemu-fault"), Some("error"));
    assert!(r.text().contains("<Message>made up"), "{}", r.text());
    assert_eq!(blob(&srv, "GET", "/cont1/b1").await.status, 404, "the failed PUT stored nothing, and a GET is not matched");
    // a status with no code gets the one Azure uses for it
    rule(&srv, json!({"service":"blob","method":"DELETE","fault":{"kind":"error","status":503}})).await;
    let r = blob(&srv, "DELETE", "/cont1/b1").await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (503, Some("ServerBusy")));
}

#[tokio::test]
async fn blob_latency_is_measured_and_the_request_still_works() {
    let srv = server().await;
    rule(&srv, json!({"service":"blob","fault":{"kind":"latency","ms":400}})).await;
    let t = Instant::now();
    let r = blob(&srv, "PUT", "/cont1?restype=container").await;
    let took = t.elapsed();
    assert_eq!(r.status, 201, "the request is handled normally after the wait");
    assert!(r.header("x-azemu-fault").is_none());
    assert!(took >= Duration::from_millis(390) && took < Duration::from_secs(10), "{took:?}");
    // the data plane of another service is not slowed
    let t = Instant::now();
    call(&srv, "GET", "queue", "acct", "/q1?comp=metadata", &[]).await;
    assert!(t.elapsed() < Duration::from_millis(300), "{:?}", t.elapsed());
}

#[tokio::test]
async fn blob_drop_is_a_connection_with_no_answer() {
    let srv = server().await;
    rule(&srv, json!({"service":"blob","times":1,"fault":{"kind":"drop"}})).await;
    let url = format!("{}/cont1?restype=container", srv.url("blob", "acct"));
    let e = srv.client().try_send("PUT", &url, &[("authorization", "Bearer test")], "").await.expect_err("no answer");
    let text = format!("{e:#}").to_lowercase();
    assert!(text.contains("connection") || text.contains("closed") || text.contains("reset"), "{text}");
    // the request was not handled
    assert_eq!(blob(&srv, "GET", "/cont1?restype=container").await.status, 404);
}

#[tokio::test]
async fn blob_timeout_holds_the_request_then_closes() {
    let srv = server().await;
    rule(&srv, json!({"service":"blob","times":1,"fault":{"kind":"timeout","ms":500}})).await;
    let url = format!("{}/cont1?restype=container", srv.url("blob", "acct"));
    let t = Instant::now();
    let e = srv.client().try_send("PUT", &url, &[("authorization", "Bearer test")], "").await;
    assert!(e.is_err(), "no answer");
    assert!(t.elapsed() >= Duration::from_millis(480), "{:?}", t.elapsed());
}

#[tokio::test]
async fn dfs_throttle_is_the_json_form() {
    let srv = server().await;
    rule(&srv, json!({"service":"dfs","fault":{"kind":"throttle"}})).await;
    let r = call(&srv, "GET", "dfs", "acct", "/fs1?resource=filesystem&recursive=false", &[]).await;
    assert_eq!(r.status, 503, "{}", r.text());
    assert_eq!(r.json()["error"]["code"], "ServerBusy");
    assert_eq!(r.header("x-ms-error-code"), Some("ServerBusy"));
    // Blob is not the dfs endpoint
    assert_eq!(blob(&srv, "PUT", "/cont1?restype=container").await.status, 201);
}

// ---- Cosmos DB ---------------------------------------------------------------------------------------------------------

async fn cosmos_container(srv: &TestServer) {
    assert_eq!(cosmos(srv, "POST", "/dbs", json!({"id":"d"}), &[]).await.status, 201);
    let r = cosmos(srv, "POST", "/dbs/d/colls", json!({"id":"c","partitionKey":{"paths":["/pk"],"kind":"Hash"}}), &[]).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = cosmos(srv, "POST", "/dbs/d/colls/c/docs", json!({"id":"1","pk":"a"}), &[("x-ms-documentdb-partitionkey", r#"["a"]"#)]).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

#[tokio::test]
async fn cosmos_throttle_is_a_429_with_the_retry_header_and_substatus() {
    let srv = server().await;
    cosmos_container(&srv).await;
    rule(&srv, json!({"service":"cosmos","path":"/dbs/d/colls/c/docs/*","fault":{"kind":"throttle","retryAfterMs":750},"times":1})).await;
    let pk = [("x-ms-documentdb-partitionkey", r#"["a"]"#)];
    let r = cosmos(&srv, "GET", "/dbs/d/colls/c/docs/1", Value::Null, &pk).await;
    assert_eq!(r.status, 429, "{}", r.text());
    assert_eq!(r.header("x-ms-retry-after-ms"), Some("750"));
    assert_eq!(r.header("x-ms-substatus"), Some("3200"));
    assert_eq!(r.header("x-azemu-fault"), Some("throttle"));
    assert!(r.header("x-ms-activity-id").is_some());
    let body = r.json();
    assert_eq!(body["code"], "429");
    assert!(body["message"].as_str().unwrap().contains("Request rate is large"), "{body}");
    assert_eq!(cosmos(&srv, "GET", "/dbs/d/colls/c/docs/1", Value::Null, &pk).await.status, 200);
}

#[tokio::test]
async fn cosmos_error_and_a_throttle_with_no_wait() {
    let srv = server().await;
    cosmos_container(&srv).await;
    rule(&srv, json!({"service":"cosmos","method":"GET","times":1,"fault":{"kind":"throttle"}})).await;
    let pk = [("x-ms-documentdb-partitionkey", r#"["a"]"#)];
    let r = cosmos(&srv, "GET", "/dbs/d/colls/c/docs/1", Value::Null, &pk).await;
    assert_eq!(r.status, 429);
    assert!(r.header("x-ms-retry-after-ms").is_some(), "Azure always sends the wait with a 429");
    rule(&srv, json!({"service":"cosmos","method":"GET","fault":{"kind":"error","status":503,"message":"down"}})).await;
    let r = cosmos(&srv, "GET", "/dbs/d/colls/c/docs/1", Value::Null, &pk).await;
    assert_eq!(r.status, 503);
    assert!(r.header("x-ms-substatus").is_none());
    assert!(r.json()["message"].as_str().unwrap().contains("down"));
}

#[tokio::test]
async fn cosmos_latency_and_drop() {
    let srv = server().await;
    cosmos_container(&srv).await;
    let pk = [("x-ms-documentdb-partitionkey", r#"["a"]"#)];
    rule(&srv, json!({"service":"cosmos","fault":{"kind":"latency","ms":300,"jitterMs":100},"times":1})).await;
    let t = Instant::now();
    assert_eq!(cosmos(&srv, "GET", "/dbs/d/colls/c/docs/1", Value::Null, &pk).await.status, 200);
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(290) && took < Duration::from_secs(10), "{took:?}");
    rule(&srv, json!({"service":"cosmos","fault":{"kind":"drop"},"times":1})).await;
    let mut r = srv.client().request("GET", &format!("{}/dbs/d/colls/c/docs/1", srv.url("documents", "acct")));
    for (k, v) in COSMOS.iter().chain(pk.iter()) {
        r = r.header(k, v);
    }
    assert!(r.try_send().await.is_err());
}

#[tokio::test]
async fn cosmos_ru_budget_throttles_once_the_second_is_used_up() {
    let srv = server().await;
    cosmos_container(&srv).await;
    let pk = [("x-ms-documentdb-partitionkey", r#"["a"]"#)];
    // a point read costs about 1 RU, so a budget of 5 allows a few reads in a second
    rule(&srv, json!({"service":"cosmos","path":"/dbs/d/colls/c/docs/*","fault":{"kind":"ruBudget","ruPerSecond":5}})).await;
    let mut seen_ok = 0;
    let mut throttled = None;
    for _ in 0..40 {
        let r = cosmos(&srv, "GET", "/dbs/d/colls/c/docs/1", Value::Null, &pk).await;
        match r.status {
            200 => seen_ok += 1,
            429 => {
                throttled = Some(r);
                break;
            }
            s => panic!("unexpected {s}: {}", r.text()),
        }
    }
    let r = throttled.expect("a 429 within 40 reads");
    assert!(seen_ok >= 1, "the first reads are let through");
    let wait: u64 = r.header("x-ms-retry-after-ms").unwrap().parse().unwrap();
    assert!((1..=1000).contains(&wait), "{wait}: to the next second");
    assert_eq!(r.header("x-ms-substatus"), Some("3200"));
    assert_eq!(r.header("x-azemu-fault"), Some("ruBudget"));
    // a request for another path is not counted, and after the wait the budget is new
    assert_eq!(cosmos(&srv, "GET", "/dbs/d/colls/c", Value::Null, &[]).await.status, 200);
    tokio::time::sleep(Duration::from_millis(wait + 50)).await;
    assert_eq!(cosmos(&srv, "GET", "/dbs/d/colls/c/docs/1", Value::Null, &pk).await.status, 200);
    let list = srv.send("GET", &format!("{}/_azemu/faults", srv.gateway_url()), &[], "").await.json();
    assert!(list["rules"][0]["fired"].as_u64().unwrap() >= 1);
}

// ---- Key Vault ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn key_vault_throttle_error_latency_and_drop() {
    let srv = server().await;
    fn put(srv: &TestServer) -> azemu_core::testing::RequestBuilder<'_> {
        let url = format!("{}/secrets/s1?api-version=7.5", srv.url("vault", "v1"));
        srv.request("PUT", &url)
            .header("authorization", "Bearer t")
            .header("content-type", "application/json")
            .body(r#"{"value":"x"}"#.to_string())
    }
    rule(&srv, json!({"service":"keyvault","account":"v1","times":2,"fault":{"kind":"throttle","retryAfterSeconds":3}})).await;
    for _ in 0..2 {
        let r = put(&srv).send().await;
        assert_eq!(r.status, 429, "{}", r.text());
        assert_eq!(r.header("retry-after"), Some("3"));
        assert_eq!(r.header("x-azemu-fault"), Some("throttle"));
        assert_eq!(r.json()["error"]["code"], "Throttled");
        assert!(r.header("x-ms-keyvault-region").is_some(), "the headers every Key Vault answer has");
    }
    assert_eq!(put(&srv).send().await.status, 200, "the rule was for two requests");
    rule(&srv, json!({"service":"keyvault","method":"GET","fault":{"kind":"error","status":500,"code":"Boom","message":"bad day"}})).await;
    let r = srv
        .request("GET", &format!("{}/secrets/s1?api-version=7.5", srv.url("vault", "v1")))
        .header("authorization", "Bearer t")
        .send()
        .await;
    assert_eq!(r.status, 500);
    assert_eq!((r.json()["error"]["code"].clone(), r.json()["error"]["message"].clone()), (json!("Boom"), json!("bad day")));
    srv.send("DELETE", &format!("{}/_azemu/faults", srv.gateway_url()), &[], "").await;
    rule(&srv, json!({"service":"keyvault","fault":{"kind":"latency","ms":300}})).await;
    let t = Instant::now();
    assert_eq!(put(&srv).send().await.status, 200);
    assert!(t.elapsed() >= Duration::from_millis(290), "{:?}", t.elapsed());
    srv.send("DELETE", &format!("{}/_azemu/faults", srv.gateway_url()), &[], "").await;
    rule(&srv, json!({"service":"keyvault","fault":{"kind":"drop"},"times":1})).await;
    assert!(put(&srv).try_send().await.is_err());
    assert_eq!(put(&srv).send().await.status, 200);
}

// ---- the other HTTP services -------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_http_service_answers_a_throttle_in_its_own_shape() {
    let srv = server().await;
    rule(&srv, json!({"fault":{"kind":"throttle","retryAfterSeconds":1}})).await;
    let t = |label: &'static str, account: &'static str, path: &'static str| call_h(&srv, label, account, path);
    // Queue: 503 ServerBusy, XML
    let r = t("queue", "acct", "/q1?comp=metadata").await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (503, Some("ServerBusy")));
    assert!(r.text().contains("<Code>ServerBusy</Code>"));
    // Table: 503 ServerBusy, OData JSON
    let r = t("table", "acct", "/Tables").await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (503, Some("ServerBusy")));
    assert_eq!(r.json()["odata.error"]["code"], "ServerBusy");
    assert!(r.json()["odata.error"]["message"]["value"].as_str().unwrap().contains("unable to receive requests"));
    // App Configuration: 429, retry-after-ms, problem+json
    let r = t("azconfig", "store1", "/kv?api-version=1.0").await;
    assert_eq!(r.status, 429, "{}", r.text());
    assert_eq!(r.header("retry-after-ms"), Some("1000"));
    assert!(r.header("content-type").unwrap().starts_with("application/problem+json"));
    assert_eq!(r.json()["type"], "https://azconfig.io/errors/too-many-requests");
    assert_eq!(r.json()["status"], 429);
    // Event Grid and Resource Manager: 429, Retry-After, JSON error
    let r = t("eventgrid", "topic1", "/api/events?api-version=2018-01-01").await;
    assert_eq!((r.status, r.header("retry-after")), (429, Some("1")));
    assert_eq!(r.json()["error"]["code"], "TooManyRequests");
    let r = srv
        .request("GET", &format!("http://management.localhost:{}/subscriptions?api-version=2022-12-01", srv.port()))
        .header("authorization", "Bearer t")
        .send()
        .await;
    assert_eq!((r.status, r.header("retry-after")), (429, Some("1")), "{}", r.text());
    assert_eq!(r.json()["error"]["code"], "TooManyRequests");
    // Service Bus administration API: the Atom error
    let r = t("servicebus", "ns1", "/q1?api-version=2021-05").await;
    assert_eq!((r.status, r.header("retry-after")), (429, Some("1")), "{}", r.text());
    assert!(r.text().starts_with("<Error><Code>429</Code><Detail>ServerBusy"), "{}", r.text());
}

async fn call_h(srv: &TestServer, label: &str, account: &str, path: &str) -> TestResponse {
    call(srv, "GET", label, account, path, &[("accept", "application/json;odata=nometadata")]).await
}

// ---- the program -------------------------------------------------------------------------------------------------------

fn scratch(what: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("azemu-faults-test-{what}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

const BIN: &str = env!("CARGO_BIN_EXE_azemu");

fn base_command(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(dir).args(["--port", "0", "--cert-dir"]).arg(dir.join("ca")).args(args);
    for (k, _) in std::env::vars().filter(|(k, _)| k.starts_with("AZEMU_") && k != "AZEMU_GIT_COMMIT") {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

#[tokio::test]
async fn the_program_takes_the_file_and_logs_what_fired() {
    let dir = scratch("log");
    std::fs::write(
        dir.join("faults.json"),
        r#"{"rules":[{"service":"blob","method":"GET","fault":{"kind":"latency","ms":20}},{"service":"blob","fault":{"kind":"throttle","retryAfterSeconds":1}}]}"#,
    )
    .unwrap();
    let log = dir.join("stderr.log");
    let mut child = base_command(&dir, &["--faults", "faults.json"], &[])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("start azemu");
    let out = child.stdout.take().unwrap();
    let mut ready = String::new();
    BufReader::new(out).read_line(&mut ready).unwrap();
    let gateway = ready.split_whitespace().find_map(|w| w.strip_prefix("gateway=")).expect("gateway in the ready line").to_string();
    let client = azemu_core::testing::Client::new();
    let cfg = client.request("GET", &format!("{gateway}/_azemu/config")).send().await.json();
    assert!(cfg["faultsFile"].as_str().unwrap().ends_with("faults.json"), "{cfg}");
    assert_eq!(cfg["faultRules"], 2);
    assert!(cfg["unreadVariables"].as_array().is_none_or(|v| v.is_empty()), "{cfg}");
    let url = gateway.replace("127.0.0.1", "acct.blob.localhost");
    let r = client.request("GET", &format!("{url}/cont1?restype=container")).header("authorization", "Bearer t").send().await;
    assert_eq!(r.status, 503);
    // an admin path is not faulted
    assert_eq!(client.request("GET", &format!("{gateway}/_azemu/health")).send().await.status, 200);
    let _ = child.kill();
    let _ = child.wait();
    let text = std::fs::read_to_string(&log).unwrap();
    let line = text.lines().find(|l| l.contains("fault=")).unwrap_or_else(|| panic!("no fault in the log:\n{text}"));
    assert!(line.contains("fault=latency,throttle") && line.contains("rule=1,2") && line.contains("status=503"), "{line}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_program_does_not_start_with_a_wrong_faults_file_and_says_what_is_wrong() {
    let dir = scratch("bad");
    std::fs::write(dir.join("typo.json"), r#"{"rules":[{"servce":"blob","fault":{"kind":"drop"}}]}"#).unwrap();
    std::fs::write(dir.join("kind.json"), r#"{"rules":[{"fault":{"kind":"slow"}}]}"#).unwrap();
    for (args, env, want) in [
        (vec!["--faults", "typo.json"], vec![], vec!["typo.json", "rules[0]", "unknown field `servce`"]),
        (vec![], vec![("AZEMU_FAULTS", "kind.json")], vec!["kind.json", "unknown variant `slow`"]),
        (vec!["--faults", "missing.json"], vec![], vec!["missing.json", "cannot be read"]),
    ] {
        let out = base_command(&dir, &args, &env).stdin(Stdio::null()).output().expect("run azemu");
        assert_eq!(out.status.code(), Some(1));
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        for w in want {
            assert!(err.contains(w), "wanted '{w}' in:\n{err}");
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_settings_table_lists_the_variable() {
    let out = Command::new(BIN).arg("__settings-table").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("`AZEMU_FAULTS`"));
    let _ = serde_json::Value::Null;
}

// ---- Kafka -------------------------------------------------------------------------------------------------------------

/// What a Kafka client sends first: `ApiVersions` v0 with no client id. The answer starts with its length.
const API_VERSIONS: [u8; 14] = [0, 0, 0, 10, 0, 18, 0, 0, 0, 0, 0, 1, 0xff, 0xff];

/// Send it to the gateway (which tells Kafka from HTTP by the first bytes) and read the first four bytes of the answer.
/// `None`: the connection was closed with no answer.
async fn kafka_answer(port: u16, wait: Duration) -> (Option<[u8; 4]>, Duration) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let started = Instant::now();
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let _ = s.write_all(&API_VERSIONS).await;
    let mut four = [0u8; 4];
    let got = match tokio::time::timeout(wait, s.read_exact(&mut four)).await {
        Ok(Ok(_)) => Some(four),
        Ok(Err(_)) => None,
        Err(_) => panic!("still waiting after {wait:?}"),
    };
    (got, started.elapsed())
}

#[tokio::test]
async fn kafka_takes_latency_drop_and_timeout_but_no_answer_rules() {
    let srv = server().await;
    let port = srv.port();
    let (got, took) = kafka_answer(port, Duration::from_secs(10)).await;
    assert!(got.is_some() && took < Duration::from_millis(300), "{got:?} {took:?}: no rule, a normal answer");

    // `management` is every request that is not a produce or a fetch
    rule(&srv, json!({"service":"kafka","operation":"management","times":1,"fault":{"kind":"latency","ms":500}})).await;
    let (got, took) = kafka_answer(port, Duration::from_secs(10)).await;
    assert!(got.is_some(), "the request is answered after the wait");
    assert!(took >= Duration::from_millis(480), "{took:?}");

    rule(&srv, json!({"service":"kafka","operation":"management","times":1,"fault":{"kind":"drop"}})).await;
    let (got, _) = kafka_answer(port, Duration::from_secs(10)).await;
    assert!(got.is_none(), "the connection was closed with no answer");

    rule(&srv, json!({"service":"kafka","operation":"management","times":1,"fault":{"kind":"timeout","ms":400}})).await;
    let (got, took) = kafka_answer(port, Duration::from_secs(10)).await;
    assert!(got.is_none() && took >= Duration::from_millis(380), "{got:?} {took:?}: held, then closed");

    rule(&srv, json!({"service":"kafka","operation":"connect","times":1,"fault":{"kind":"drop"}})).await;
    let (got, _) = kafka_answer(port, Duration::from_secs(10)).await;
    assert!(got.is_none(), "a connection that is dropped when it opens");
    let (got, _) = kafka_answer(port, Duration::from_secs(10)).await;
    assert!(got.is_some(), "all the rules are spent");

    // a throttle for the kafka service is refused, and one with no service is left out for Kafka
    let r = srv
        .send(
            "POST",
            &format!("{}/_azemu/faults", srv.gateway_url()),
            &[("content-type", "application/json")],
            json!({"service":"kafka","fault":{"kind":"throttle"}}).to_string(),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    rule(&srv, json!({"fault":{"kind":"throttle"}})).await;
    let (got, _) = kafka_answer(port, Duration::from_secs(10)).await;
    assert!(got.is_some(), "Kafka has no throttling answer");
}
