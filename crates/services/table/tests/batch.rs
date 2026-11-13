mod common;
use common::*;

const T: &str = "/tbl";

async fn with_table() -> Env {
    let e = env().await;
    e.create_table("tbl").await;
    e
}

fn ent(rk: &str, extra: &str) -> String {
    format!(r#"{{"PartitionKey":"p","RowKey":"{rk}"{extra}}}"#)
}

fn path(rk: &str) -> String {
    format!("/tbl(PartitionKey='p',RowKey='{rk}')")
}

#[tokio::test]
async fn all_operations_in_one_change_set() {
    let e = with_table().await;
    for rk in ["del", "upd", "rep"] {
        e.insert("tbl", &ent(rk, r#","v":1,"keep":1"#)).await;
    }
    let (b1, b2, b3, b4) = (ent("new", r#","v":1"#), ent("upd", r#","v":2"#), ent("rep", r#","v":2"#), ent("ups", r#","v":9"#));
    let (pdel, pupd, prep, pups) = (path("del"), path("upd"), path("rep"), path("ups"));
    let body = batch(&[
        ("POST", T, &[], &b1),
        ("DELETE", &pdel, &[("If-Match", "*")], ""),
        ("PATCH", &pupd, &[("If-Match", "*")], &b2),
        ("PUT", &prep, &[("If-Match", "*")], &b3),
        ("PUT", &pups, &[], &b4),
    ]);
    let r = e.send("POST", "/$batch", &[BATCH_CT], &body).await;
    assert_eq!(r.status, 202, "{}", r.text());
    assert!(r.header("content-type").unwrap().starts_with("multipart/mixed; boundary=batchresponse_"));
    let p = parts(&r);
    let statuses: Vec<u16> = p.iter().map(|(s, _)| *s).collect();
    assert_eq!(statuses, [201, 204, 204, 204, 204]);
    assert!(p[0].1.contains("\"RowKey\":\"new\""), "the insert part carries the entity: {}", p[0].1);
    let text = r.text();
    assert!(text.contains("Content-Type: multipart/mixed; boundary=changesetresponse_"));
    assert!(text.contains("DataServiceVersion: 3.0;") && text.contains("DataServiceVersion: 1.0;"));
    // inside a batch answer Azure writes the content type without spaces, unlike the header of a whole answer
    // (recording `test_encoder.pyTestTableEncodertest_encoder_batch.json`, entry 1)
    assert!(text.contains("Content-Type: application/json;odata=minimalmetadata;streaming=true;charset=utf-8"), "{text}");
    assert!(text.contains(&format!("Location: {}/tbl(PartitionKey='p',RowKey='new')", e.base)), "{text}");
    assert_eq!(text.matches("ETag: W/").count(), 5 - 1, "every part but the delete has an etag");

    assert_eq!(e.row_keys("tbl", "").await, ["new", "rep", "upd", "ups"]);
    let upd = e.get("tbl", "p", "upd").await.json();
    assert_eq!((upd["v"].clone(), upd["keep"].clone()), (2.into(), 1.into()), "PATCH merges");
    assert!(e.get("tbl", "p", "rep").await.json().get("keep").is_none(), "PUT replaces");
}

#[tokio::test]
async fn etags_in_the_response_match_the_stored_entities() {
    let e = with_table().await;
    let (a, b) = (ent("a", ""), ent("b", ""));
    let r = e.send("POST", "/$batch", &[BATCH_CT], &batch(&[("POST", T, &[], &a), ("POST", T, &[], &b)])).await;
    let tags: Vec<String> = r.text().lines().filter_map(|l| l.strip_prefix("ETag: ")).map(str::to_string).collect();
    assert_eq!(tags.len(), 2);
    assert_eq!(e.get("tbl", "p", "a").await.header("etag"), Some(tags[0].as_str()));
    assert_eq!(e.get("tbl", "p", "b").await.header("etag"), Some(tags[1].as_str()));
}

#[tokio::test]
async fn a_failing_operation_undoes_the_whole_change_set() {
    let e = with_table().await;
    e.insert("tbl", &ent("keep", r#","v":1"#)).await;
    e.insert("tbl", &ent("gone", r#","v":1"#)).await;
    let before = e.get("tbl", "p", "keep").await.header("etag").unwrap().to_string();
    let (up, ins, miss) = (ent("keep", r#","v":2"#), ent("fresh", ""), ent("missing", r#","v":1"#));
    let (pkeep, pgone, pmiss) = (path("keep"), path("gone"), path("missing"));
    let r = e
        .send(
            "POST",
            "/$batch",
            &[BATCH_CT],
            &batch(&[
                ("PUT", &pkeep, &[("If-Match", "*")], &up),
                ("DELETE", &pgone, &[("If-Match", "*")], ""),
                ("POST", T, &[], &ins),
                ("PATCH", &pmiss, &[("If-Match", "*")], &miss),
            ]),
        )
        .await;
    assert_eq!(r.status, 202);
    let p = parts(&r);
    assert_eq!(p.len(), 1, "one error part and nothing else");
    assert_eq!(p[0].0, 404);
    assert!(p[0].1.contains("\"code\":\"ResourceNotFound\""), "{}", p[0].1);
    assert_eq!(e.get("tbl", "p", "keep").await.json()["v"], 1);
    assert_eq!(e.get("tbl", "p", "keep").await.header("etag"), Some(before.as_str()));
    assert_eq!(e.get("tbl", "p", "gone").await.status, 200);
    assert_eq!(e.get("tbl", "p", "fresh").await.status, 404);
}

#[tokio::test]
async fn error_messages_name_the_failing_operation() {
    let e = with_table().await;
    e.insert("tbl", &ent("taken", "")).await;
    let (a, b, c) = (ent("a", ""), ent("b", ""), ent("taken", ""));
    let r = e.send("POST", "/$batch", &[BATCH_CT], &batch(&[("POST", T, &[], &a), ("POST", T, &[], &b), ("POST", T, &[], &c)])).await;
    let p = parts(&r);
    assert_eq!(p[0].0, 409);
    assert!(p[0].1.contains("\"code\":\"EntityAlreadyExists\""));
    assert!(p[0].1.contains("\"value\":\"2:The specified entity already exists."), "{}", p[0].1);
    assert_eq!(e.row_keys("tbl", "").await, ["taken"]);

    // 412 and 404 carry no index, as recorded
    let (stale, pa) = (ent("taken", r#","v":1"#), path("taken"));
    let r = e
        .send(
            "POST",
            "/$batch",
            &[BATCH_CT],
            &batch(&[("PUT", &pa, &[("If-Match", "W/\"datetime'2012-06-15T22%3A51%3A44.9662825Z'\"")], &stale)]),
        )
        .await;
    let p = parts(&r);
    assert_eq!(p[0].0, 412);
    assert!(p[0].1.contains("\"value\":\"The update condition specified in the request was not satisfied."), "{}", p[0].1);
}

#[tokio::test]
async fn duplicates_partitions_tables_and_limits() {
    let e = with_table().await;
    let (a, a2) = (ent("a", ""), ent("a", r#","v":2"#));
    let r = e.send("POST", "/$batch", &[BATCH_CT], &batch(&[("POST", T, &[], &a), ("PUT", &path("a"), &[], &a2)])).await;
    let p = parts(&r);
    assert_eq!(p[0].0, 400);
    assert!(p[0].1.contains("\"code\":\"InvalidDuplicateRow\"") && p[0].1.contains("\"value\":\"1:The batch request contains multiple changes with same row key. An entity can appear only once in a batch request."), "{}", p[0].1);
    assert!(e.row_keys("tbl", "").await.is_empty());

    let other = r#"{"PartitionKey":"q","RowKey":"x"}"#;
    let r = e.send("POST", "/$batch", &[BATCH_CT], &batch(&[("POST", T, &[], &a), ("POST", T, &[], other)])).await;
    let p = parts(&r);
    assert!(p[0].1.contains("CommandsInBatchActOnDifferentPartitions") && p[0].1.contains("\"value\":\"1:"), "{}", p[0].1);

    let r = e.send("POST", "/$batch", &[BATCH_CT], &batch(&[("POST", "/nothere", &[], &a)])).await;
    let p = parts(&r);
    assert_eq!(p[0].0, 404);
    assert!(p[0].1.contains("\"value\":\"0:The table specified does not exist."), "{}", p[0].1);

    let r = e.send("POST", "/$batch", &[BATCH_CT], &batch(&[("POST", "/1bad", &[], &a)])).await;
    assert!(parts(&r)[0].1.contains("\"code\":\"InvalidResourceName\"") && parts(&r)[0].1.contains("\"value\":\"0:"));

    let many: Vec<String> = (0..101).map(|i| ent(&format!("{i:03}"), "")).collect();
    let ops: Vec<Op> = many.iter().map(|b| ("POST", T, &[][..], b.as_str())).collect();
    let r = e.send("POST", "/$batch", &[BATCH_CT], &batch(&ops)).await;
    let p = parts(&r);
    assert_eq!(p[0].0, 400);
    assert!(
        p[0].1.contains("\"code\":\"InvalidInput\"") && p[0].1.contains("exceeds the maximum 100 changes per change set"),
        "{}",
        p[0].1
    );
    assert!(e.row_keys("tbl", "").await.is_empty());

    let r = e.send("POST", "/$batch", &[BATCH_CT], &batch(&ops[..100])).await;
    assert_eq!(parts(&r).len(), 100);
    assert_eq!(e.row_keys("tbl", "").await.len(), 100);
}

#[tokio::test]
async fn invalid_entities_fail_the_batch() {
    let e = with_table().await;
    let (ok, bad) = (ent("ok", ""), ent("bad/key", ""));
    let r = e.send("POST", "/$batch", &[BATCH_CT], &batch(&[("POST", T, &[], &ok), ("POST", T, &[], &bad)])).await;
    let p = parts(&r);
    assert_eq!(p[0].0, 400);
    assert!(p[0].1.contains("\"value\":\"1:"));
    assert!(e.row_keys("tbl", "").await.is_empty());
}

#[tokio::test]
async fn insert_honours_prefer_and_accept_per_operation() {
    let e = with_table().await;
    let (a, b) = (ent("a", ""), ent("b", ""));
    let r = e
        .send(
            "POST",
            "/$batch",
            &[BATCH_CT],
            &batch(&[
                ("POST", T, &[("Prefer", "return-no-content")], &a),
                ("POST", T, &[("Accept", "application/json;odata=nometadata")], &b),
            ]),
        )
        .await;
    let p = parts(&r);
    assert_eq!(p[0].0, 204);
    assert_eq!(p[1].0, 201);
    assert!(!p[1].1.contains("odata."), "nometadata inside the batch: {}", p[1].1);
}

#[tokio::test]
async fn query_in_a_batch() {
    let e = with_table().await;
    e.insert("tbl", &ent("r", r#","v":7"#)).await;
    let body = "--batch_b\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\nGET http://acct.table.core.windows.net/tbl(PartitionKey='p',RowKey='r') HTTP/1.1\r\nAccept: application/json;odata=minimalmetadata\r\n\r\n\r\n--batch_b--\r\n";
    let r = e.send("POST", "/$batch", &[BATCH_CT], body).await;
    assert_eq!(r.status, 202, "{}", r.text());
    let p = parts(&r);
    assert_eq!(p[0].0, 200);
    assert!(p[0].1.contains("\"v\":7"), "{}", p[0].1);
}

#[tokio::test]
async fn wrong_content_type_and_garbage_bodies() {
    let e = with_table().await;
    let r = e.send("POST", "/$batch", &[("Content-Type", "application/json")], "{}").await;
    assert_eq!((r.status, code(&r).as_str()), (415, "MediaTypeNotSupported"));
    let r = e.send("POST", "/$batch", &[BATCH_CT], "this is not multipart").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidInput"));
}
