mod common;

use common::start;
use serde_json::json;

#[tokio::test]
async fn account_document_points_back_at_the_host() {
    let h = start().await;
    let r = h.req("GET", "", &[], "").await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["id"], "acct1");
    assert_eq!(v["_dbs"], "//dbs/");
    assert_eq!(v["userConsistencyPolicy"]["defaultConsistencyLevel"], "Session");
    let ep = v["writableLocations"][0]["databaseAccountEndpoint"].as_str().unwrap().to_string();
    assert!(ep.starts_with(&h.base), "{ep}");
    assert_eq!(v["readableLocations"][0]["databaseAccountEndpoint"], ep.as_str());
    assert!(v["queryEngineConfiguration"].is_string());
    assert!(r.header("x-ms-activity-id").is_some());
}

#[tokio::test]
async fn missing_authorization_is_401() {
    let h = start().await;
    let r = h.srv.send("GET", &format!("{}/", h.base), &[], "").await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["code"], "Unauthorized");
}

#[tokio::test]
async fn unbuilt_operation_is_501_in_cosmos_form() {
    let h = start().await;
    h.db("d").await;
    let r = h.req("GET", "dbs/d/colls/x/docs/y/attachments", &[], "").await;
    assert_eq!(r.status, 404); // container x does not exist
    let r = h.req("PATCH", "dbs/d", &[], "").await;
    assert_eq!(r.status, 501);
    assert_eq!(r.json()["code"], "NotImplemented");
    assert!(r.json()["message"].as_str().unwrap().contains("PATCH /dbs/d"));
}

/// Direct mode (`GET //addresses/`) is not built: the 501 tells the user to use gateway mode, and the operation is named
/// for `GET /_azemu/unsupported`.
#[tokio::test]
async fn direct_mode_address_lookup_is_501_and_says_to_use_gateway_mode() {
    let h = start().await;
    for path in ["/addresses/", "//addresses/", "addresses?$resolveFor=dbs%2Fd%2Fcolls%2Fc%2Fdocs&$filter=protocol%20eq%20rntbd"] {
        let r = h.req("GET", path, &[], "").await;
        assert_eq!(r.status, 501, "{path}: {}", r.text());
        let msg = r.json()["message"].as_str().unwrap().to_string();
        assert!(msg.contains("direct mode") && msg.contains("ConnectionMode.Gateway") && msg.contains("gateway"), "{msg}");
    }
    let u = h.srv.send("GET", &format!("{}/_azemu/unsupported", h.srv.gateway_url()), &[], "").await;
    assert_eq!(u.status, 200);
    assert!(u.text().contains("GET /addresses (direct mode)"), "{}", u.text());
}

#[tokio::test]
async fn database_and_container_by_name_and_rid() {
    let h = start().await;
    let db = h.db("mydb").await;
    assert_eq!(db["_self"], format!("dbs/{}/", db["_rid"].as_str().unwrap()));
    assert!(db["_etag"].as_str().unwrap().starts_with('"'));
    let c = h.coll("mydb", "mycoll", "/pk").await;
    assert_eq!(c["partitionKey"]["paths"][0], "/pk");
    assert_eq!(c["indexingPolicy"]["indexingMode"], "consistent");
    assert_eq!(c["conflictResolutionPolicy"]["mode"], "LastWriterWins");
    let db_rid = db["_rid"].as_str().unwrap();
    let c_rid = c["_rid"].as_str().unwrap();
    // rid based addressing
    let r = h.req("GET", &format!("dbs/{db_rid}/colls/{c_rid}"), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["id"], "mycoll");
    // rids are base64 of 4 and 8 bytes
    assert_eq!(db_rid.len(), 8);
    assert_eq!(c_rid.len(), 12);
    // duplicate and missing
    let r = h.json("POST", "dbs", &[], &json!({"id": "mydb"})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json()["code"], "Conflict");
    let r = h.req("GET", "dbs/nope", &[], "").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["code"], "NotFound");
    assert!(r.header("x-ms-activity-id").is_some());
    // delete
    assert_eq!(h.req("DELETE", "dbs/mydb/colls/mycoll", &[], "").await.status, 204);
    assert_eq!(h.req("GET", "dbs/mydb/colls/mycoll", &[], "").await.status, 404);
}

#[tokio::test]
async fn document_crud_by_name_and_by_rid() {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    let r = h.doc("d", "c", &json!("x"), &json!({"id": "a", "pk": "x", "z": 1, "a": 2})).await;
    assert_eq!(r.status, 201);
    let created = r.json();
    let keys: Vec<&String> = created.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["id", "pk", "z", "a", "_rid", "_self", "_etag", "_attachments", "_ts"]);
    assert_eq!(r.header("etag"), created["_etag"].as_str());
    assert!(r.header("x-ms-session-token").unwrap().starts_with("0:"));

    let pk = [("x-ms-documentdb-partitionkey", "[\"x\"]")];
    let r = h.req("GET", "dbs/d/colls/c/docs/a", &pk, "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["z"], 1);
    // by rid, no partition key needed
    let rid = created["_rid"].as_str().unwrap();
    let r = h.req("GET", &format!("dbs/d/colls/c/docs/{rid}"), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["id"], "a");

    // duplicate -> 409, upsert -> 200, replace, stale etag -> 412
    let r = h.doc("d", "c", &json!("x"), &json!({"id": "a", "pk": "x"})).await;
    assert_eq!(r.status, 409);
    let r =
        h.json("POST", "dbs/d/colls/c/docs", &[pk[0], ("x-ms-documentdb-is-upsert", "True")], &json!({"id": "a", "pk": "x", "v": 2})).await;
    assert_eq!(r.status, 200);
    let r = h.json("PUT", "dbs/d/colls/c/docs/a", &[pk[0], ("if-match", "\"nope\"")], &json!({"id": "a", "pk": "x"})).await;
    assert_eq!(r.status, 412);
    let r = h.req("GET", "dbs/d/colls/c/docs/a", &[pk[0], ("if-none-match", r.header("etag").unwrap_or("x"))], "").await;
    assert_eq!(r.status, 200);
    let etag = h.req("GET", "dbs/d/colls/c/docs/a", &pk, "").await;
    let tag = etag.header("etag").unwrap().to_string();
    let r = h.req("GET", "dbs/d/colls/c/docs/a", &[pk[0], ("if-none-match", &tag)], "").await;
    assert_eq!(r.status, 304);
    // header / body partition key mismatch
    let r = h.doc("d", "c", &json!("other"), &json!({"id": "b", "pk": "x"})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.header("x-ms-substatus"), Some("1001"));
    // delete
    assert_eq!(h.req("DELETE", "dbs/d/colls/c/docs/a", &pk, "").await.status, 204);
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/a", &pk, "").await.status, 404);
}

#[tokio::test]
async fn document_over_two_megabytes_is_413_and_unique_keys_are_enforced() {
    let h = start().await;
    h.db("d").await;
    h.json(
        "POST",
        "dbs/d/colls",
        &[],
        &json!({"id": "u", "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2},
                "uniqueKeyPolicy": {"uniqueKeys": [{"paths": ["/email"]}]}}),
    )
    .await;
    let big = "a".repeat(2_200_000);
    let r = h.doc("d", "u", &json!("x"), &json!({"id": "big", "pk": "x", "b": big})).await;
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["code"], "RequestEntityTooLarge");
    assert_eq!(h.doc("d", "u", &json!("x"), &json!({"id": "1", "pk": "x", "email": "e"})).await.status, 201);
    assert_eq!(h.doc("d", "u", &json!("x"), &json!({"id": "2", "pk": "x", "email": "e"})).await.status, 409);
    assert_eq!(h.doc("d", "u", &json!("y"), &json!({"id": "2", "pk": "y", "email": "e"})).await.status, 201);
}

#[tokio::test]
async fn offers_manual_and_autoscale() {
    let h = start().await;
    let r = h.json("POST", "dbs", &[("x-ms-offer-throughput", "1000")], &json!({"id": "d"})).await;
    assert_eq!(r.status, 201);
    let db = r.json();
    let q = json!({"query": "SELECT * FROM root r WHERE r.resource=@l", "parameters": [{"name": "@l", "value": db["_self"]}]});
    let r =
        h.req("POST", "offers", &[("content-type", "application/query+json"), ("x-ms-documentdb-isquery", "True")], &q.to_string()).await;
    assert_eq!(r.status, 200);
    let offer = r.json()["Offers"][0].clone();
    assert_eq!(offer["content"]["offerThroughput"], 1000);
    let mut updated = offer.clone();
    updated["content"]["offerThroughput"] = json!(2000);
    let r = h.json("PUT", &format!("offers/{}", offer["_rid"].as_str().unwrap()), &[], &updated).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["content"]["offerThroughput"], 2000);
    // autoscale on a container; too small is a 400
    let auto = json!({"maxThroughput": 5000, "autoUpgradePolicy": {"throughputPolicy": {"incrementPercent": 3}}}).to_string();
    let body = json!({"id": "c", "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}});
    let r = h.json("POST", "dbs/d/colls", &[("x-ms-cosmos-offer-autopilot-settings", &auto)], &body).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let bad = json!({"maxThroughput": -200}).to_string();
    let body2 = json!({"id": "c2", "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}});
    let r = h.json("POST", "dbs/d/colls", &[("x-ms-cosmos-offer-autopilot-settings", &bad)], &body2).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("Requested throughput -200 is less than required minimum throughput 1000"));
}

#[tokio::test]
async fn scripts_users_and_sproc_execution() {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    let r = h.json("POST", "dbs/d/colls/c/sprocs", &[], &json!({"id": "sp", "body": "function(){}"})).await;
    assert_eq!(r.status, 201);
    let r = h.json("POST", "dbs/d/colls/c/sprocs", &[], &json!({"id": "sp", "body": "function(){}"})).await;
    assert_eq!(r.status, 409);
    // executing needs the partition key (scripts are scoped to one logical partition)
    let r = h.json("POST", "dbs/d/colls/c/sprocs/sp", &[], &json!([])).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["message"].as_str().unwrap().contains("PartitionKey value must be supplied"));
    let r = h.json("POST", "dbs/d/colls/c/sprocs/sp", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], &json!([])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = h.json("POST", "dbs/d/users", &[], &json!({"id": "u1"})).await;
    assert_eq!(r.status, 201);
    let p = json!({"id": "p1", "permissionMode": "read", "resource": "dbs/d/colls/c"});
    let r = h.json("POST", "dbs/d/users/u1/permissions", &[], &p).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["permissionMode"], "Read");
    assert!(r.json()["_token"].is_string());
    let r = h.req("GET", "dbs/d/users/u1/permissions", &[], "").await;
    assert_eq!(r.json()["_count"], 1);
}

#[tokio::test]
async fn binary_json_bodies_are_refused_with_a_message_naming_the_switch() {
    // The Rust SDK sends item bodies as Cosmos binary JSON (first byte 0x80) and advertises it with
    // `x-ms-cosmos-supported-serialization-formats: CosmosBinary`. azemu does not read that format.
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    let url = format!("{}/dbs/d/colls/c/docs", h.base);
    let auth = "type%3Dmaster%26ver%3D1.0%26sig%3Dxyz";
    let binary_body: Vec<u8> = vec![0x80, 0xE0, 0x1B, 0x02, 0x69, 0x64, 0x03, 0x31];
    let headers = [
        ("authorization", auth),
        ("x-ms-version", "2020-07-15"),
        ("content-type", "application/json"),
        ("x-ms-documentdb-partitionkey", "[\"p\"]"),
        ("x-ms-cosmos-supported-serialization-formats", "CosmosBinary"),
    ];
    let r = h.srv.send("POST", &url, &headers, binary_body).await;
    assert_eq!(r.status, 400);
    let v = r.json();
    assert_eq!(v["code"], "BadRequest");
    let m = v["message"].as_str().unwrap();
    assert!(m.contains("binary"), "{m}");
    assert!(m.contains("AZURE_COSMOS_BINARY_ENCODING_ENABLED=false"), "{m}");
    // nothing was stored
    let r = h.req("GET", "dbs/d/colls/c/docs/x", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], "").await;
    assert_eq!(r.status, 404);
    // text JSON bodies are still read, and the answers are text JSON even when the client says it can read binary
    let ok_headers = [headers[0], headers[1], headers[2], headers[3], headers[4]];
    let r = h.srv.send("POST", &url, &ok_headers, br#"{"id":"x","pk":"p","v":1}"#.to_vec()).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.body.first(), Some(&b'{'));
    assert!(r.header("content-type").unwrap_or("").starts_with("application/json"));
    assert_eq!(r.json()["v"], 1);
    let r = h.srv.send("GET", &format!("{url}/x"), &ok_headers, "").await;
    assert_eq!(r.body.first(), Some(&b'{'));
    let q = [
        headers[0],
        headers[1],
        ("content-type", "application/query+json"),
        ("x-ms-documentdb-isquery", "true"),
        ("x-ms-documentdb-query-enablecrosspartition", "true"),
        headers[4],
    ];
    let r = h.srv.send("POST", &url, &q, br#"{"query":"SELECT * FROM c"}"#.to_vec()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.body.first(), Some(&b'{'));
    // a binary query body or a batch body gets the same answer
    let r = h.srv.send("POST", &url, &q, vec![0x80u8, 0x01]).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["message"].as_str().unwrap().contains("AZURE_COSMOS_BINARY_ENCODING_ENABLED=false"));
}
