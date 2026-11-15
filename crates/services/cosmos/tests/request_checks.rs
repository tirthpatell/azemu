//! What the service checks on every request and how its errors are worded, as a real account answered in November 2026:
//! the API version, the consistency level, the session token, the partition key header, a replace with another id,
//! the shapes of the error bodies and the messages of patch.

mod common;

use common::{start, H};
use serde_json::{json, Value};

const PK: &str = "x-ms-documentdb-partitionkey";

async fn setup() -> H {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h
}

fn message(r: &azemu_core::testing::TestResponse) -> String {
    r.json()["message"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn an_api_version_that_is_missing_or_not_a_date_is_a_400() {
    let h = setup().await;
    let url = format!("{}/dbs/d", h.base);
    let auth = ("authorization", "type%3Dmaster%26ver%3D1.0%26sig%3Dxyz");
    for version in ["2000-01-01", "abc", "", "1.0", "2020-7-15"] {
        let r = h.srv.send("GET", &url, &[auth, ("x-ms-version", version)], "").await;
        assert_eq!(r.status, 400, "{version:?}");
        assert!(message(&r).starts_with("Invalid API version. Ensure a valid x-ms-version header value is passed."), "{}", r.text());
    }
    for version in ["2018-12-31", "2020-07-15", "2020-11-05"] {
        assert_eq!(h.srv.send("GET", &url, &[auth, ("x-ms-version", version)], "").await.status, 200, "{version}");
    }
    // no x-ms-version at all
    let r = h.srv.send("GET", &url, &[auth], "").await;
    assert_eq!(r.status, 400, "{}", r.text());
}

#[tokio::test]
async fn the_consistency_level_may_not_be_stronger_than_the_account_s() {
    let h = setup().await;
    for level in ["Strong", "BoundedStaleness"] {
        let r = h.req("GET", "dbs/d", &[("x-ms-consistency-level", level)], "").await;
        assert_eq!(r.status, 400, "{level}");
        assert!(message(&r).starts_with(&format!(
            "ConsistencyLevel {level} specified in the request is invalid when service is configured with consistency level Session."
        )));
    }
    for level in ["bogus", "session", "STRONG"] {
        let r = h.req("GET", "dbs/d", &[("x-ms-consistency-level", level)], "").await;
        assert_eq!(r.status, 400, "{level}");
        assert!(message(&r).starts_with(&format!("Value '{level}' specified for the header 'x-ms-consistency-level' is invalid.")));
    }
    for level in ["Session", "ConsistentPrefix", "Eventual", ""] {
        assert_eq!(h.req("GET", "dbs/d", &[("x-ms-consistency-level", level)], "").await.status, 200, "{level:?}");
    }
}

#[tokio::test]
async fn session_tokens_are_checked_on_documents() {
    let h = setup().await;
    let made = h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p"})).await;
    let own = made.header("x-ms-session-token").unwrap().to_string();
    assert!(own.starts_with("0:-1#"), "{own}");
    let pk = format!("[{}]", json!("p"));
    for token in ["garbage", "0", "-1", "0:-1#x", "0:x#5"] {
        let r = h.req("GET", "dbs/d/colls/c/docs/a", &[(PK, &pk), ("x-ms-session-token", token)], "").await;
        assert_eq!(r.status, 400, "{token}: {}", r.text());
        assert!(message(&r).contains("The session token provided"), "{}", r.text());
        // a container does not look at it
        assert_eq!(h.req("GET", "dbs/d/colls/c", &[("x-ms-session-token", token)], "").await.status, 200);
    }
    let ahead = [(PK, pk.as_str()), ("x-ms-session-token", "0:-1#9999999")];
    let r = h.req("GET", "dbs/d/colls/c/docs/a", &ahead, "").await;
    assert_eq!(r.status, 404, "{}", r.text());
    assert_eq!(r.header("x-ms-substatus"), Some("1002"));
    assert!(message(&r).starts_with("The read session is not available for the input session token."));
    let r = h.query("d", "c", &ahead, json!({"query": "SELECT * FROM c"})).await;
    assert_eq!((r.status, r.header("x-ms-substatus")), (404, Some("1002")));
    // the token the service gave, another range, an old one and a write are fine
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/a", &[(PK, &pk), ("x-ms-session-token", &own)], "").await.status, 200);
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/a", &[(PK, &pk), ("x-ms-session-token", "1:-1#9999999")], "").await.status, 200);
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "b", "pk": "p"})).await.status, 201);
    let r = h.json("POST", "dbs/d/colls/c/docs", &ahead, &json!({"id": "w", "pk": "p"})).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

#[tokio::test]
async fn a_replace_with_another_id_renames_the_document() {
    let h = setup().await;
    let created = h.doc("d", "c", &json!("a"), &json!({"id": "p1", "pk": "a", "v": 1})).await.json();
    h.doc("d", "c", &json!("a"), &json!({"id": "q1", "pk": "a"})).await;
    let pk = r#"["a"]"#;
    let r = h.json("PUT", "dbs/d/colls/c/docs/p1", &[(PK, pk)], &json!({"id": "q1", "pk": "a", "v": 2})).await;
    assert_eq!(r.status, 409, "{}", r.text());
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/p1", &[(PK, pk)], "").await.json()["v"], 1);
    let r = h.json("PUT", "dbs/d/colls/c/docs/p1", &[(PK, pk)], &json!({"id": "other", "pk": "a", "v": 2})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["id"], "other");
    assert_eq!(r.json()["_rid"], created["_rid"], "the document keeps its resource id");
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/p1", &[(PK, pk)], "").await.status, 404);
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/other", &[(PK, pk)], "").await.json()["v"], 2);
    // by resource id, with no partition key header, the same
    let rid = created["_rid"].as_str().unwrap();
    let r = h.json("PUT", &format!("dbs/d/colls/c/docs/{rid}"), &[(PK, pk)], &json!({"id": "again", "pk": "a"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/again", &[(PK, pk)], "").await.status, 200);
    assert_eq!(h.json("PUT", "dbs/d/colls/c/docs/nope", &[(PK, pk)], &json!({"id": "nope2", "pk": "a"})).await.status, 404);
    let r = h.json("PUT", "dbs/d/colls/c/docs/again", &[(PK, pk)], &json!({"id": "again", "pk": "b"})).await;
    assert_eq!(r.status, 400);
    assert!(message(&r).contains("doesn't match the one specified in the header"), "{}", r.text());
    // the change feed sees the new document
    let r = h.req("GET", "dbs/d/colls/c/docs", &[("A-IM", "Incremental Feed")], "").await;
    let ids: Vec<String> = r.json()["Documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    assert!(ids.iter().any(|i| i == "again") && !ids.iter().any(|i| i == "p1"), "{ids:?}");
}

#[tokio::test]
async fn item_errors_are_not_wrapped_and_resource_errors_are() {
    let h = setup().await;
    h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p"})).await;
    let pk = r#"["p"]"#;
    let conflict = h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p"})).await;
    let missing = h.req("GET", "dbs/d/colls/c/docs/nope", &[(PK, pk)], "").await;
    let stale = h.json("PUT", "dbs/d/colls/c/docs/a", &[(PK, pk), ("If-Match", "\"stale\"")], &json!({"id": "a", "pk": "p"})).await;
    for (r, status, text) in [
        (conflict, 409, "Entity with the specified id already exists in the system."),
        (missing, 404, "Entity with the specified id does not exist in the system. More info: https://aka.ms/cosmosdb-tsg-not-found"),
        (stale, 412, "Operation cannot be performed because one of the specified precondition is not met."),
    ] {
        assert_eq!(r.status, status);
        let m = message(&r);
        assert!(m.starts_with(text) && m.contains(", \r\nRequestStartTime: ") && m.contains("Number of regions attempted:1"), "{m}");
        assert!(!m.contains("Message:"));
    }
    let r = h.json("POST", "dbs", &[], &json!({"id": "d"})).await;
    assert_eq!(r.status, 409);
    assert!(
        message(&r).starts_with("Message: {\"Errors\":[\"Resource with specified id, name, or unique index already exists.\"]}"),
        "{}",
        r.text()
    );
    // not found: databases and containers keep the wrapped text
    let r = h.req("GET", "dbs/nope", &[], "").await;
    assert!(message(&r).starts_with("Message: {\"Errors\":[\"Resource Not Found."), "{}", r.text());
}

#[tokio::test]
async fn the_partition_key_header_errors_have_azure_s_texts() {
    let h = setup().await;
    let wrong = "PartitionKey extracted from document doesn't match the one specified in the header. Learn more: https://aka.ms/CosmosDB/sql/errors/wrong-pk-value";
    let r = h.json("POST", "dbs/d/colls/c/docs", &[(PK, r#"["b"]"#)], &json!({"id": "1", "pk": "a"})).await;
    assert_eq!((r.status, r.header("x-ms-substatus")), (400, Some("1001")));
    assert!(message(&r).contains(wrong), "{}", r.text());
    let r = h.req("GET", "dbs/d/colls/c/docs/1", &[], "").await;
    assert_eq!((r.status, r.header("x-ms-substatus")), (400, Some("1001")));
    assert!(message(&r).contains("has fewer components than defined in the the collection."), "{}", r.text());
    let r = h.req("GET", "dbs/d/colls/c/docs/1", &[(PK, r#"["a","b"]"#)], "").await;
    assert_eq!(r.status, 400);
    assert!(message(&r).starts_with("Partition key provided either doesn't correspond to definition in the collection"), "{}", r.text());
    let r = h.req("GET", "dbs/d/colls/c/docs/1", &[(PK, "abc")], "").await;
    assert!(message(&r).starts_with("Partition key abc is invalid."), "{}", r.text());
}

#[tokio::test]
async fn names_and_bodies_are_refused_with_azure_s_words() {
    let h = setup().await;
    let r = h.json("POST", "dbs/d/colls/c/docs", &[(PK, r#"["p"]"#)], &json!({"pk": "p"})).await;
    assert!(
        message(&r).contains("the required properties - 'id; ' - are missing. Learn more: https://aka.ms/CosmosDB/sql/errors/missing-id"),
        "{}",
        r.text()
    );
    let r = h.json("POST", "dbs", &[], &json!({"id": "x".repeat(255)})).await;
    assert_eq!(r.status, 400);
    assert!(
        message(&r).contains("less than '255' characters.\",\"The request payload is invalid. Ensure to provide a valid request payload."),
        "{}",
        r.text()
    );
    let r = h.json("POST", "dbs", &[], &json!({})).await;
    assert!(message(&r).contains("'id; ' - are missing"), "{}", r.text());
    let r = h.req("POST", "dbs/d/colls/c/docs", &[("content-type", "application/json"), (PK, r#"["p"]"#)], "not json").await;
    assert!(message(&r).contains("The request payload is invalid. Ensure to provide a valid request payload."), "{}", r.text());
    let r = h.req("POST", "dbs/d/colls/c/docs", &[("content-type", "application/json"), (PK, r#"["p"]"#)], "[1]").await;
    assert!(message(&r).contains("One of the specified inputs is invalid"), "{}", r.text());
    let r = h.json("POST", "dbs/d/users/u1/permissions", &[], &json!({"id": "p"})).await;
    assert_eq!(r.status, 404);
    h.json("POST", "dbs/d/users", &[], &json!({"id": "u1"})).await;
    let r = h
        .json("POST", "dbs/d/users/u1/permissions", &[], &json!({"id": "p", "resource": "dbs/d/colls/c", "permissionMode": "write"}))
        .await;
    assert!(
        message(&r).contains("The input Permission mode 'write' is invalid. Ensure to provide a valid permission mode."),
        "{}",
        r.text()
    );
    let t = json!({"id": "t", "body": "function(){}", "triggerType": "middle", "triggerOperation": "All"});
    let r = h.json("POST", "dbs/d/colls/c/triggers", &[], &t).await;
    assert!(message(&r).contains("The input trigger type 'middle' is invalid. Ensure to provide a valid trigger type."), "{}", r.text());
    let r = h.json("POST", "dbs/d/colls/c/triggers", &[], &json!({"id": "t", "body": "function(){}"})).await;
    assert!(message(&r).contains("'triggerOperation; triggerType; ' - are missing"), "{}", r.text());
}

#[tokio::test]
async fn permissions_keep_the_case_and_a_second_one_on_a_resource_is_a_409() {
    let h = setup().await;
    h.json("POST", "dbs/d/users", &[], &json!({"id": "u1"})).await;
    let p = |id: &str, mode: &str| json!({"id": id, "permissionMode": mode, "resource": "dbs/d/colls/c"});
    let r = h.json("POST", "dbs/d/users/u1/permissions", &[], &p("p1", "Read")).await;
    assert_eq!((r.status, r.json()["permissionMode"].clone()), (201, json!("Read")));
    let r = h.json("POST", "dbs/d/users/u1/permissions", &[], &p("p2", "All")).await;
    assert_eq!(r.status, 409, "{}", r.text());
    let r = h.json("PUT", "dbs/d/users/u1/permissions/p1", &[], &p("p1", "all")).await;
    assert_eq!((r.status, r.json()["permissionMode"].clone()), (200, json!("all")));
    let t = json!({"id": "t", "body": "function(){}", "triggerType": "Pre", "triggerOperation": "all"});
    let r = h.json("POST", "dbs/d/colls/c/triggers", &[], &t).await;
    assert_eq!((r.json()["triggerType"].clone(), r.json()["triggerOperation"].clone()), (json!("Pre"), json!("all")));
}

async fn patch(h: &H, ops: Value, cond: Option<&str>) -> azemu_core::testing::TestResponse {
    let mut body = json!({ "operations": ops });
    if let Some(c) = cond {
        body["condition"] = json!(c);
    }
    h.json("PATCH", "dbs/d/colls/c/docs/d1", &[(PK, r#"["a"]"#)], &body).await
}

#[tokio::test]
async fn patch_answers_in_azure_s_words() {
    let h = setup().await;
    h.doc("d", "c", &json!("a"), &json!({"id": "d1", "pk": "a", "v": 1, "s": "x", "arr": [1, 2]})).await;
    let learn = "Learn more: https://aka.ms/cosmosdbpatchdocs";
    let cases: Vec<(Value, String)> = vec![
        (json!([{"op": "set", "path": "/id", "value": "x"}]), "Invalid patch request: Cannot patch system property 'id'.".into()),
        (json!([{"op": "set", "path": "/_etag", "value": "x"}]), "Invalid patch request: Cannot patch system property '_etag'.".into()),
        (json!([{"op": "bogus", "path": "/s", "value": 1}]), "Invalid value of the patch operation 'op' property in patch request: 'bogus'".into()),
        (json!([{"op": "set", "value": 1}]), "The required property 'path' could not be found in patch request.".into()),
        (json!([{"op": "set", "path": "/s"}]), "The required property 'value' could not be found in patch request.".into()),
        (json!([{"op": "replace", "path": "/nothere", "value": 1}]), format!("For Operation(1): Node('nothere') to be replaced is absent. {learn}")),
        (json!([{"op": "remove", "path": "/nothere"}]), format!("For Operation(1): Path(from 'nothere') on which patch operation has to be applied is absent. {learn}")),
        (json!([{"op": "incr", "path": "/s", "value": 1}]), format!("For Operation(1): Node('s') is not a number. {learn}")),
        (json!([{"op": "incr", "path": "/v", "value": "x"}]), format!("For Operation(1): The Increment patch operation requested has an unsupported data type. {learn}")),
        (json!([{"op": "add", "path": "/a/b", "value": 1}]), format!("For Operation(1): Add Operation can only create a child object of an existing node(array or object) and cannot create path recursively, no path found beyond: 'a'. {learn}")),
        (json!([{"op": "set", "path": "/a/b", "value": 1}]), format!("For Operation(1): Given Operation can only create a child object of an existing node(array or object) and cannot create path recursively, no path found beyond: 'a'. {learn}")),
        (json!([{"op": "add", "path": "/arr/5", "value": 1}]), "For Operation(1): Index('5') to operate on is out of array bounds. Learn More: https://aka.ms/cosmosdbpatchdocs".into()),
        (json!([{"op": "set", "path": "/v", "value": 2}, {"op": "remove", "path": "/x"}]), format!("For Operation(2): Path(from 'x') on which patch operation has to be applied is absent. {learn}")),
        (json!([{"op": "set", "path": "s", "value": 1}]), "One of the specified inputs is invalid".into()),
        (Value::Array((0..11).map(|i| json!({"op": "set", "path": format!("/z{i}"), "value": i})).collect()), "The number of patch operations cannot exceed '10'.".into()),
    ];
    for (ops, text) in cases {
        let r = patch(&h, ops.clone(), None).await;
        assert_eq!(r.status, 400, "{ops}: {}", r.text());
        assert!(message(&r).contains(&text), "{ops}: {}", r.text());
    }
    // the partition key is a mismatch, as it is a property of the document and not a name
    let r = patch(&h, json!([{"op": "set", "path": "/pk", "value": "b"}]), None).await;
    assert_eq!((r.status, r.header("x-ms-substatus")), (400, Some("1001")));
    // no operations: 304 with no body
    let r = patch(&h, json!([]), None).await;
    assert_eq!(r.status, 304);
    assert!(r.body.is_empty());
    let r = h.json("PATCH", "dbs/d/colls/c/docs/d1", &[(PK, r#"["a"]"#)], &json!({})).await;
    assert!(message(&r).contains("The required property 'operations' could not be found in patch request."), "{}", r.text());
    // conditions
    let r = patch(&h, json!([{"op": "set", "path": "/s", "value": "y"}]), Some("from c where c.v = 99")).await;
    assert_eq!((r.status, r.header("x-ms-substatus")), (412, Some("1110")));
    let r = patch(&h, json!([{"op": "set", "path": "/s", "value": "y"}]), Some("nonsense")).await;
    assert_eq!(r.status, 400);
    let wrapped: Value = serde_json::from_str(message(&r).split("\r\nActivityId").next().unwrap().trim_start_matches("Message: ")).unwrap();
    let inner: Value = serde_json::from_str(wrapped["Errors"][0].as_str().unwrap()).unwrap();
    assert_eq!(inner["errors"][0]["code"], 1001);
    assert!(inner["errors"][0]["message"].as_str().unwrap().starts_with("Syntax error, incorrect syntax near 'nonsense'"), "{inner}");
    // `/` is a path (the empty name)
    let r = patch(&h, json!([{"op": "set", "path": "/", "value": 1}]), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()[""], 1);
}
