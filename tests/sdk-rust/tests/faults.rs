//! Fault rules (`docs/faults.md`) through the Rust SDK: Blob Storage retries what the emulator throttles or drops (and waits for
//! `Retry-After`), a client with retries off sees the error, and Cosmos DB shows what its client does with a 429.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use azemu_sdk_rust_tests::{cred, emu, uniq};
use azure_core::http::{ClientOptions, ExponentialRetryOptions, RequestContent, RetryOptions, StatusCode, Url};
use azure_storage_blob::clients::BlobServiceClientOptions;
use azure_storage_blob::{BlobClient, BlobServiceClient};

// ---- the admin API --------------------------------------------------------------------------------------------------

fn admin(method: &str, path: &str, body: &str) -> (u16, String) {
    let port = emu().port;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let text = out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, text)
}

/// Adds a rule; returns its id. The rule is removed when the guard is dropped, so a test leaves nothing behind for the others
/// (they share one emulator).
struct Rule(u64);

impl Rule {
    fn add(json: serde_json::Value) -> Rule {
        let (status, text) = admin("POST", "/_azemu/faults", &json.to_string());
        assert_eq!(status, 201, "{text}");
        Rule(serde_json::from_str::<serde_json::Value>(&text).unwrap()["id"].as_u64().unwrap())
    }
    fn fired(&self) -> u64 {
        let (_, text) = admin("GET", "/_azemu/faults", "");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        v["rules"].as_array().unwrap().iter().find(|r| r["id"] == self.0).expect("the rule")["fired"].as_u64().unwrap()
    }
}

impl Drop for Rule {
    fn drop(&mut self) {
        admin("DELETE", &format!("/_azemu/faults/{}", self.0), "");
    }
}

// ---- Blob -----------------------------------------------------------------------------------------------------------

fn retrying(max_retries: u32) -> RetryOptions {
    RetryOptions::exponential(ExponentialRetryOptions {
        initial_delay: azure_core::time::Duration::milliseconds(50),
        max_retries,
        ..Default::default()
    })
}

fn blob_service(account: &str, retry: RetryOptions) -> BlobServiceClient {
    let url = Url::parse(&emu().https("blob", account)).unwrap();
    let options = BlobServiceClientOptions { client_options: ClientOptions { retry, ..Default::default() }, ..Default::default() };
    BlobServiceClient::new(url, Some(cred()), Some(options)).unwrap()
}

/// A container with one blob, made with a client that is not faulted (no rule exists yet).
async fn a_blob() -> (String, String, String) {
    let (account, container, blob) = (uniq("acct"), uniq("c"), uniq("b"));
    let svc = blob_service(&account, RetryOptions::none());
    svc.blob_container_client(&container).create(None).await.unwrap();
    svc.blob_container_client(&container).blob_client(&blob).upload(RequestContent::from(b"hello".to_vec()), None).await.unwrap();
    (account, container, blob)
}

fn blob_client(account: &str, container: &str, blob: &str, retry: RetryOptions) -> BlobClient {
    blob_service(account, retry).blob_container_client(container).blob_client(blob)
}

#[tokio::test]
async fn blob_retries_a_throttled_download_and_waits_for_retry_after() {
    let (account, container, blob) = a_blob().await;
    let rule = Rule::add(serde_json::json!({"service": "blob", "account": account, "method": "GET", "times": 2,
        "fault": {"kind": "throttle", "retryAfterSeconds": 1}}));
    let c = blob_client(&account, &container, &blob, retrying(4));
    let start = Instant::now();
    let r = c.download(None).await.expect("the download succeeds after two throttled answers");
    let body = r.body.collect().await.unwrap();
    let took = start.elapsed();
    assert_eq!(&body[..], b"hello");
    assert_eq!(rule.fired(), 2);
    // the retry policy reads Retry-After on a 503: two waits of a second
    assert!(took >= Duration::from_millis(1900) && took < Duration::from_secs(10), "{took:?}");
}

#[tokio::test]
async fn blob_with_retries_off_raises_server_busy() {
    let (account, container, blob) = a_blob().await;
    let _rule = Rule::add(serde_json::json!({"service": "blob", "account": account, "method": "GET",
        "fault": {"kind": "throttle", "retryAfterSeconds": 3}}));
    let c = blob_client(&account, &container, &blob, RetryOptions::none());
    let e = c.download(None).await.err().expect("an error");
    assert_eq!(e.http_status(), Some(StatusCode::ServiceUnavailable));
    match e.kind() {
        azure_core::error::ErrorKind::HttpResponse { error_code, .. } => assert_eq!(error_code.as_deref(), Some("ServerBusy")),
        other => panic!("not an HTTP response error: {other:?}"),
    }
}

#[tokio::test]
async fn blob_error_rule_gives_the_code_it_spells_out() {
    let (account, container, blob) = a_blob().await;
    let _rule = Rule::add(serde_json::json!({"service": "blob", "account": account, "method": "DELETE",
        "fault": {"kind": "error", "status": 500, "code": "InternalError", "message": "on purpose"}}));
    let c = blob_client(&account, &container, &blob, RetryOptions::none());
    let e = c.delete(None).await.expect_err("an error");
    assert_eq!(e.http_status(), Some(StatusCode::InternalServerError));
    assert!(format!("{e:?}").contains("on purpose"), "{e:?}");
}

#[tokio::test]
async fn blob_retries_after_a_dropped_connection() {
    let (account, container, blob) = a_blob().await;
    let rule =
        Rule::add(serde_json::json!({"service": "blob", "account": account, "method": "GET", "times": 1, "fault": {"kind": "drop"}}));
    let c = blob_client(&account, &container, &blob, retrying(3));
    let r = c.download(None).await.expect("the dropped connection is retried");
    assert_eq!(&r.body.collect().await.unwrap()[..], b"hello");
    assert_eq!(rule.fired(), 1);
}

#[tokio::test]
async fn blob_with_retries_off_sees_a_dropped_connection_as_a_transport_error() {
    let (account, container, blob) = a_blob().await;
    let _rule =
        Rule::add(serde_json::json!({"service": "blob", "account": account, "method": "GET", "times": 1, "fault": {"kind": "drop"}}));
    let c = blob_client(&account, &container, &blob, RetryOptions::none());
    let e = c.download(None).await.err().expect("an error");
    assert!(e.http_status().is_none(), "no HTTP answer: {e:?}");
    assert!(matches!(e.kind(), azure_core::error::ErrorKind::Io | azure_core::error::ErrorKind::Connection), "{:?}", e.kind());
}

#[tokio::test]
async fn blob_latency_is_waited_for_and_a_shorter_timeout_gives_up() {
    let (account, container, blob) = a_blob().await;
    let _rule = Rule::add(serde_json::json!({"service": "blob", "account": account, "method": "GET", "times": 1,
        "fault": {"kind": "latency", "ms": 1500}}));
    let c = blob_client(&account, &container, &blob, RetryOptions::none());
    let start = Instant::now();
    // the caller's own time limit: the call is dropped when it passes
    let r = tokio::time::timeout(Duration::from_millis(500), c.download(None)).await;
    assert!(r.is_err(), "the call did not finish within 500 ms");
    assert!(start.elapsed() < Duration::from_millis(1400));
    // the rule is spent by the first request; a second one is not slow
    let start = Instant::now();
    let r = c.download(None).await.unwrap();
    assert_eq!(&r.body.collect().await.unwrap()[..], b"hello");
    assert!(start.elapsed() < Duration::from_millis(1000));
    // and the slow one, waited for
    let _rule = Rule::add(serde_json::json!({"service": "blob", "account": account, "method": "GET", "times": 1,
        "fault": {"kind": "latency", "ms": 700}}));
    let start = Instant::now();
    c.download(None).await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(650), "{:?}", start.elapsed());
}

// ---- Cosmos DB ------------------------------------------------------------------------------------------------------

mod cosmos {
    use super::*;
    use azure_core::credentials::Secret;
    use azure_data_cosmos::models::ContainerProperties;
    use azure_data_cosmos::{AccountEndpoint, AccountReference, CosmosClient, RoutingStrategy};

    const KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

    async fn client(account: &str) -> CosmosClient {
        std::env::set_var("AZURE_COSMOS_BINARY_ENCODING_ENABLED", "false");
        let endpoint: AccountEndpoint = emu().https("documents", account).parse().unwrap();
        let reference = AccountReference::with_authentication_key(endpoint, Secret::from(KEY.to_string()));
        CosmosClient::builder().build(reference, RoutingStrategy::ProximityTo("East US".into())).await.unwrap()
    }

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Doc {
        id: String,
        pk: String,
        n: i64,
    }

    #[tokio::test]
    async fn cosmos_client_waits_for_the_retry_header_of_a_429() {
        let account = uniq("cos");
        let c = client(&account).await;
        let db = uniq("db");
        c.create_database(&db, None).await.unwrap();
        let d = c.database_client(&db);
        d.create_container(ContainerProperties::new("items", "/pk".into()), None).await.unwrap();
        let items = d.container_client("items", None).await.unwrap();
        items.create_item("a", "1", Doc { id: "1".into(), pk: "a".into(), n: 7 }, None).await.unwrap();

        let rule = Rule::add(serde_json::json!({"service": "cosmos", "account": account, "method": "GET",
            "path": format!("/dbs/{db}/colls/items/docs/*"), "times": 2,
            "fault": {"kind": "throttle", "retryAfterMs": 600}}));
        let start = Instant::now();
        let r = items.read_item("a", "1", None).await;
        let took = start.elapsed();
        let fired = rule.fired();
        match r {
            Ok(r) => {
                assert_eq!(r.into_model::<Doc>().unwrap(), Doc { id: "1".into(), pk: "a".into(), n: 7 });
                assert_eq!(fired, 2);
                assert!(took >= Duration::from_millis(1100) && took < Duration::from_secs(10), "{took:?}");
            }
            Err(e) => panic!("the read should succeed after two throttled answers (rule fired {fired} times, {took:?}): {e:?}"),
        }
    }

    #[tokio::test]
    async fn cosmos_client_raises_the_429_when_the_throttle_does_not_end() {
        let account = uniq("cos");
        let c = client(&account).await;
        let db = uniq("db");
        c.create_database(&db, None).await.unwrap();
        let d = c.database_client(&db);
        d.create_container(ContainerProperties::new("items", "/pk".into()), None).await.unwrap();
        let items = d.container_client("items", None).await.unwrap();
        items.create_item("a", "1", Doc { id: "1".into(), pk: "a".into(), n: 7 }, None).await.unwrap();
        let _rule = Rule::add(serde_json::json!({"service": "cosmos", "account": account, "method": "GET",
            "path": format!("/dbs/{db}/colls/items/docs/*"), "fault": {"kind": "throttle", "retryAfterMs": 20}}));
        let e = items.read_item("a", "1", None).await.err().expect("a 429 in the end");
        assert_eq!(e.status().status_code(), StatusCode::TooManyRequests, "{e:?}");
    }
}
