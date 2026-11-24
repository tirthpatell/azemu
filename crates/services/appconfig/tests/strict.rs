//! Strict authorisation on App Configuration (`docs/strict-auth.md`): empty 403 and 401 answers, as Azure sent in November 2026.

use std::sync::Arc;

use azemu_appconfig::AppConfigService;
use azemu_core::auth::{Assignment, Auth, Origin, TestAuthority};
use azemu_core::testing::{TestResponse, TestServer};

const OWNER: &str = "5ae67dd6-50cb-40e7-96ff-dc2bfa4b606b";
const READER: &str = "516239f1-63e1-4d78-a4de-a74fb236a071";
const STORE_ID: &str = "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.AppConfiguration/configurationStores/store1";

fn give(auth: &Auth, principal: &str, role: &str) {
    auth.registry()
        .put_assignment(Assignment {
            id: azemu_core::auth::config::derived_guid(&[principal, role]),
            scope: STORE_ID.into(),
            role_id: role.into(),
            principal_id: principal.into(),
            principal_type: "ServicePrincipal".into(),
            condition: None,
            condition_version: None,
            description: None,
            created_on: String::new(),
            updated_on: String::new(),
            origin: Origin::Api,
        })
        .unwrap();
}

async fn call(srv: &TestServer, who: &str, method: &str, path: &str, body: &str) -> TestResponse {
    let t = format!("Bearer {}", TestAuthority::token(who, "https://store1.azconfig.io"));
    srv.send(
        method,
        &format!("{}{path}", srv.url("azconfig", "store1")),
        &[("authorization", &t), ("content-type", "application/json")],
        body.to_string(),
    )
    .await
}

#[tokio::test]
async fn roles_decide_and_the_refusals_have_no_body() {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.AppConfiguration/configurationStores", "store1", STORE_ID);
    let svc = AppConfigService::new();
    svc.set_auth(auth.clone());
    give(&auth, "main", OWNER);
    give(&auth, "reader", READER);
    let srv = TestServer::start(vec![Arc::new(svc)]).await;
    assert_eq!(call(&srv, "main", "PUT", "/kv/k1?api-version=2023-11-01", r#"{"value":"v"}"#).await.status, 200);
    // the owner: lists, locks, deletes
    assert_eq!(call(&srv, "main", "GET", "/kv?api-version=2023-11-01", "").await.status, 200);
    assert_eq!(call(&srv, "main", "PUT", "/locks/k1?api-version=2023-11-01", "").await.status, 200);
    assert_eq!(call(&srv, "main", "DELETE", "/locks/k1?api-version=2023-11-01", "").await.status, 200);
    // no role: 403, nothing in the body, no Content-Type, also for a missing key
    for (m, p) in [
        ("GET", "/kv/k1"),
        ("GET", "/kv/missing"),
        ("GET", "/kv"),
        ("HEAD", "/kv"),
        ("GET", "/keys"),
        ("GET", "/labels"),
        ("GET", "/snapshots"),
        ("PUT", "/kv/k2"),
        ("DELETE", "/kv/k1"),
    ] {
        let r = call(&srv, "norole", m, &format!("{p}?api-version=2023-11-01"), r#"{"value":"x"}"#).await;
        assert_eq!(r.status, 403, "{m} {p}");
        assert!(r.body.is_empty(), "{m} {p}");
        assert!(r.header("content-type").is_none(), "{m} {p}");
        assert!(r.header("www-authenticate").is_none());
        assert!(r.header("x-ms-request-id").is_some());
    }
    // a Data Reader reads everything, a missing key is 404, and nothing is written
    for p in ["/kv/k1", "/kv", "/keys", "/labels", "/revisions", "/snapshots"] {
        assert_eq!(call(&srv, "reader", "GET", &format!("{p}?api-version=2023-11-01"), "").await.status, 200, "{p}");
    }
    assert_eq!(call(&srv, "reader", "GET", "/kv/missing?api-version=2023-11-01", "").await.status, 404);
    for (m, p) in [("PUT", "/kv/k1"), ("PUT", "/kv/new"), ("DELETE", "/kv/k1"), ("PUT", "/locks/k1"), ("DELETE", "/locks/k1")] {
        let r = call(&srv, "reader", m, &format!("{p}?api-version=2023-11-01"), r#"{"value":"x"}"#).await;
        assert_eq!(r.status, 403, "{m} {p}");
    }
    assert_eq!(call(&srv, "main", "GET", "/kv/k1?api-version=2023-11-01", "").await.json()["value"], "v");
}

#[tokio::test]
async fn a_token_that_is_wrong_is_401_with_the_challenge_and_hmac_is_not_touched() {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    let svc = AppConfigService::new();
    svc.set_auth(auth.clone());
    let srv = TestServer::start(vec![Arc::new(svc)]).await;
    let url = format!("{}/kv?api-version=2023-11-01", srv.url("azconfig", "store1"));
    for t in [
        TestAuthority::token("main", "https://vault.azure.net"),
        format!("{}x", TestAuthority::token("main", "https://azconfig.io")),
        "garbage".to_string(),
    ] {
        let r = srv.send("GET", &url, &[("authorization", &format!("Bearer {t}"))], "").await;
        assert_eq!(r.status, 401);
        assert_eq!(
            r.header("www-authenticate"),
            Some("HMAC-SHA256, Bearer error=\"invalid_token\", error_description=\"Authorization token failed validation\"")
        );
        assert!(r.body.is_empty());
    }
    // a key is checked as it was: any `HMAC-SHA256` credential passes
    let r = srv.send("GET", &url, &[("authorization", "HMAC-SHA256 Credential=x&SignedHeaders=a&Signature=b")], "").await;
    assert_eq!(r.status, 200);
}
