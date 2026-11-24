mod common;

use common::*;
use serde_json::json;

fn rows() -> Vec<u8> {
    json!([{"Time": "2026-11-20T10:00:00Z", "Computer": "c1"}, {"Time": "2026-11-20T10:00:01Z", "Computer": "c2"}]).to_string().into_bytes()
}

#[tokio::test]
async fn upload_is_204_and_the_rows_are_kept_with_their_rule_and_stream() {
    let t = start().await;
    let r = t.upload("dcr-0123456789abcdef", "Custom-MyTable_CL", &[BEARER, JSON], rows()).await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert_eq!(r.body.len(), 0);
    assert!(r.header("x-ms-request-id").is_some());
    let r = t.upload("dcr-other", "Custom-Other_CL", &[BEARER, JSON], json!([{"a": 1}]).to_string().into_bytes()).await;
    assert_eq!(r.status, 204);
    let all = t.admin("GET", "/logs").await.json();
    assert_eq!(all["count"], 3);
    assert_eq!(all["items"][0]["dcr"], "dcr-0123456789abcdef");
    assert_eq!(all["items"][0]["stream"], "Custom-MyTable_CL");
    assert_eq!(all["items"][0]["row"]["Computer"], "c1");
    let one = t.admin("GET", "/logs?dcr=dcr-other").await.json();
    assert_eq!(one["count"], 1);
    let one = t.admin("GET", "/logs?stream=Custom-MyTable_CL").await.json();
    assert_eq!(one["count"], 2);
}

#[tokio::test]
async fn gzip_and_the_root_host_form() {
    let t = start().await;
    let r = t.upload("dcr-1", "s", &[BEARER, JSON, ("content-encoding", "gzip")], gzip(&rows())).await;
    assert_eq!(r.status, 204, "{}", r.text());
    let url = format!("{}/dataCollectionRules/dcr-1/streams/s?api-version=2023-01-01", t.root);
    let r = t.srv.send("POST", &url, &[BEARER, JSON], rows()).await;
    assert_eq!(r.status, 204);
    assert_eq!(t.admin("GET", "/logs").await.json()["count"], 4);
}

#[tokio::test]
async fn errors() {
    let t = start().await;
    // no token
    let r = t.upload("dcr-1", "s", &[JSON], rows()).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["error"]["code"], "Unauthorized");
    // not an array / not objects / not JSON / bad gzip
    for body in [&b"{\"a\":1}"[..], b"[1,2]", b"nope", b""] {
        let r = t.upload("dcr-1", "s", &[BEARER, JSON], body.to_vec()).await;
        assert_eq!(r.status, 400, "{}", String::from_utf8_lossy(body));
        assert_eq!(r.json()["error"]["code"], "InvalidRequest");
    }
    let r = t.upload("dcr-1", "s", &[BEARER, JSON, ("content-encoding", "gzip")], rows()).await;
    assert_eq!(r.status, 400);
    assert_eq!(t.admin("GET", "/logs").await.json()["count"], 0, "nothing of a refused call is kept");
    let r = t.srv.send("GET", &format!("{}/dataCollectionRules/d/streams/s", t.logs), &[BEARER], "").await;
    assert_eq!(r.status, 405);
}

#[tokio::test]
async fn one_megabyte_is_the_limit_compressed_or_not() {
    let t = start().await;
    // over the limit as sent
    let big = json!([{"x": "a".repeat(1 << 20)}]).to_string().into_bytes();
    let r = t.upload("dcr-1", "s", &[BEARER, JSON], big.clone()).await;
    assert_eq!(r.status, 413, "{}", r.text());
    assert_eq!(r.json()["error"]["code"], "ContentLengthLimitExceeded");
    // small as sent, far over the limit when undone
    let huge = json!([{"x": "a".repeat(2 << 20)}]).to_string().into_bytes();
    let r = t.upload("dcr-1", "s", &[BEARER, JSON, ("content-encoding", "gzip")], gzip(&huge)).await;
    assert_eq!(r.status, 413, "{}", r.text());
    // a little over 1 MiB once undone is what the Python SDK itself sends (it counts the records without the commas between
    // them), so it is taken
    let r = t.upload("dcr-1", "s", &[BEARER, JSON, ("content-encoding", "gzip")], gzip(&big)).await;
    assert_eq!(r.status, 204, "{}", r.text());
    // just under: accepted
    let ok = json!([{"x": "a".repeat((1 << 20) - 100)}]).to_string().into_bytes();
    assert!(ok.len() < 1 << 20);
    assert_eq!(t.upload("dcr-1", "s", &[BEARER, JSON], ok).await.status, 204);
}
