//! Strict authorisation of the Schema Registry (`docs/strict-auth.md`), as Azure answered in November 2026: every refusal is 401 `ErrorInRequest`
//! and names the actions that would have let the call through; a shared access signature is not taken at all (403 `NotAvaliable`).

mod common;

use std::sync::Arc;

use azemu_core::auth::{Assignment, Auth, Origin, TestAuthority};
use common::*;
use serde_json::Value;

const CONTRIBUTOR: &str = "5dffeca3-4936-4216-b2bc-10343a5abb25";
const SCHEMA_READER: &str = "2c56ea50-c6b3-40a6-83c0-9d98858bc7d2";
const READER: &str = "acdd72a7-3385-48ef-bd42-f606fba81ae7";
const NS_ID: &str = "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.EventHub/namespaces/myns";
const AUD: &str = "https://eventhubs.azure.net";
const AVRO: &str = "application/json; serialization=Avro";
const SCHEMA: &str = r#"{"type":"record","name":"R","namespace":"n","fields":[{"name":"a","type":"string"}]}"#;

fn assign(auth: &Auth, n: u8, role: &str, principal: &str, scope: &str) {
    auth.registry()
        .put_assignment(Assignment {
            id: format!("44444444-4444-4444-4444-4444444444{n:02}"),
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

async fn world() -> Env {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.EventHub/namespaces", "myns", NS_ID);
    assign(&auth, 1, CONTRIBUTOR, "contributor", NS_ID);
    assign(&auth, 2, READER, "reader", NS_ID); // the Azure `Reader` role: its `*/read` actions
    assign(&auth, 3, SCHEMA_READER, "schemareader", NS_ID);
    assign(&auth, 4, CONTRIBUTOR, "scoped", &format!("{NS_ID}/schemagroups/orders"));
    let shared = azemu_eventhubs::state::Shared::new(false, None);
    shared.set_auth(auth);
    let env = env_with(shared).await;
    for g in ["orders", "other"] {
        let r = env
            .srv
            .send(
                "PUT",
                &format!("{}/_azemu/eventhubs/myns/schemagroups/{g}", env.srv.gateway_url()),
                &[("content-type", "application/json")],
                "{}".to_string(),
            )
            .await;
        assert!(r.status < 300, "{}", r.text());
    }
    env
}

async fn call(env: &Env, who: &str, method: &str, path: &str, body: &str) -> azemu_core::testing::TestResponse {
    let tok = TestAuthority::token(who, AUD);
    call_with(env, &format!("Bearer {tok}"), method, path, body).await
}

async fn call_with(env: &Env, auth: &str, method: &str, path: &str, body: &str) -> azemu_core::testing::TestResponse {
    let sep = if path.contains('?') { '&' } else { '?' };
    let url = format!("{}{path}{sep}api-version=2023-07-01", env.srv.url("eventhub", "myns"));
    env.srv.send(method, &url, &[("authorization", auth), ("content-type", AVRO)], body.to_string()).await
}

fn message(r: &azemu_core::testing::TestResponse) -> String {
    let v: Value = r.json();
    v["error"]["message"].as_str().unwrap_or("").to_string()
}

#[tokio::test]
async fn a_contributor_writes_and_a_reader_reads_and_nothing_else() {
    let env = world().await;
    let r = call(&env, "contributor", "PUT", "/$schemaGroups/orders/schemas/order", SCHEMA).await;
    assert_eq!(r.status, 204, "{}", r.text());
    for who in ["reader", "schemareader", "contributor"] {
        assert_eq!(call(&env, who, "GET", "/$schemaGroups", "").await.status, 200, "{who}");
        assert_eq!(call(&env, who, "GET", "/$schemaGroups/orders", "").await.status, 200, "{who}");
        assert_eq!(call(&env, who, "GET", "/$schemaGroups/orders/schemas", "").await.status, 200, "{who}");
        assert_eq!(call(&env, who, "GET", "/$schemaGroups/orders/schemas/order/versions/1", "").await.status, 200, "{who}");
    }
    // the Azure `Reader` role (actions `*/read`) is enough to read a version that does not exist: 404, not 401 (found on Azure)
    assert_eq!(call(&env, "reader", "GET", "/$schemaGroups/orders/schemas/order/versions/9", "").await.status, 404);
    for who in ["reader", "schemareader"] {
        let r = call(&env, who, "PUT", "/$schemaGroups/orders/schemas/new", SCHEMA).await;
        assert_eq!(r.status, 401, "{who}");
        assert_eq!(r.json()["error"]["code"], "ErrorInRequest");
        assert_eq!(r.header("x-ms-error-code"), Some("ErrorInRequest"));
        assert!(
            message(&r).starts_with("Authorization failed for specified action: SchemaGroupWrite,SchemaWrite. TrackingId:"),
            "{}",
            message(&r)
        );
        assert!(message(&r).contains("SystemTracker:myns.servicebus.windows.net:$schemaGroups/orders/schemas/new"), "{}", message(&r));
        let r = call(&env, who, "DELETE", "/$schemaGroups/orders/schemas/order", "").await;
        assert_eq!(r.status, 401);
        assert!(
            message(&r).starts_with("Authorization failed for specified action: SchemaGroupWrite,SchemaGroupDelete,SchemaDelete."),
            "{}",
            message(&r)
        );
    }
    // the reader did not delete anything
    assert_eq!(call(&env, "reader", "GET", "/$schemaGroups/orders/schemas/order/versions/1", "").await.status, 200);
}

#[tokio::test]
async fn a_principal_with_no_role_is_refused_everywhere_with_the_actions_that_would_do() {
    let env = world().await;
    let cases = [
        ("PUT", "/$schemaGroups/orders/schemas/s", "SchemaGroupWrite,SchemaWrite."),
        ("POST", "/$schemaGroups/orders/schemas/s:get-id", "SchemaGroupRead,SchemaRead."),
        ("DELETE", "/$schemaGroups/orders/schemas/s", "SchemaGroupWrite,SchemaGroupDelete,SchemaDelete."),
        ("DELETE", "/$schemaGroups/orders/schemas/s/versions/1", "SchemaGroupWrite,SchemaGroupDelete,SchemaDelete."),
        ("GET", "/$schemaGroups", "SchemaGroupRead."),
        ("GET", "/$schemaGroups/orders", "SchemaGroupRead."),
        ("GET", "/$schemaGroups/orders/schemas", "SchemaGroupRead."),
        ("GET", "/$schemaGroups/orders/schemas/s/versions", "SchemaGroupRead,SchemaRead."),
        ("GET", "/$schemaGroups/orders/schemas/s/versions/1", "SchemaGroupRead,SchemaRead."),
        ("GET", "/$schemaGroups/$schemas/00000000000000000000000000000001", "SchemaGroupRead,SchemaRead."),
    ];
    for (method, path, words) in cases {
        let r = call(&env, "norole", method, path, SCHEMA).await;
        assert_eq!(r.status, 401, "{method} {path}");
        assert!(
            message(&r).starts_with(&format!("Authorization failed for specified action: {words} TrackingId:")),
            "{method} {path}: {}",
            message(&r)
        );
    }
}

#[tokio::test]
async fn a_role_on_one_group_is_none_on_another() {
    let env = world().await;
    assert_eq!(call(&env, "scoped", "PUT", "/$schemaGroups/orders/schemas/s", SCHEMA).await.status, 204);
    assert_eq!(call(&env, "scoped", "PUT", "/$schemaGroups/other/schemas/s", SCHEMA).await.status, 401);
    // and the namespace level list is not what the role covers
    assert_eq!(call(&env, "scoped", "GET", "/$schemaGroups", "").await.status, 401);
}

#[tokio::test]
async fn a_token_that_is_wrong_and_a_shared_access_signature() {
    let env = world().await;
    let good = TestAuthority::token("contributor", AUD);
    let r =
        call_with(&env, &format!("Bearer {}", TestAuthority::token("contributor", "https://vault.azure.net")), "GET", "/$schemaGroups", "")
            .await;
    assert_eq!(r.status, 401);
    assert!(message(&r).starts_with("InvalidAudience: Invalid authorization token audience. TrackingId:"), "{}", message(&r));
    let r = call_with(&env, &format!("Bearer {good}x"), "GET", "/$schemaGroups", "").await;
    assert!(message(&r).starts_with("InvalidSignature: Invalid authorization token signature. TrackingId:"), "{}", message(&r));
    let r = call_with(&env, "Bearer not-a-token", "GET", "/$schemaGroups/orders/schemas", "").await;
    assert!(message(&r).starts_with("MalformedToken: . TrackingId:"), "{}", message(&r));
    // a SAS token, good or not: 403 NotAvaliable (Azure's spelling)
    let sas = azemu_core::auth::sas::make_token(
        "https://myns.servicebus.windows.net/",
        4_000_000_000,
        "RootManageSharedAccessKey",
        "SAS_KEY_VALUE",
    );
    let r = call_with(&env, &sas, "GET", "/$schemaGroups", "").await;
    assert_eq!(r.status, 403);
    assert_eq!(r.json()["error"]["code"], "NotAvaliable");
    assert!(message(&r).starts_with("SubCode=40301, Nullable object must have a value. TrackingId:"), "{}", message(&r));
}

#[tokio::test]
async fn with_the_mode_off_any_bearer_token_still_passes() {
    let env = env_with(azemu_eventhubs::state::Shared::new(true, None)).await;
    let r = env
        .srv
        .send(
            "GET",
            &format!("{}/$schemaGroups?api-version=2023-07-01", env.srv.url("eventhub", "myns")),
            &[("authorization", "Bearer anything")],
            String::new(),
        )
        .await;
    assert_eq!(r.status, 200);
}
