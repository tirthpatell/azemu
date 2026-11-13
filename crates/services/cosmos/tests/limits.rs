//! Request input that could overflow the stack or grow without bound is refused with a 400
//! (or 413), and the service keeps answering afterwards.

mod common;

use common::{start, H};
use serde_json::{json, Value};

const PK: [(&str, &str); 1] = [("x-ms-documentdb-partitionkey", "[\"p\"]")];

async fn setup() -> H {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h
}

async fn alive(h: &H) {
    let r = h.doc("d", "c", &json!("p"), &json!({"id": "alive", "pk": "p"})).await;
    assert!(r.status == 201 || r.status == 409, "service did not answer: {}", r.status);
}

fn nested(levels: usize) -> Value {
    let mut v = json!(1);
    for _ in 0..levels {
        v = json!({ "a": v });
    }
    v
}

async fn patch(h: &H, id: &str, body: &Value) -> azemu_core::testing::TestResponse {
    h.json("PATCH", &format!("dbs/d/colls/c/docs/{id}"), &PK, body).await
}

#[tokio::test]
async fn document_nested_100000_deep_is_400() {
    let h = setup().await;
    let body = format!("{{\"id\":\"deep\",\"pk\":\"p\",\"x\":{}1{}}}", "{\"a\":".repeat(100_000), "}".repeat(100_000));
    let r = h.req("POST", "dbs/d/colls/c/docs", &[("content-type", "application/json"), PK[0]], &body).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("nested too deeply"), "{}", r.text());
    // arrays too
    let body = format!("{{\"id\":\"deep\",\"pk\":\"p\",\"x\":{}1{}}}", "[".repeat(100_000), "]".repeat(100_000));
    let r = h.req("POST", "dbs/d/colls/c/docs", &[("content-type", "application/json"), PK[0]], &body).await;
    assert_eq!(r.status, 400);
    alive(&h).await;
}

#[tokio::test]
async fn deepest_allowed_document_is_stored_and_read_back() {
    let h = setup().await;
    // the document object is level 1, so "x" may hold 126 more levels
    let ok = json!({"id": "ok", "pk": "p", "x": nested(125)});
    assert_eq!(h.doc("d", "c", &json!("p"), &ok).await.status, 201);
    let r = h.req("GET", "dbs/d/colls/c/docs/ok", &PK, "").await;
    assert_eq!(r.status, 200);
    let too = json!({"id": "too", "pk": "p", "x": nested(127)});
    assert_eq!(h.doc("d", "c", &json!("p"), &too).await.status, 400);
}

#[tokio::test]
async fn query_nested_10000_deep_is_400() {
    let h = setup().await;
    let shapes = [
        format!("SELECT VALUE {}1{} FROM c", "(".repeat(10_000), ")".repeat(10_000)),
        format!("SELECT VALUE {}1{} FROM c", "ARRAY(SELECT VALUE ".repeat(10_000), ")".repeat(10_000)),
        format!("SELECT VALUE c.x IN ({}1{}) FROM c", "[".repeat(10_000), "]".repeat(10_000)),
        format!("SELECT VALUE {}1 FROM c", "c.q ?? ".repeat(10_000)),
        format!("SELECT VALUE {}1 FROM c", "true ? 1 : ".repeat(10_000)),
    ];
    for q in shapes {
        let r = h.query("d", "c", &PK, json!({"query": q})).await;
        assert_eq!(r.status, 400, "{}", &r.text()[..r.text().len().min(200)]);
        assert_eq!(r.json()["code"], "BadRequest");
    }
    alive(&h).await;
}

#[tokio::test]
async fn query_text_over_512_kb_is_400() {
    let h = setup().await;
    let q = format!("SELECT * FROM c WHERE c.x = 1{}", " OR c.x = 2".repeat(60_000));
    assert!(q.len() > 512 * 1024);
    let r = h.query("d", "c", &PK, json!({"query": q})).await;
    assert_eq!(r.status, 400, "{}", &r.text()[..r.text().len().min(200)]);
}

#[tokio::test]
async fn request_over_two_megabytes_is_413_for_every_kind_of_request() {
    let h = setup().await;
    let big = "a".repeat(2_200_000);
    let r = h.json("PATCH", "dbs/d/colls/c/docs/x", &PK, &json!({"operations": [{"op": "add", "path": "/a", "value": big}]})).await;
    assert_eq!(r.status, 413);
    let r = h.query("d", "c", &PK, json!({"query": "SELECT * FROM c", "parameters": [{"name": "@a", "value": big}]})).await;
    assert_eq!(r.status, 413);
    let r = h.json("POST", "dbs", &[], &json!({"id": big})).await;
    assert_eq!(r.status, 413);
    alive(&h).await;
}

#[tokio::test]
async fn patch_with_eleven_operations_is_400_and_ten_work() {
    let h = setup().await;
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p"})).await.status, 201);
    let ops = |n: usize| -> Value {
        json!({"operations": (0..n).map(|i| json!({"op": "add", "path": format!("/f{i}"), "value": i})).collect::<Vec<_>>()})
    };
    let r = patch(&h, "a", &ops(11)).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("more than 10 operations"), "{}", r.text());
    // nothing was applied
    let doc = h.req("GET", "dbs/d/colls/c/docs/a", &PK, "").await.json();
    assert!(doc.get("f0").is_none());
    assert_eq!(patch(&h, "a", &ops(10)).await.status, 200);
}

#[tokio::test]
async fn repeated_patches_cannot_build_a_document_deeper_than_the_limit() {
    let h = setup().await;
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p", "x": nested(100)})).await.status, 201);
    // path to the innermost object: /x/a/a/.../a (100 levels of "a" below x, the last holds 1)
    let mut path = String::from("/x");
    for _ in 0..99 {
        path.push_str("/a");
    }
    // adds a 100 levels deep value at the innermost object: total depth ~ 200
    let body = json!({"operations": [{"op": "add", "path": format!("{path}/b"), "value": nested(100)}]});
    let r = patch(&h, "a", &body).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("nested too deeply"), "{}", r.text());
    // ten operations each adding 100 levels, one after the other, in one request
    let ops: Vec<Value> = (0..10).map(|i| json!({"op": "add", "path": format!("{path}/b{i}"), "value": nested(30)})).collect();
    let r = patch(&h, "a", &json!({"operations": ops})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    // the document is unchanged and still readable
    let r = h.req("GET", "dbs/d/colls/c/docs/a", &PK, "").await;
    assert_eq!(r.status, 200);
    assert!(r.json()["x"]["a"]["a"].is_object());
    alive(&h).await;
}

#[tokio::test]
async fn patch_path_with_100000_segments_is_refused() {
    let h = setup().await;
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p"})).await.status, 201);
    let path = "/a".repeat(100_000);
    for op in ["add", "set", "replace", "remove", "incr"] {
        let body = json!({"operations": [{"op": op, "path": path, "value": 1}]});
        let r = patch(&h, "a", &body).await;
        assert_eq!(r.status, 400, "{op}: {}", &r.text()[..r.text().len().min(200)]);
    }
    let body = json!({"operations": [{"op": "move", "from": path, "path": "/b"}]});
    assert_eq!(patch(&h, "a", &body).await.status, 400);
    alive(&h).await;
}

#[tokio::test]
async fn patch_in_a_batch_obeys_the_same_limit() {
    let h = setup().await;
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p", "x": nested(100)})).await.status, 201);
    let mut path = String::from("/x");
    for _ in 0..99 {
        path.push_str("/a");
    }
    let ops = json!([{"operationType": "Patch", "id": "a", "resourceBody":
        {"operations": [{"op": "add", "path": format!("{path}/b"), "value": nested(100)}]}}]);
    let r = h
        .req(
            "POST",
            "dbs/d/colls/c/docs",
            &[("content-type", "application/json"), ("x-ms-cosmos-is-batch-request", "True"), PK[0]],
            &ops.to_string(),
        )
        .await;
    assert_eq!(r.status, 207, "{}", r.text());
    assert_eq!(r.json()[0]["statusCode"], 400);
}

#[tokio::test]
async fn many_patches_in_a_row_stop_at_the_nesting_limit() {
    // Each request nests 100 more levels under the previous one. Without a limit the document
    // gets thousands of levels deep and cloning, dropping or writing it overflows the stack.
    let h = setup().await;
    assert_eq!(h.doc("d", "c", &json!("p"), &json!({"id": "a", "pk": "p", "x": {}})).await.status, 201);
    let mut path = String::from("/x");
    let mut accepted = 0;
    for _ in 0..200 {
        let body = json!({"operations": [{"op": "add", "path": format!("{path}/a"), "value": nested(100)}]});
        let r = patch(&h, "a", &body).await;
        if r.status != 200 {
            assert_eq!(r.status, 400, "{}", r.text());
            break;
        }
        accepted += 1;
        for _ in 0..100 {
            path.push_str("/a");
        }
    }
    assert_eq!(accepted, 1, "accepted {accepted} patches");
    let r = h.req("GET", "dbs/d/colls/c/docs/a", &PK, "").await;
    assert_eq!(r.status, 200);
    alive(&h).await;
}
