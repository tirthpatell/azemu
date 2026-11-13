mod common;

use std::sync::Arc;

use azemu_core::testing::Client;
use azemu_core::Emulator;
use azemu_cosmos::CosmosService;
use common::start;
use serde_json::{json, Value};

const PK: &str = "x-ms-documentdb-partitionkey";

#[tokio::test]
async fn patch_operations_and_condition() {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h.doc("d", "c", &json!("k"), &json!({"id": "a", "pk": "k", "n": 1, "tags": ["x"], "gone": 1, "mv": "m"})).await;
    let ops = json!({"operations": [
        {"op": "add", "path": "/new", "value": "v"},
        {"op": "incr", "path": "/n", "value": 4},
        {"op": "add", "path": "/tags/-", "value": "y"},
        {"op": "remove", "path": "/gone"},
        {"op": "move", "from": "/mv", "path": "/moved"},
    ]});
    let r = h.json("PATCH", "dbs/d/colls/c/docs/a", &[(PK, "[\"k\"]")], &ops).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!((v["n"].clone(), v["tags"].clone(), v["moved"].clone()), (json!(5), json!(["x", "y"]), json!("m")));
    assert!(v.get("gone").is_none() && v.get("mv").is_none());
    // failed condition: 412, patching the key: 400
    let c = json!({"condition": "from c where c.n = 0", "operations": [{"op": "set", "path": "/n", "value": 9}]});
    assert_eq!(h.json("PATCH", "dbs/d/colls/c/docs/a", &[(PK, "[\"k\"]")], &c).await.status, 412);
    let k = json!({"operations": [{"op": "set", "path": "/pk", "value": "z"}]});
    assert_eq!(h.json("PATCH", "dbs/d/colls/c/docs/a", &[(PK, "[\"k\"]")], &k).await.status, 400);
}

#[tokio::test]
async fn batch_is_atomic() {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h.doc("d", "c", &json!("k"), &json!({"id": "dup", "pk": "k"})).await;
    let hdrs = [(PK, "[\"k\"]"), ("x-ms-cosmos-is-batch-request", "True"), ("x-ms-cosmos-batch-atomic", "True")];
    let ops = json!([
        {"operationType": "Create", "resourceBody": {"id": "new", "pk": "k"}},
        {"operationType": "Create", "resourceBody": {"id": "dup", "pk": "k"}},
        {"operationType": "Read", "id": "dup"},
    ]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdrs, &ops).await;
    assert_eq!(r.status, 207);
    let codes: Vec<i64> = r.json().as_array().unwrap().iter().map(|x| x["statusCode"].as_i64().unwrap()).collect();
    assert_eq!(codes, [424, 409, 424]);
    // rolled back: "new" does not exist
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/new", &[(PK, "[\"k\"]")], "").await.status, 404);
    // success
    let ok = json!([
        {"operationType": "Create", "resourceBody": {"id": "new", "pk": "k"}},
        {"operationType": "Read", "id": "new"},
        {"operationType": "Delete", "id": "dup"},
    ]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdrs, &ok).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()[1]["resourceBody"]["id"], "new");
    // limits
    let many: Vec<Value> =
        (0..101).map(|i| json!({"operationType": "Create", "resourceBody": {"id": format!("i{i}"), "pk": "k"}})).collect();
    assert_eq!(h.json("POST", "dbs/d/colls/c/docs", &hdrs, &json!(many)).await.status, 400);
    assert_eq!(h.json("POST", "dbs/d/colls/c/docs", &hdrs, &json!([])).await.status, 400);
}

#[tokio::test]
async fn change_feed_in_modification_order() {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    let feed = |extra: Vec<(&'static str, String)>| {
        let h = &h;
        async move {
            let mut hdrs: Vec<(&str, &str)> = vec![("a-im", "Incremental feed")];
            for (k, v) in &extra {
                hdrs.push((k, v));
            }
            h.req("GET", "dbs/d/colls/c/docs", &hdrs, "").await
        }
    };
    let r = feed(vec![]).await;
    assert_eq!(r.status, 304);
    let first = r.header("etag").unwrap().to_string();
    for (i, p) in [("1", "a"), ("2", "b"), ("3", "a")] {
        h.doc("d", "c", &json!(p), &json!({"id": i, "pk": p})).await;
    }
    h.json("PUT", "dbs/d/colls/c/docs/1", &[(PK, "[\"a\"]")], &json!({"id": "1", "pk": "a", "v": 2})).await;
    let r = feed(vec![("if-none-match", first)]).await;
    assert_eq!(r.status, 200);
    let ids: Vec<String> = r.json()["Documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids, ["2", "3", "1"]); // 1 was modified last
    assert!(r.json()["Documents"][0]["_lsn"].is_number());
    let cont = r.header("etag").unwrap().to_string();
    assert_eq!(feed(vec![("if-none-match", cont.clone())]).await.status, 304);
    h.doc("d", "c", &json!("b"), &json!({"id": "4", "pk": "b"})).await;
    let r = feed(vec![("if-none-match", cont), ("x-ms-documentdb-partitionkey", "[\"b\"]".to_string())]).await;
    assert_eq!(r.json()["Documents"][0]["id"], "4");
    // from now
    assert_eq!(feed(vec![("if-none-match", "*".to_string())]).await.status, 304);
}

#[tokio::test]
async fn ttl_expires_documents_lazily() {
    let h = start().await;
    h.db("d").await;
    let r = h
        .json(
            "POST",
            "dbs/d/colls",
            &[],
            &json!({"id": "c", "defaultTtl": 1, "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}}),
        )
        .await;
    assert_eq!(r.status, 201);
    h.doc("d", "c", &json!("k"), &json!({"id": "short", "pk": "k"})).await;
    h.doc("d", "c", &json!("k"), &json!({"id": "forever", "pk": "k", "ttl": -1})).await;
    assert_eq!(h.doc("d", "c", &json!("k"), &json!({"id": "bad", "pk": "k", "ttl": 0})).await.status, 400);
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/short", &[(PK, "[\"k\"]")], "").await.status, 200);
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/short", &[(PK, "[\"k\"]")], "").await.status, 404);
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/forever", &[(PK, "[\"k\"]")], "").await.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_listener_uses_the_default_account() {
    let dir = std::env::temp_dir().join(format!("azemu-cosmos-pin-{}", std::process::id()));
    let emu = Emulator::builder()
        .service(Arc::new(CosmosService::new()))
        .pinned("127.0.0.1:0".parse().unwrap(), "cosmos")
        .cert_dir(dir)
        .start()
        .await
        .unwrap();
    let port = emu.pinned_addr("cosmos").unwrap().port();
    let client = Client::new();
    let url = format!("http://localhost:{port}");
    let auth = [("authorization", "type%3Dmaster%26ver%3D1.0%26sig%3Dxyz"), ("content-type", "application/json")];
    let r = client.send("GET", &format!("{url}/"), &auth, "").await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["id"], "localhost");
    assert_eq!(v["writableLocations"][0]["databaseAccountEndpoint"], format!("http://localhost:{port}/"));
    let r = client.send("POST", &format!("{url}/dbs"), &auth, json!({"id": "pinned"}).to_string()).await;
    assert_eq!(r.status, 201);
    emu.shutdown().await;
}

#[tokio::test]
async fn patch_body_may_be_a_bare_array_of_operations() {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h.doc("d", "c", &json!("k"), &json!({"id": "a", "pk": "k", "n": 1})).await;
    let ops = json!([{"op": "incr", "path": "/n", "value": 2}, {"op": "add", "path": "/x", "value": true}]);
    let r = h.json("PATCH", "dbs/d/colls/c/docs/a", &[(PK, "[\"k\"]")], &ops).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!((r.json()["n"].clone(), r.json()["x"].clone()), (json!(3), json!(true)));
    // an empty array and a body that is neither form are still 400
    assert_eq!(h.json("PATCH", "dbs/d/colls/c/docs/a", &[(PK, "[\"k\"]")], &json!([])).await.status, 400);
    assert_eq!(h.json("PATCH", "dbs/d/colls/c/docs/a", &[(PK, "[\"k\"]")], &json!({"ops": []})).await.status, 400);
}

/// What the JavaScript bulk executor sends: no partition key header, a partition key range id,
/// and the partition key in each operation as a string holding a JSON array.
#[tokio::test]
async fn bulk_batch_runs_each_operation_on_its_own_partition() {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h.doc("d", "c", &json!("p1"), &json!({"id": "exists", "pk": "p1", "v": 0})).await;
    let hdrs = [
        ("x-ms-cosmos-is-batch-request", "True"),
        ("x-ms-cosmos-batch-atomic", "False"),
        ("x-ms-cosmos-batch-continue-on-error", "True"),
        ("x-ms-documentdb-partitionkeyrangeid", "0"),
    ];
    let ops = json!([
        {"operationType": "Create", "partitionKey": "[\"p1\"]", "resourceBody": {"id": "c1", "pk": "p1", "v": 1}},
        {"operationType": "Create", "partitionKey": "[\"p2\"]", "resourceBody": {"id": "c1", "pk": "p2", "v": 2}},
        {"operationType": "Upsert", "partitionKey": "[\"p2\"]", "resourceBody": {"id": "u1", "pk": "p2"}},
        {"operationType": "Read", "id": "exists", "partitionKey": "[\"p1\"]"},
        {"operationType": "Read", "id": "exists", "partitionKey": "[\"p2\"]"},
        {"operationType": "Create", "partitionKey": "[\"p1\"]", "resourceBody": {"id": "exists", "pk": "p1"}},
        {"operationType": "Replace", "id": "exists", "partitionKey": "[\"p1\"]", "resourceBody": {"id": "exists", "pk": "p1", "v": 99}, "ifMatch": "\"nope\""},
        {"operationType": "Patch", "id": "c1", "partitionKey": "[\"p2\"]", "resourceBody": {"operations": [{"op": "set", "path": "/v", "value": 5}]}},
        {"operationType": "Delete", "id": "c1", "partitionKey": "[\"p1\"]"},
        {"operationType": "Create", "resourceBody": {"id": "nopk", "pk": "p1"}},
        {"operationType": "Create", "partitionKey": "[\"p9\"]", "resourceBody": {"id": "mismatch", "pk": "p1"}},
    ]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdrs, &ops).await;
    assert_eq!(r.status, 207, "{}", r.text());
    assert!(r.header("x-ms-request-charge").is_some());
    let body = r.json();
    let codes: Vec<i64> = body.as_array().unwrap().iter().map(|x| x["statusCode"].as_i64().unwrap()).collect();
    assert_eq!(codes, [201, 201, 201, 200, 404, 409, 412, 200, 204, 400, 400]);
    // successful operations carry the body, the etag and a charge; failed ones a charge too
    assert_eq!(body[0]["resourceBody"]["id"], "c1");
    assert_eq!(body[0]["eTag"], body[0]["resourceBody"]["_etag"]);
    assert!(body[0]["requestCharge"].as_f64().unwrap() > 0.0 && body[4]["requestCharge"].as_f64().unwrap() > 0.0);
    assert_eq!(body[3]["resourceBody"]["v"], 0);
    assert_eq!(body[7]["resourceBody"]["v"], 5);
    assert!(body[8].get("resourceBody").is_none());
    // what worked is stored (c1 exists on p2 only; its twin on p1 was deleted), what failed is not
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/c1", &[(PK, "[\"p2\"]")], "").await.status, 200);
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/c1", &[(PK, "[\"p1\"]")], "").await.status, 404);
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/exists", &[(PK, "[\"p1\"]")], "").await.json()["v"], 0);

    // all good: 200
    let ok = json!([{"operationType": "Create", "partitionKey": "[\"q\"]", "resourceBody": {"id": "a", "pk": "q"}}]);
    assert_eq!(h.json("POST", "dbs/d/colls/c/docs", &hdrs, &ok).await.status, 200);

    // continue-on-error false: stop at the first failure, the rest answer 424 and are not run
    let hdrs =
        [("x-ms-cosmos-is-batch-request", "True"), ("x-ms-cosmos-batch-atomic", "False"), ("x-ms-cosmos-batch-continue-on-error", "False")];
    let ops = json!([
        {"operationType": "Create", "partitionKey": "[\"r\"]", "resourceBody": {"id": "1", "pk": "r"}},
        {"operationType": "Create", "partitionKey": "[\"r\"]", "resourceBody": {"id": "1", "pk": "r"}},
        {"operationType": "Create", "partitionKey": "[\"r\"]", "resourceBody": {"id": "3", "pk": "r"}},
    ]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdrs, &ops).await;
    assert_eq!(r.status, 207);
    let codes: Vec<i64> = r.json().as_array().unwrap().iter().map(|x| x["statusCode"].as_i64().unwrap()).collect();
    assert_eq!(codes, [201, 409, 424]);
    assert_eq!(h.req("GET", "dbs/d/colls/c/docs/3", &[(PK, "[\"r\"]")], "").await.status, 404);

    // the header partition key still serves as the default for operations without their own
    let hdrs = [("x-ms-cosmos-is-batch-request", "True"), ("x-ms-cosmos-batch-atomic", "False"), (PK, "[\"r\"]")];
    let ops = json!([{"operationType": "Read", "id": "1"}, {"operationType": "Read", "id": "1", "partitionKey": ["zz"]}]);
    let r = h.json("POST", "dbs/d/colls/c/docs", &hdrs, &ops).await;
    let codes: Vec<i64> = r.json().as_array().unwrap().iter().map(|x| x["statusCode"].as_i64().unwrap()).collect();
    assert_eq!(codes, [200, 404]);
}

/// Enum-like strings in container definitions are accepted in any case (the Go SDK sends
/// "Consistent" and "None"); the definition shows the documented spelling.
#[tokio::test]
async fn container_enum_strings_are_not_case_sensitive() {
    let h = start().await;
    h.db("d").await;
    for (id, mode, expect) in [("a", "Consistent", "consistent"), ("b", "None", "none"), ("c", "LAZY", "lazy")] {
        let body = json!({
            "id": id,
            "partitionKey": {"paths": ["/pk"], "kind": "hash", "version": 2},
            "indexingPolicy": {"indexingMode": mode},
            "conflictResolutionPolicy": {"mode": "lastwriterwins", "conflictResolutionPath": "/_ts"},
        });
        let r = h.json("POST", "dbs/d/colls", &[], &body).await;
        assert_eq!(r.status, 201, "{id}: {}", r.text());
        let v = r.json();
        assert_eq!(v["indexingPolicy"]["indexingMode"], expect);
        assert_eq!(v["partitionKey"]["kind"], "Hash");
        assert_eq!(v["conflictResolutionPolicy"]["mode"], "LastWriterWins");
    }
    // replace takes the same spellings
    let body =
        json!({"id": "a", "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}, "indexingPolicy": {"indexingMode": "None"}});
    assert_eq!(h.json("PUT", "dbs/d/colls/a", &[], &body).await.status, 200);
    // hierarchical keys and an unknown mode
    let multi = json!({"id": "m", "partitionKey": {"paths": ["/a", "/b"], "kind": "multihash", "version": 2}});
    let r = h.json("POST", "dbs/d/colls", &[], &multi).await;
    assert_eq!((r.status, r.json()["partitionKey"]["kind"].clone()), (201, json!("MultiHash")), "{}", r.text());
    let bad = json!({"id": "x", "partitionKey": {"paths": ["/pk"]}, "indexingPolicy": {"indexingMode": "sometimes"}});
    assert_eq!(h.json("POST", "dbs/d/colls", &[], &bad).await.status, 400);
    let bad = json!({"id": "x", "partitionKey": {"paths": ["/pk"], "kind": "Bogus"}});
    assert_eq!(h.json("POST", "dbs/d/colls", &[], &bad).await.status, 400);
    // composite index order in capitals still lets a two-field ORDER BY through
    let comp = json!({"id": "o", "partitionKey": {"paths": ["/pk"]}, "indexingPolicy": {"compositeIndexes": [[{"path": "/a", "order": "Ascending"}, {"path": "/b", "order": "DESCENDING"}]]}});
    assert_eq!(h.json("POST", "dbs/d/colls", &[], &comp).await.status, 201);
    h.doc("d", "o", &json!("k"), &json!({"id": "1", "pk": "k", "a": 1, "b": 2})).await;
    let r = h.query("d", "o", &[(PK, "[\"k\"]")], json!({"query": "SELECT * FROM c ORDER BY c.a ASC, c.b DESC"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
}
