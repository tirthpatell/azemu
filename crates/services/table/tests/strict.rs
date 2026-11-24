//! Strict authorisation on the Table endpoint (`docs/strict-auth.md`, the answers of Azure in November 2026).

use std::sync::Arc;

use azemu_core::auth::{Assignment, Auth, Origin, TestAuthority};
use azemu_core::testing::{TestResponse, TestServer};
use azemu_table::TableService;

const VERSION: &str = "2019-02-02";
const TABLE_CONTRIBUTOR: &str = "0a9a7e1f-b9d0-4cc4-a60d-0319b160aaa3";
const TABLE_READER: &str = "76199698-9eea-4c19-bc75-cec21354c6b6";
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
    let t = TableService::new();
    t.set_auth(auth.clone());
    give(&auth, "main", TABLE_CONTRIBUTOR, ACCT_ID);
    give(&auth, "main", READER_ROLE, ACCT_ID);
    give(&auth, "reader", TABLE_READER, ACCT_ID);
    give(&auth, "reader", READER_ROLE, ACCT_ID);
    give(&auth, "scoped", TABLE_READER, &format!("{ACCT_ID}/tableServices/default/tables/tab1"));
    TestServer::start(vec![Arc::new(t)]).await
}

async fn call(srv: &TestServer, who: &str, method: &str, path: &str, body: &str) -> TestResponse {
    let t = format!("Bearer {}", TestAuthority::token(who, "https://storage.azure.com"));
    srv.send(
        method,
        &format!("{}{path}", srv.url("table", "acct")),
        &[
            ("authorization", &t),
            ("x-ms-version", VERSION),
            ("content-type", "application/json"),
            ("accept", "application/json;odata=nometadata"),
        ],
        body.to_string(),
    )
    .await
}

fn assert_refused(r: &TestResponse, what: &str) {
    assert_eq!(r.status, 403, "{what}: {}", r.text());
    // Azure's Table answer has no x-ms-error-code
    assert!(r.header("x-ms-error-code").is_none(), "{what}");
    assert_eq!(r.header("content-type"), Some("application/json;odata=nometadata;streaming=true;charset=utf-8"));
    let v = r.json();
    assert_eq!(v["odata.error"]["code"], "AuthorizationPermissionMismatch");
    assert_eq!(v["odata.error"]["message"]["lang"], "en-US");
    assert!(v["odata.error"]["message"]["value"]
        .as_str()
        .unwrap()
        .starts_with("This request is not authorized to perform this operation using this permission.\nRequestId:"));
}

#[tokio::test]
async fn a_table_reader_reads_and_a_missing_role_is_refused_in_odata_json() {
    let srv = start().await;
    assert_eq!(call(&srv, "main", "POST", "/Tables", r#"{"TableName":"tab1"}"#).await.status, 201);
    assert_eq!(call(&srv, "main", "POST", "/tab1", r#"{"PartitionKey":"p","RowKey":"r","v":1}"#).await.status, 201);
    // norole
    for (m, p) in [
        ("GET", "/Tables"),
        ("GET", "/tab1()"),
        ("GET", "/tab1(PartitionKey='p',RowKey='r')"),
        ("GET", "/?restype=service&comp=properties"),
    ] {
        assert_refused(&call(&srv, "norole", m, p, "").await, &format!("{m} {p}"));
    }
    assert_refused(&call(&srv, "norole", "POST", "/Tables", r#"{"TableName":"tab2"}"#).await, "create table");
    assert_refused(&call(&srv, "norole", "POST", "/tab1", r#"{"PartitionKey":"p","RowKey":"x"}"#).await, "insert");
    // a table that does not exist is 404 on delete even for a principal with no role; an existing one is refused
    assert_eq!(call(&srv, "norole", "DELETE", "/Tables('nothere')", "").await.status, 404);
    assert_refused(&call(&srv, "norole", "DELETE", "/Tables('tab1')", "").await, "delete table");
    // the reader
    for (m, p) in [
        ("GET", "/Tables"),
        ("GET", "/tab1()"),
        ("GET", "/tab1(PartitionKey='p',RowKey='r')"),
        ("GET", "/?restype=service&comp=properties"),
    ] {
        let r = call(&srv, "reader", m, p, "").await;
        assert_eq!(r.status, 200, "{m} {p}: {}", r.text());
    }
    assert_refused(&call(&srv, "reader", "POST", "/tab1", r#"{"PartitionKey":"p","RowKey":"y"}"#).await, "insert");
    assert_refused(&call(&srv, "reader", "PUT", "/tab1(PartitionKey='p',RowKey='r')", r#"{"v":2}"#).await, "upsert");
    assert_refused(&call(&srv, "reader", "DELETE", "/tab1(PartitionKey='p',RowKey='r')", "").await, "delete entity");
    assert_refused(&call(&srv, "reader", "POST", "/Tables", r#"{"TableName":"tab3"}"#).await, "create table");
    assert_refused(&call(&srv, "reader", "GET", "/tab1?comp=acl", "").await, "acl");
    // a role on one table
    assert_eq!(call(&srv, "scoped", "GET", "/tab1()", "").await.status, 200);
    assert_eq!(call(&srv, "main", "POST", "/Tables", r#"{"TableName":"tab2"}"#).await.status, 201);
    assert_refused(&call(&srv, "scoped", "GET", "/tab2()", "").await, "other table");
    assert_refused(&call(&srv, "scoped", "GET", "/Tables", "").await, "list tables");
    // nothing changed
    assert_eq!(call(&srv, "main", "GET", "/tab1()", "").await.json()["value"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_wrong_audience_is_403_for_the_version_the_python_sdk_sends_and_401_for_a_newer_one() {
    let srv = start().await;
    let t = format!("Bearer {}", TestAuthority::token("main", "https://vault.azure.net"));
    for (version, status, code) in [("2019-02-02", 403, "AuthenticationFailed"), ("2021-08-06", 401, "InvalidAuthenticationInfo")] {
        let r =
            srv.send("GET", &format!("{}/Tables", srv.url("table", "acct")), &[("authorization", &t), ("x-ms-version", version)], "").await;
        assert_eq!(r.status, status, "{version}");
        assert_eq!(r.json()["odata.error"]["code"], code);
        // no detail about the audience in the Table answer
        assert!(!r.text().contains("Audience"));
    }
}
