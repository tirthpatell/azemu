mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn no_authorization_is_401_with_challenge() {
    let t = start().await;
    let url = format!("{}/kv?api-version=2024-09-01", t.base);
    let r = t.srv.send("GET", &url, &[], "").await;
    assert_eq!(r.status, 401);
    assert_eq!(r.header("www-authenticate"), Some("HMAC-SHA256, Bearer"));
    assert_eq!(r.text(), "", "the service sends no body");
    // other schemes are no credentials
    let r = t.srv.send("GET", &url, &[("Authorization", "Basic abc")], "").await;
    assert_eq!(r.status, 401);
    let r = t.srv.send("GET", &url, &[("Authorization", "Bearer")], "").await;
    assert_eq!(r.status, 401);
    // the check comes before the api-version check
    let r = t.srv.send("GET", &format!("{}/kv", t.base), &[], "").await;
    assert_eq!(r.status, 401);
}

#[tokio::test]
async fn hmac_and_bearer_credentials_are_accepted_unchecked() {
    let t = start().await;
    let url = format!("{}/kv?api-version=2024-09-01", t.base);
    assert_eq!(t.srv.send("GET", &url, &[HMAC], "").await.status, 200);
    assert_eq!(t.srv.send("GET", &url, &[AUTH], "").await.status, 200);
    assert_eq!(t.srv.send("GET", &url, &[("authorization", "bearer lower")], "").await.status, 200);
}

#[tokio::test]
async fn unauthenticated_requests_still_get_common_headers() {
    let t = start().await;
    let r = t.srv.send("GET", &format!("{}/kv?api-version=1.0", t.base), &[], "").await;
    assert!(r.header("x-ms-request-id").is_some());
    assert!(r.header("sync-token").is_some());
}

#[tokio::test]
async fn api_version_as_azure_takes_it() {
    let t = start().await;
    // no version is taken, and answers in the older shape: no `etag` in the body (tried in November 2026)
    let r = t.srv.send("GET", &format!("{}/kv", t.base), &[AUTH], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json(), json!({"items": []}));
    for v in ["1.0", "2022-11-01-preview"] {
        assert!(t.call_v(v, "GET", "/kv", &[], "").await.json().get("etag").is_none(), "{v}");
    }
    for v in ["2023-10-01", "2023-11-01", "2024-09-01", "2026-04-01"] {
        let r = t.call_v(v, "GET", "/kv", &[], "").await;
        assert_eq!(r.status, 200, "{v}");
        assert!(r.json()["etag"].is_string(), "{v}");
    }
    for v in ["1999-01-01", "2030-01-01", "2022-11-01", "2025-01-01", "2026-04-01-preview", "2.0", "1.1"] {
        let r = t.call_v(v, "GET", "/kv", &[], "").await;
        assert_eq!(r.status, 400, "{v}");
        assert_eq!(r.header("content-type"), Some("application/problem+json; charset=utf-8"));
        let j = r.json();
        assert_eq!(j["type"], "https://azconfig.io/errors/invalid-argument");
        assert_eq!((j["title"].as_str(), j["name"].as_str()), (Some("Unsupported API version"), Some("api-version")));
        assert_eq!(
            j["detail"],
            format!("The HTTP resource that matches the request URI '{}/kv' does not support the API version '{v}'.", t.base)
        );
    }
    // text that is no version at all has another title
    let r = t.call_v("banana", "GET", "/kv", &[], "").await;
    assert_eq!((r.status, r.json()["title"].clone()), (400, json!("Invalid API version")));
}

#[tokio::test]
async fn unbuilt_operations_answer_501_problem_json_naming_the_operation() {
    let t = start().await;
    let r = t.call("PATCH", "/kv/k", &[], "{}").await;
    assert_eq!(r.status, 501);
    assert_eq!(r.header("content-type"), Some("application/problem+json; charset=utf-8"));
    assert!(r.json()["title"].as_str().unwrap().contains("PATCH /kv/k"));
    assert_eq!(r.json()["status"], 501);
    assert_eq!(t.call("GET", "/nothing", &[], "").await.status, 501);
    assert_eq!(t.call("DELETE", "/snapshots/x", &[], "").await.status, 501);
    // preflight is not built either (and needs no credentials)
    let url = format!("{}/kv?api-version=2024-09-01", t.base);
    assert_eq!(t.srv.send("OPTIONS", &url, &[], "").await.status, 501);
}

#[tokio::test]
async fn client_request_id_is_echoed_and_request_ids_differ() {
    let t = start().await;
    let a = t.call("GET", "/kv", &[("x-ms-client-request-id", "abc")], "").await;
    let b = t.call("GET", "/kv", &[], "").await;
    assert_eq!(a.header("x-ms-client-request-id"), Some("abc"));
    assert!(b.header("x-ms-client-request-id").is_none());
    assert_ne!(a.header("x-ms-request-id"), b.header("x-ms-request-id"));
}

#[tokio::test]
async fn works_over_https_with_a_bearer_token() {
    let t = start().await;
    let https = t.srv.https_url("azconfig", "teststore");
    let r = t
        .srv
        .request("PUT", &format!("{https}/kv/k?api-version=2024-09-01"))
        .header("Authorization", "Bearer x")
        .json(&json!({"value": "v"}))
        .send()
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(t.get("/kv/k").await.json()["value"], "v");
}
