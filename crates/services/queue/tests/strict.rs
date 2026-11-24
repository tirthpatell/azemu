//! Strict authorisation on the Queue endpoint (`docs/strict-auth.md`, the answers of Azure in November 2026).

use std::sync::Arc;

use azemu_core::auth::{Assignment, Auth, Origin, TestAuthority};
use azemu_core::testing::{TestResponse, TestServer};
use azemu_queue::QueueService;

const VERSION: &str = "2024-11-04";
const QUEUE_CONTRIBUTOR: &str = "974c5e8b-45b9-4653-ba55-5f855dd0fb88";
const QUEUE_READER: &str = "19e7f393-937e-4f77-808e-94535e297925";
const PROCESSOR: &str = "8a0f0c08-91a1-4084-bc3d-661d67233fed";
const SENDER: &str = "c6a89b2d-59bc-44d0-9896-0f6e12d7b80a";
const READER_ROLE: &str = "acdd72a7-3385-48ef-bd42-f606fba81ae7";
const ACCT_ID: &str = "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.Storage/storageAccounts/acct";

fn give(auth: &Auth, principal: &str, role: &str, scope: &str) {
    auth.registry()
        .put_assignment(Assignment {
            id: azemu_core::auth::config::derived_guid(&[principal, role, scope]),
            scope: scope.into(),
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

async fn start() -> TestServer {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.Storage/storageAccounts", "acct", ACCT_ID);
    let q = QueueService::new();
    q.set_auth(auth.clone());
    give(&auth, "main", QUEUE_CONTRIBUTOR, ACCT_ID);
    give(&auth, "reader", QUEUE_READER, ACCT_ID);
    give(&auth, "reader", READER_ROLE, ACCT_ID);
    give(&auth, "processor", PROCESSOR, ACCT_ID);
    give(&auth, "sender", SENDER, ACCT_ID);
    give(&auth, "scoped", QUEUE_READER, &format!("{ACCT_ID}/queueServices/default/queues/que1"));
    TestServer::start(vec![Arc::new(q)]).await
}

async fn call(srv: &TestServer, who: &str, method: &str, path: &str, body: &str) -> TestResponse {
    let t = format!("Bearer {}", TestAuthority::token(who, "https://storage.azure.com"));
    srv.send(method, &format!("{}{path}", srv.url("queue", "acct")), &[("authorization", &t), ("x-ms-version", VERSION)], body.to_string())
        .await
}

fn code(r: &TestResponse) -> String {
    r.header("x-ms-error-code").unwrap_or("").to_string()
}

const MSG: &str = "<QueueMessage><MessageText>aGk=</MessageText></QueueMessage>";

#[tokio::test]
async fn roles_decide_who_may_send_peek_and_receive() {
    let srv = start().await;
    assert_eq!(call(&srv, "main", "PUT", "/que1", "").await.status, 201);
    assert_eq!(call(&srv, "main", "POST", "/que1/messages", MSG).await.status, 201);
    // no role
    for (m, p) in [
        ("GET", "/que1/messages?peekonly=true"),
        ("GET", "/que1/messages"),
        ("GET", "/?comp=list"),
        ("GET", "/que1?comp=metadata"),
        ("DELETE", "/que1"),
    ] {
        let r = call(&srv, "norole", m, p, "").await;
        assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"), "{m} {p}");
        assert!(r.text().contains("This request is not authorized to perform this operation using this permission."));
    }
    let r = call(&srv, "norole", "PUT", "/que2", "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationFailure"));
    // a Data Reader lists, peeks, reads the metadata; receiving is a different right
    for (m, p) in [
        ("GET", "/?comp=list"),
        ("GET", "/que1/messages?peekonly=true"),
        ("GET", "/que1?comp=metadata"),
        ("GET", "/?restype=service&comp=properties"),
    ] {
        let r = call(&srv, "reader", m, p, "").await;
        assert_eq!(r.status, 200, "{m} {p}: {}", r.text());
    }
    for (m, p, b) in [
        ("GET", "/que1/messages", ""),
        ("POST", "/que1/messages", MSG),
        ("DELETE", "/que1/messages", ""),
        ("PUT", "/que1?comp=metadata", ""),
        ("GET", "/que1?comp=acl", ""),
        ("DELETE", "/que1", ""),
    ] {
        let r = call(&srv, "reader", m, p, b).await;
        assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"), "{m} {p}");
    }
    let r = call(&srv, "reader", "PUT", "/que3", "").await;
    assert_eq!(code(&r), "AuthorizationFailure");
    // a sender sends and nothing else
    assert_eq!(call(&srv, "sender", "POST", "/que1/messages", MSG).await.status, 201);
    assert_eq!(call(&srv, "sender", "GET", "/que1/messages?peekonly=true", "").await.status, 403);
    // a processor receives
    assert_eq!(call(&srv, "processor", "GET", "/que1/messages", "").await.status, 200);
    // a role on one queue
    assert_eq!(call(&srv, "scoped", "GET", "/que1/messages?peekonly=true", "").await.status, 200);
    assert_eq!(call(&srv, "scoped", "GET", "/que2/messages?peekonly=true", "").await.status, 403);
    assert_eq!(call(&srv, "scoped", "GET", "/?comp=list", "").await.status, 403);
}

#[tokio::test]
async fn a_bad_token_has_the_blob_answer() {
    let srv = start().await;
    let t = format!("Bearer {}", TestAuthority::token("main", "https://vault.azure.net"));
    let r =
        srv.send("GET", &format!("{}/?comp=list", srv.url("queue", "acct")), &[("authorization", &t), ("x-ms-version", VERSION)], "").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "InvalidAuthenticationInfo"));
    assert!(r.header("www-authenticate").is_some());
    assert!(r.text().contains("Audience validation failed. Audience did not match."));
}
