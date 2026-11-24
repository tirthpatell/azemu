mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn a_good_batch_is_accepted_and_kept() {
    let t = start().await;
    let r = t.track(&json!([envelope("RequestData", "GET /a", "op1"), envelope("MessageData", "m", "op1")])).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!({"itemsReceived": 2, "itemsAccepted": 2, "appId": null, "errors": []}));
    assert!(r.header("content-type").unwrap().starts_with("application/json"));
    let got = t.telemetry("").await;
    assert_eq!(got["count"], 2);
    assert_eq!(got["items"][0]["type"], "RequestData");
    assert_eq!(got["items"][1]["type"], "MessageData");
}

#[tokio::test]
async fn both_paths_and_every_host_form() {
    let t = start().await;
    for (i, url) in [
        format!("{}/v2/track", t.ingest),
        format!("{}/v2.1/track", t.ingest),
        // an endpoint written with a trailing slash gives a double slash
        format!("{}//v2.1/track", t.ingest),
        // no host that names a service: the path is enough
        format!("{}/v2/track", t.root),
        format!("{}//v2.1/track", t.root),
    ]
    .iter()
    .enumerate()
    {
        let r = t.srv.send("POST", url, &[JSON], json!([envelope("EventData", "e", &format!("op{i}"))]).to_string()).await;
        assert_eq!(r.status, 200, "{url}: {}", r.text());
    }
    assert_eq!(t.telemetry("").await["count"], 5);
}

#[tokio::test]
async fn gzip_and_newline_delimited_bodies() {
    let t = start().await;
    let batch = json!([envelope("RequestData", "gz", "op")]).to_string();
    let r = t.track_raw("/v2.1/track", &[("content-encoding", "gzip")], gzip(batch.as_bytes())).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let nd = format!("{}\n{}\n", envelope("EventData", "n1", "op"), envelope("EventData", "n2", "op"));
    let r = t.track_raw("/v2/track", &[("content-type", "application/x-json-stream")], nd.clone().into_bytes()).await;
    assert_eq!(r.json()["itemsAccepted"], 2);
    let r = t.track_raw("/v2/track", &[("content-encoding", "gzip")], gzip(nd.as_bytes())).await;
    assert_eq!(r.json()["itemsAccepted"], 2);
    // one object that is not in an array
    let r = t.track_raw("/v2/track", &[], envelope("EventData", "single", "op").to_string().into_bytes()).await;
    assert_eq!(r.json()["itemsAccepted"], 1);
    assert_eq!(t.telemetry("").await["count"], 6);
}

#[tokio::test]
async fn a_bad_envelope_among_good_ones_is_206_with_its_index() {
    let t = start().await;
    let mut bad = envelope("RequestData", "x", "op");
    bad.as_object_mut().unwrap().remove("name");
    let r = t.track(&json!([envelope("EventData", "ok1", "op"), bad, envelope("EventData", "ok2", "op")])).await;
    assert_eq!(r.status, 206, "{}", r.text());
    let j = r.json();
    assert_eq!((j["itemsReceived"].as_u64(), j["itemsAccepted"].as_u64()), (Some(3), Some(2)));
    assert_eq!(j["errors"][0]["index"], 1);
    assert_eq!(j["errors"][0]["statusCode"], 400);
    // checked against Azure in November 2026
    assert_eq!(j["errors"][0]["message"], "109: Field 'name' on type 'Envelope' is required but missing or empty. Expected: string");
    assert_eq!(t.telemetry("").await["count"], 2, "only the good ones are kept");
}

#[tokio::test]
async fn a_bad_key_is_a_refused_item() {
    let t = start().await;
    for key in [json!("abc"), json!(5), json!(null)] {
        let mut e = envelope("RequestData", "x", "op");
        e["iKey"] = key;
        let r = t.track(&json!([e])).await;
        assert_eq!(r.status, 400);
        let j = r.json();
        assert_eq!((j["itemsReceived"].as_u64(), j["itemsAccepted"].as_u64()), (Some(1), Some(0)));
        assert_eq!(j["errors"][0]["message"], "Invalid instrumentation key");
    }
    let mut no_key = envelope("RequestData", "x", "op");
    no_key.as_object_mut().unwrap().remove("iKey");
    assert_eq!(t.track(&json!([no_key])).await.json()["errors"][0]["message"], "Invalid instrumentation key");
    // any other GUID is a key: Azure checks that it exists, the emulator cannot
    let mut other = envelope("EventData", "x", "op");
    other["iKey"] = json!("{99999999-2222-3333-4444-555555555555}");
    assert_eq!(t.track(&json!([other])).await.status, 200);
    assert_eq!(t.telemetry("?ikey=99999999222233334444555555555555").await["count"], 1, "kept under the key in its usual form");
}

#[tokio::test]
async fn an_envelope_that_is_too_old_or_too_new() {
    let t = start().await;
    let mut old = envelope("RequestData", "x", "op");
    old["time"] = json!(azemu_core::time::iso8601_7(time::OffsetDateTime::now_utc() - time::Duration::days(3)));
    let r = t.track(&json!([old])).await;
    assert_eq!(r.status, 400);
    assert_eq!(
        r.json()["errors"][0]["message"],
        "103: Field 'time' on type 'Envelope' is older than the allowed min date. Expected: now - 172800000ms"
    );
    let mut new = envelope("RequestData", "x", "op");
    new["time"] = json!(azemu_core::time::iso8601_7(time::OffsetDateTime::now_utc() + time::Duration::hours(3)));
    assert_eq!(
        t.track(&json!([new])).await.json()["errors"][0]["message"],
        "104: Field 'time' on type 'Envelope' is newer than the allowed max date. Expected: now + 7200000ms"
    );
}

#[tokio::test]
async fn bodies_that_are_not_what_they_say() {
    let t = start().await;
    // text that is not JSON is skipped: nothing received, nothing wrong (Azure answers 200)
    let r = t.track_raw("/v2/track", &[], b"this is not json".to_vec()).await;
    assert_eq!((r.status, r.json()["itemsReceived"].as_u64()), (200, Some(0)));
    // a JSON value that is cut off is a refused body
    let r = t.track_raw("/v2/track", &[], b"[{\"name\":\"x\"".to_vec()).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["errors"][0]["message"], "Invalid JSON specified in the request");
    // gzip that is not gzip: another shape of answer
    let r = t.track_raw("/v2/track", &[("content-encoding", "gzip")], b"plain text".to_vec()).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json(), json!({"message": "Malformed content", "statusCode": 400}));
    // an encoding Azure does not know is not undone
    let r =
        t.track_raw("/v2/track", &[("content-encoding", "br")], json!([envelope("EventData", "e", "op")]).to_string().into_bytes()).await;
    assert_eq!((r.status, r.json()["itemsAccepted"].as_u64()), (200, Some(1)));
    // what is not an object inside an array is left out and not counted
    let r = t.track_raw("/v2/track", &[], format!("[1, \"x\", null, {}]", envelope("EventData", "e", "op")).into_bytes()).await;
    assert_eq!((r.json()["itemsReceived"].as_u64(), r.json()["itemsAccepted"].as_u64()), (Some(1), Some(1)));
}

#[tokio::test]
async fn an_empty_body_receives_nothing() {
    let t = start().await;
    let r = t.track_raw("/v2/track", &[], Vec::new()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!({"itemsReceived": 0, "itemsAccepted": 0, "appId": null, "errors": []}));
    let r = t.track_raw("/v2/track", &[], b"[]".to_vec()).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn a_bearer_token_or_none_is_taken() {
    let t = start().await;
    for h in [vec![BEARER], vec![("authorization", "Bearer not-even-a-jwt")], vec![]] {
        let mut headers = vec![JSON];
        headers.extend(h);
        let r = t.track_raw("/v2.1/track", &headers, json!([envelope("EventData", "e", "op")]).to_string().into_bytes()).await;
        assert_eq!(r.status, 200, "{}", r.text());
    }
}

#[tokio::test]
async fn a_body_over_the_limit_is_413() {
    let t = start().await;
    // a gzip body that grows past the decoded limit (70 MiB of spaces compresses to about 70 KiB)
    let big = vec![b' '; 70 << 20];
    let r = t.track_raw("/v2/track", &[("content-encoding", "gzip")], gzip(&big)).await;
    assert_eq!(r.status, 413, "{}", r.text());
}

#[tokio::test]
async fn other_methods_and_the_preflight() {
    let t = start().await;
    for m in ["GET", "PUT", "DELETE"] {
        let r = t.srv.send(m, &format!("{}/v2/track", t.ingest), &[], "").await;
        assert_eq!(r.status, 405, "{m}");
        assert_eq!(r.header("allow"), Some("POST"));
        assert_eq!(r.body.len(), 0);
    }
    let r = t
        .srv
        .send(
            "OPTIONS",
            &format!("{}/v2.1/track", t.ingest),
            &[("origin", "https://example.com"), ("access-control-request-method", "POST")],
            "",
        )
        .await;
    assert_eq!(r.status, 204);
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
    assert_eq!(r.header("access-control-allow-methods"), Some("POST"));
    // a browser's post names its origin and gets it allowed back
    let r = t
        .track_raw("/v2.1/track", &[("origin", "https://example.com")], json!([envelope("EventData", "e", "op")]).to_string().into_bytes())
        .await;
    assert_eq!((r.status, r.header("access-control-allow-origin")), (200, Some("*")));
}

#[tokio::test]
async fn every_type_is_kept_under_its_name() {
    let t = start().await;
    let types = [
        "RequestData",
        "RemoteDependencyData",
        "ExceptionData",
        "MessageData",
        "EventData",
        "MetricData",
        "PageViewData",
        "AvailabilityData",
    ];
    let batch: Vec<_> = types.iter().map(|ty| envelope(ty, "n", "op")).collect();
    let r = t.track(&json!(batch)).await;
    assert_eq!((r.status, r.json()["itemsAccepted"].as_u64()), (200, Some(8)), "{}", r.text());
    for ty in types {
        assert_eq!(t.telemetry(&format!("?type={ty}")).await["count"], 1, "{ty}");
    }
}
