//! What a request can make the sink hold. Items are kept as the text they came as, so the byte limit counts what is held
//! (a parsed JSON value takes about thirty times the size of its text), and a body is read one item at a time.

mod common;

use common::*;
use serde_json::json;

/// The tests measure the memory of the whole process, so they run one at a time.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Resident memory of this process, in bytes (the server runs in the same process).
fn rss() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    statm.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()).unwrap_or(0) * 4096
}

/// An envelope of about 1.2 MB made of a long list of small numbers: the worst case for a parsed value.
fn heavy() -> serde_json::Value {
    let mut e = envelope("EventData", "heavy", "op");
    e["data"]["baseData"]["pad"] = json!(vec![0; 600_000]);
    e
}

#[tokio::test]
async fn items_are_kept_as_text_not_as_parsed_values() {
    let _one = ONE_AT_A_TIME.lock().await;
    let t = start().await;
    let body = json!([heavy(), heavy(), heavy(), heavy()]).to_string();
    let packed = gzip(body.as_bytes());
    assert!(packed.len() < 20_000, "{}", packed.len());
    let before = rss();
    for _ in 0..3 {
        let r = t.track_raw("/v2/track", &[("content-encoding", "gzip")], packed.clone()).await;
        assert_eq!((r.status, r.json()["itemsAccepted"].as_u64()), (200, Some(4)), "{}", r.text());
    }
    let grown = rss().saturating_sub(before);
    // 12 items of 1.2 MB are 14 MB of text; as parsed values they were over 400 MB
    assert!(grown < 150 << 20, "three small gzip requests grew the process by {grown} bytes");
    // and what was kept reads back whole
    let got = t.telemetry("").await;
    assert_eq!(got["count"], 12);
    assert_eq!(got["items"][0]["envelope"]["data"]["baseData"]["pad"].as_array().map(Vec::len), Some(600_000));
}

#[tokio::test]
async fn the_byte_limit_counts_the_text_that_is_held() {
    let _one = ONE_AT_A_TIME.lock().await;
    let t = start_with(azemu_monitor::Limits { max_items: 1000, max_log_rows: 1000, max_bytes: 3 << 20 }).await;
    // each item is 1.2 MB of text: two fit in 3 MiB, the third makes the oldest go
    for _ in 0..3 {
        assert_eq!(t.track(&json!([heavy()])).await.status, 200);
    }
    let got = t.telemetry("").await;
    assert_eq!((got["count"].as_u64(), got["dropped"].as_u64()), (Some(2), Some(1)), "{got}");
}

#[tokio::test]
async fn a_body_of_millions_of_tiny_items_is_refused_before_it_is_held() {
    let _one = ONE_AT_A_TIME.lock().await;
    let t = start().await;
    // 64 MiB once undone: the most a body may be. 21 million empty objects, in a few kilobytes of gzip
    let mut body = Vec::with_capacity(64 << 20);
    body.push(b'[');
    while body.len() + 3 < 64 << 20 {
        body.extend_from_slice(b"{},");
    }
    body.extend_from_slice(b"{}]");
    let packed = gzip(&body);
    drop(body);
    let before = rss();
    let r = t.track_raw("/v2/track", &[("content-encoding", "gzip")], packed).await;
    assert_eq!(r.status, 400, "{}", r.text());
    let text = r.text();
    assert!(text.contains("more than 65536 items"), "{}", &text[..text.len().min(300)]);
    assert!(text.len() < 10_000, "the refusal is short: {} bytes", text.len());
    let grown = rss().saturating_sub(before);
    assert!(grown < 200 << 20, "the request grew the process by {grown} bytes");
    assert_eq!(t.telemetry("").await["count"], 0);
}

#[tokio::test]
async fn the_most_items_in_a_request_are_taken_one_more_is_not() {
    let _one = ONE_AT_A_TIME.lock().await;
    let t = start().await;
    let items = |n: usize| format!("[{}]", vec!["{}"; n].join(","));
    let r = t.track_raw("/v2/track", &[], items(65_536).into_bytes()).await;
    assert_eq!((r.status, r.json()["itemsReceived"].as_u64()), (400, Some(65_536)), "every item is refused, none is kept");
    let r = t.track_raw("/v2/track", &[], items(65_537).into_bytes()).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("more than 65536 items"), "{}", r.text());
    // lines count the same
    let lines: String = (0..65_537).map(|_| "{}\n").collect();
    let r = t.track_raw("/v2/track", &[], lines.into_bytes()).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("more than 65536 items"), "{}", r.text());
}

#[tokio::test]
async fn an_item_over_the_length_is_refused_without_being_parsed() {
    let _one = ONE_AT_A_TIME.lock().await;
    let t = start().await;
    // 20 MB of small numbers in one item: parsed, it would take 600 MB
    let mut e = envelope("EventData", "huge", "op");
    e["data"]["baseData"]["pad"] = json!(vec![0; 10_000_000]);
    let before = rss();
    let r = t.track(&json!([e, envelope("EventData", "fine", "op")])).await;
    assert_eq!((r.status, r.json()["itemsAccepted"].as_u64()), (206, Some(1)), "{}", r.text());
    assert_eq!(r.json()["errors"][0]["message"], "Telemetry item length must not exceed 1258291");
    assert_eq!(r.json()["errors"][0]["index"], 0);
    let grown = rss().saturating_sub(before);
    assert!(grown < 200 << 20, "the request grew the process by {grown} bytes");
}

#[tokio::test]
async fn log_rows_are_kept_as_text_too() {
    let _one = ONE_AT_A_TIME.lock().await;
    let t = start().await;
    let rows: Vec<_> = (0..20).map(|i| json!({"i": i, "pad": vec![0; 25_000]})).collect();
    let before = rss();
    for _ in 0..3 {
        let r = t
            .srv
            .send(
                "POST",
                &format!("{}/dataCollectionRules/dcr-1/streams/Custom-X?api-version=2023-01-01", t.logs),
                &[("authorization", "Bearer x"), ("content-type", "application/json"), ("content-encoding", "gzip")],
                gzip(json!(rows).to_string().as_bytes()),
            )
            .await;
        assert_eq!(r.status, 204, "{}", r.text());
    }
    let grown = rss().saturating_sub(before);
    assert!(grown < 150 << 20, "three requests grew the process by {grown} bytes");
}
