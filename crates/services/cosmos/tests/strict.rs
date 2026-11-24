//! Strict authorisation on Cosmos DB (`docs/strict-auth.md`, the answers of Azure in November 2026): the three kinds of 403.

mod common;

use std::sync::Arc;

use azemu_core::auth::{Auth, CosmosAssignment, Origin, TestAuthority};
use azemu_core::testing::TestResponse;
use common::*;
use serde_json::json;

const READER: &str = "00000000-0000-0000-0000-000000000001";
const CONTRIBUTOR: &str = "00000000-0000-0000-0000-000000000002";

fn give(auth: &Auth, principal: &str, role: &str, scope: &str) {
    auth.registry().put_cosmos_assignment(
        "acct1",
        CosmosAssignment {
            id: format!("{principal}-{role}"),
            role_id: role.into(),
            principal_id: principal.into(),
            scope: scope.into(),
            origin: Origin::Api,
        },
    );
}

async fn as_who(h: &H, who: &str, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> TestResponse {
    let token = TestAuthority::token(who, "https://acct1.documents.azure.com");
    let enc = format!("type%3Daad%26ver%3D1.0%26sig%3D{token}");
    let mut hs: Vec<(&str, &str)> = vec![
        ("authorization", &enc),
        ("x-ms-date", "Wed, 11 Nov 2026 12:00:00 GMT"),
        ("x-ms-version", "2020-07-15"),
        ("content-type", "application/json"),
    ];
    hs.extend_from_slice(extra);
    h.srv.send(method, &format!("{}/{}", h.base, path.trim_start_matches('/')), &hs, body.to_string()).await
}

fn sub(r: &TestResponse) -> String {
    r.header("x-ms-substatus").unwrap_or("").to_string()
}

#[tokio::test]
async fn the_three_kinds_of_403() {
    let h = start().await;
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    h.svc.set_auth(auth.clone());
    // data made with the key, which strict mode leaves alone
    h.db("db1").await;
    h.coll("db1", "c1", "/pk").await;
    assert_eq!(
        h.json("POST", "dbs/db1/colls/c1/docs", &[("x-ms-documentdb-partitionkey", "[\"a\"]")], &json!({"id": "i1", "pk": "a"}))
            .await
            .status,
        201
    );
    give(&auth, "main", CONTRIBUTOR, "/");
    give(&auth, "reader", READER, "/");
    let pk = ("x-ms-documentdb-partitionkey", "[\"a\"]");

    // 5301: no role, metadata
    for (p, any) in [("/", false), ("/dbs", true), ("/dbs/db1", false), ("/dbs/db1/colls/c1", false), ("/dbs/db1/colls", false)] {
        let r = as_who(&h, "norole", "GET", p, &[], "").await;
        assert_eq!((r.status, sub(&r).as_str()), (403, "5301"), "{p}: {}", r.text());
        let m = r.json()["message"].as_str().unwrap().to_string();
        assert!(m.starts_with("Request blocked by Auth acct1 : "), "{m}");
        assert!(m.contains("principal [norole] does not have required RBAC permissions to perform action [Microsoft.DocumentDB/databaseAccounts/readMetadata]"), "{m}");
        assert_eq!(m.contains("on any scope"), any, "{m}");
        assert!(m.contains("Learn more: https://aka.ms/cosmos-native-rbac.\r\nActivityId: "), "{m}");
    }
    // 5302: no role, an item (also a missing one: 403, not 404), a query, the change feed, a stored procedure
    let r = as_who(&h, "norole", "GET", "dbs/db1/colls/c1/docs/missing", &[pk], "").await;
    assert_eq!((r.status, sub(&r).as_str()), (403, "5302"));
    let m = r.json()["message"].as_str().unwrap().to_string();
    assert!(m.starts_with("Message: {\"Errors\":[\"Request is blocked because principal [norole] does not have the required RBAC permissions to perform action [Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/items/read] with OperationType [2] and ResourceType [2] on resource [dbs/db1/colls/c1]. Learn more: https://aka.ms/cosmos-native-rbac\"]}\r\nActivityId: "), "{m}");
    assert!(m.contains(", Request URI: /apps/") && m.ends_with("RequestStats: , SDK: Microsoft.Azure.Documents.Common/2.14.0"), "{m}");
    let r = as_who(
        &h,
        "norole",
        "POST",
        "dbs/db1/colls/c1/docs",
        &[("x-ms-documentdb-isquery", "true"), ("content-type", "application/query+json")],
        r#"{"query":"select * from c"}"#,
    )
    .await;
    assert!(r.json()["message"].as_str().unwrap().contains(
        "[Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/executeQuery] with OperationType [15] and ResourceType [2]"
    ));
    let r = as_who(&h, "norole", "POST", "dbs/db1/colls/c1/sprocs/p1", &[pk], "[]").await;
    assert!(r.json()["message"]
        .as_str()
        .unwrap()
        .contains("executeStoredProcedure] with OperationType [9] and ResourceType [109] on resource [dbs/db1/colls/c1/sprocs/p1]"));

    // the reader: reads and queries; writes are 5302
    for (m, p) in [("GET", "/"), ("GET", "/dbs"), ("GET", "/dbs/db1"), ("GET", "/dbs/db1/colls/c1"), ("GET", "/dbs/db1/colls/c1/docs/i1")] {
        let r = as_who(&h, "reader", m, p, &[pk], "").await;
        assert_eq!(r.status, 200, "{m} {p}: {}", r.text());
    }
    assert_eq!(as_who(&h, "reader", "GET", "dbs/db1/colls/c1/docs/nothere", &[pk], "").await.status, 404);
    let q = as_who(
        &h,
        "reader",
        "POST",
        "dbs/db1/colls/c1/docs",
        &[
            ("x-ms-documentdb-isquery", "true"),
            ("x-ms-documentdb-query-enablecrosspartition", "true"),
            ("content-type", "application/query+json"),
        ],
        r#"{"query":"select * from c"}"#,
    )
    .await;
    assert_eq!(q.status, 200, "{}", q.text());
    for (m, p, b, action) in [
        ("POST", "dbs/db1/colls/c1/docs", r#"{"id":"n","pk":"a"}"#, "items/create] with OperationType [0]"),
        ("PUT", "dbs/db1/colls/c1/docs/i1", r#"{"id":"i1","pk":"a"}"#, "items/replace] with OperationType [5]"),
        ("DELETE", "dbs/db1/colls/c1/docs/i1", "", "items/delete] with OperationType [4]"),
    ] {
        let r = as_who(&h, "reader", m, p, &[pk], b).await;
        assert_eq!((r.status, sub(&r).as_str()), (403, "5302"), "{m} {p}");
        assert!(r.json()["message"].as_str().unwrap().contains(action), "{}", r.text());
    }
    let r =
        as_who(&h, "reader", "POST", "dbs/db1/colls/c1/docs", &[pk, ("x-ms-documentdb-is-upsert", "true")], r#"{"id":"n","pk":"a"}"#).await;
    assert!(r.json()["message"].as_str().unwrap().contains("items/upsert] with OperationType [20]"));

    // 5300: management is refused for everybody, a contributor too
    for who in ["main", "reader", "norole"] {
        for (m, p) in [
            ("POST", "dbs"),
            ("DELETE", "dbs/db1"),
            ("POST", "dbs/db1/colls"),
            ("DELETE", "dbs/db1/colls/c1"),
            ("GET", "dbs/db1/colls/c1/sprocs"),
            ("GET", "offers"),
            ("GET", "dbs/db1/users"),
        ] {
            let r = as_who(&h, who, m, p, &[], r#"{"id":"x"}"#).await;
            assert_eq!((r.status, sub(&r).as_str()), (403, "5300"), "{who} {m} {p}");
            assert!(r.json()["message"].as_str().unwrap().starts_with(&format!("Request blocked by Auth acct1 : The given request [{m} /{p}] cannot be authorized by AAD token in data plane. Learn more: https://aka.ms/cosmos-native-rbac.\r\nActivityId: ")));
        }
    }
    assert_eq!(h.json("GET", "dbs/db1", &[], &json!({})).await.status, 200);

    // the contributor writes
    let r = as_who(&h, "main", "POST", "dbs/db1/colls/c1/docs", &[pk], r#"{"id":"n","pk":"a"}"#).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(as_who(&h, "main", "PUT", "dbs/db1/colls/c1/docs/n", &[pk], r#"{"id":"n","pk":"a","v":2}"#).await.status, 200);
    assert_eq!(as_who(&h, "main", "DELETE", "dbs/db1/colls/c1/docs/n", &[pk], "").await.status, 204);
}

#[tokio::test]
async fn a_role_on_one_database_and_the_token_problems() {
    let h = start().await;
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    h.svc.set_auth(auth.clone());
    h.db("db1").await;
    h.db("db2").await;
    h.coll("db1", "c1", "/pk").await;
    h.coll("db2", "c1", "/pk").await;
    give(&auth, "scoped", READER, "/dbs/db1");
    give(&auth, "scoped", "00000000-0000-0000-0000-000000000001", "/dbs/db1");
    // the account read needs the role at the account
    assert_eq!(as_who(&h, "scoped", "GET", "/", &[], "").await.status, 403);
    assert_eq!(
        as_who(&h, "scoped", "GET", "/dbs/db1/colls/c1/docs/x", &[("x-ms-documentdb-partitionkey", "[\"a\"]")], "").await.status,
        404
    );
    assert_eq!(
        as_who(&h, "scoped", "GET", "/dbs/db2/colls/c1/docs/x", &[("x-ms-documentdb-partitionkey", "[\"a\"]")], "").await.status,
        403
    );
    // a token that is wrong
    let bad = |t: String| {
        let enc = format!("type%3Daad%26ver%3D1.0%26sig%3D{t}");
        let h = &h;
        async move {
            h.srv
                .send(
                    "GET",
                    &format!("{}/", h.base),
                    &[("authorization", &enc), ("x-ms-date", "Wed, 11 Nov 2026 12:00:00 GMT"), ("x-ms-version", "2020-07-15")],
                    "",
                )
                .await
        }
    };
    let r = bad(TestAuthority::token("main", "https://vault.azure.net")).await;
    assert_eq!((r.status, sub(&r).as_str()), (401, "5008"));
    assert!(r.json()["message"].as_str().unwrap().starts_with("Request blocked by Auth acct1 : Provided AAD token has an invalid audience. This database account accepts tokens intended for [https://acct1.documents.azure.com, https://acct1.sql.cosmos.azure.com, https://cosmos.azure.com,"));
    let r = bad(format!("{}x", TestAuthority::token("main", "https://cosmos.azure.com"))).await;
    assert_eq!((r.status, sub(&r).as_str()), (401, "5004"));
    let r = bad("garbage".into()).await;
    assert_eq!((r.status, sub(&r).as_str()), (401, "5003"));
    assert!(r.json()["message"].as_str().unwrap().contains("IDX14100: JWT is not well formed, there are no dots (.)."));
}
