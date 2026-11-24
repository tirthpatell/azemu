mod common;

use azemu_monitor::Limits;
use common::*;
use serde_json::json;

async fn fill(t: &T) {
    let mut other = envelope("RequestData", "GET /b", "op2");
    other["iKey"] = json!("99999999-2222-3333-4444-555555555555");
    let metric = envelope("MetricData", "queue.depth", "op1");
    let r = t
        .track(&json!([
            envelope("RequestData", "GET /a", "op1"),
            envelope("RemoteDependencyData", "SQL select", "op1"),
            envelope("ExceptionData", "boom", "op1"),
            metric,
            other,
            envelope("PageViewData", "home", "op3"),
            envelope("AvailabilityData", "ping test", "op4"),
            envelope("EventData", "clicked", "op3"),
        ]))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
}

#[tokio::test]
async fn filters() {
    let t = start().await;
    fill(&t).await;
    let n = |q: &str| {
        let q = q.to_string();
        let t = &t;
        async move { t.telemetry(&q).await["count"].as_u64().unwrap() }
    };
    assert_eq!(n("").await, 8);
    assert_eq!(n("?ikey=99999999-2222-3333-4444-555555555555").await, 1);
    assert_eq!(n(&format!("?ikey={IKEY}")).await, 7);
    assert_eq!(n("?type=RequestData").await, 2);
    assert_eq!(n("?type=ExceptionData").await, 1);
    for ty in ["RemoteDependencyData", "MessageData", "EventData", "MetricData", "PageViewData", "AvailabilityData"] {
        assert!(n(&format!("?type={ty}")).await <= 1, "{ty}");
    }
    assert_eq!(n("?name=GET%20/a").await, 1);
    assert_eq!(n("?name=queue.depth").await, 1, "the name of a metric");
    assert_eq!(n("?name=Microsoft.ApplicationInsights.Request").await, 2, "the envelope's own name (two requests)");
    assert_eq!(n("?operationId=op1").await, 4);
    assert_eq!(n("?operationId=op1&type=RemoteDependencyData").await, 1);
    assert_eq!(n("?operationId=nope").await, 0);
    assert_eq!(n("?limit=3").await, 3);
    let j = t.telemetry("?limit=3").await;
    assert_eq!(j["truncated"], true);
    assert_eq!(j["items"][0]["type"], "RequestData", "the oldest first");
    assert_eq!(t.telemetry("?limit=100").await["truncated"], false);
}

#[tokio::test]
async fn since_asks_for_what_came_after_a_point() {
    let t = start().await;
    t.track(&json!([envelope("EventData", "e1", "a")])).await;
    let mark = t.telemetry("").await["lastSequence"].as_u64().unwrap();
    assert_eq!(mark, 1);
    t.track(&json!([envelope("EventData", "e2", "b"), envelope("EventData", "e3", "b")])).await;
    let j = t.telemetry(&format!("?since={mark}")).await;
    assert_eq!(j["count"], 2);
    assert_eq!(j["items"][0]["name"], "e2");
    assert_eq!(j["items"][0]["sequence"], 2);
    assert_eq!(t.telemetry("?since=3").await["count"], 0);
    // a delete does not start the numbers again
    assert_eq!(t.admin("DELETE", "/telemetry").await.json(), json!({"deleted": 3}));
    t.track(&json!([envelope("EventData", "e4", "c")])).await;
    assert_eq!(t.telemetry(&format!("?since={mark}")).await["items"][0]["sequence"], 4);
}

#[tokio::test]
async fn bad_query_parameters_are_refused() {
    let t = start().await;
    for q in ["?since=abc", "?limit=-1", "?operation_id=x", "?bogus=1"] {
        let r = t.admin("GET", &format!("/telemetry{q}")).await;
        assert_eq!(r.status, 400, "{q}");
        assert_eq!(r.json()["error"]["code"], "InvalidParameter");
    }
    assert_eq!(t.admin("GET", "/logs?type=x").await.status, 400);
    assert_eq!(t.admin("GET", "/nothing").await.status, 404);
    assert_eq!(t.admin("POST", "/telemetry").await.status, 405);
    assert_eq!(t.admin("PUT", "/").await.status, 405);
}

#[tokio::test]
async fn summary_counts_per_key_and_type() {
    let t = start().await;
    fill(&t).await;
    t.upload("dcr-1", "s1", &[BEARER, JSON], json!([{"a": 1}, {"a": 2}]).to_string().into_bytes()).await;
    let j = t.admin("GET", "/").await.json();
    assert_eq!(j["telemetry"]["stored"], 8);
    assert_eq!(j["telemetry"]["byInstrumentationKey"][IKEY]["RequestData"], 1);
    assert_eq!(j["telemetry"]["byInstrumentationKey"][IKEY]["MetricData"], 1);
    assert_eq!(j["telemetry"]["byInstrumentationKey"]["99999999-2222-3333-4444-555555555555"]["RequestData"], 1);
    assert_eq!(j["logs"]["stored"], 2);
    assert_eq!(j["logs"]["byStream"], json!([{"dcr": "dcr-1", "stream": "s1", "count": 2}]));
    assert_eq!(j["limits"]["maxItems"], 50_000);
    // the same without the trailing slash
    assert_eq!(t.srv.send("GET", &format!("{}/_azemu/monitor", t.root), &[], "").await.status, 200);
}

#[tokio::test]
async fn delete_and_reset_empty_the_lists() {
    let t = start().await;
    fill(&t).await;
    t.upload("dcr-1", "s1", &[BEARER, JSON], json!([{"a": 1}]).to_string().into_bytes()).await;
    assert_eq!(t.admin("DELETE", "/logs").await.json(), json!({"deleted": 1}));
    assert_eq!(t.telemetry("").await["count"], 8, "the other list is left alone");
    t.upload("dcr-1", "s1", &[BEARER, JSON], json!([{"a": 1}]).to_string().into_bytes()).await;
    let r = t.srv.send("POST", &format!("{}/_azemu/reset", t.root), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(t.telemetry("").await["count"], 0);
    assert_eq!(t.admin("GET", "/logs").await.json()["count"], 0);
    fill(&t).await;
    let r = t.srv.send("POST", &format!("{}/_azemu/reset?service=monitor", t.root), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(t.telemetry("").await["count"], 0);
}

#[tokio::test]
async fn the_oldest_are_dropped_and_counted() {
    let t = start_with(Limits { max_items: 5, max_log_rows: 3, max_bytes: 1 << 20 }).await;
    let batch: Vec<_> = (0..8).map(|i| envelope("EventData", &format!("e{i}"), "op")).collect();
    assert_eq!(t.track(&json!(batch)).await.status, 200);
    let j = t.telemetry("").await;
    assert_eq!((j["count"].as_u64(), j["stored"].as_u64(), j["dropped"].as_u64()), (Some(5), Some(5), Some(3)));
    assert_eq!(j["items"][0]["name"], "e3");
    assert_eq!(j["items"][4]["name"], "e7");
    assert_eq!(j["lastSequence"], 8);
    t.upload("d", "s", &[BEARER, JSON], json!([{"i": 0}, {"i": 1}, {"i": 2}, {"i": 3}]).to_string().into_bytes()).await;
    let j = t.admin("GET", "/logs").await.json();
    assert_eq!((j["count"].as_u64(), j["dropped"].as_u64()), (Some(3), Some(1)));
    assert_eq!(j["items"][0]["row"]["i"], 1);
    // the summary says it too
    assert_eq!(t.admin("GET", "/").await.json()["telemetry"]["dropped"], 3);
}

#[tokio::test]
async fn the_byte_limit_drops_the_oldest_as_well() {
    let t = start_with(Limits { max_items: 1000, max_log_rows: 1000, max_bytes: 2000 }).await;
    let batch: Vec<_> = (0..10).map(|i| envelope("EventData", &format!("e{i}"), "op")).collect();
    t.track(&json!(batch)).await;
    let j = t.telemetry("").await;
    let stored = j["stored"].as_u64().unwrap();
    assert!((1..10).contains(&stored), "{stored}");
    assert_eq!(j["dropped"].as_u64().unwrap() + stored, 10);
    assert_eq!(j["items"].as_array().unwrap().last().unwrap()["name"], "e9", "the newest stays");
}
