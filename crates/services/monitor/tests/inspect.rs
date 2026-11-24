//! The inspector of the telemetry sink (`/_azemu/inspect/monitor/...`).

mod common;

use azemu_core::inspect::walk::walk;
use common::*;
use serde_json::json;

async fn fill(t: &T) {
    let mut other = envelope("RequestData", "GET /b", "op2");
    other["iKey"] = json!("99999999-2222-3333-4444-555555555555");
    let items: Vec<_> = (0..7)
        .map(|i| envelope("RequestData", &format!("GET /{i}"), "op1"))
        .chain([other, envelope("ExceptionData", "boom", "op1")])
        .collect();
    assert_eq!(t.track(&json!(items)).await.status, 200);
    let rows = json!([{"a": 1}, {"a": 2}, {"a": 3}]).to_string().into_bytes();
    assert_eq!(t.upload("dcr-1", "Custom-One_CL", &[BEARER, JSON], rows.clone()).await.status, 204);
    assert_eq!(t.upload("dcr-2", "Custom-Two_CL", &[BEARER, JSON], rows).await.status, 204);
}

async fn insp(t: &T, p: &str) -> azemu_core::testing::TestResponse {
    t.srv.send("GET", &format!("{}/_azemu/inspect{p}", t.root), &[], "").await
}

#[tokio::test]
async fn the_whole_tree_can_be_walked_and_paging_ends() {
    let t = start().await;
    fill(&t).await;
    for limit in [1, 4, 50] {
        let nodes = walk(&t.srv, "/monitor", limit).await;
        let kinds: Vec<&str> = nodes.iter().map(|n| n.node["kind"].as_str().unwrap()).collect();
        for k in ["service", "sink", "telemetry", "logs"] {
            assert!(kinds.contains(&k), "no {k} among {kinds:?}");
        }
    }
}

#[tokio::test]
async fn rows_come_oldest_first_with_filters_and_pages_follow_the_sequence() {
    let t = start().await;
    fill(&t).await;
    let sink = insp(&t, "/monitor/sink").await.json();
    assert_eq!(sink["properties"]["telemetryStored"], 9);
    assert_eq!(sink["properties"]["logRowsStored"], 6);
    let mut seen = Vec::new();
    let mut marker = String::new();
    for _ in 0..10 {
        let v = insp(&t, &format!("/monitor/sink/telemetry?limit=4{marker}")).await.json();
        seen.extend(v["items"].as_array().unwrap().iter().map(|i| i["sequence"].as_u64().unwrap()));
        match v["next"].as_str() {
            Some(m) => marker = format!("&marker={m}"),
            None => break,
        }
    }
    assert_eq!(seen, (1..=9).collect::<Vec<u64>>());
    let ex = insp(&t, "/monitor/sink/telemetry?type=ExceptionData").await.json();
    assert_eq!(ex["items"].as_array().unwrap().len(), 1);
    assert_eq!(ex["items"][0]["envelope"]["data"]["baseType"], "ExceptionData");
    let logs = insp(&t, "/monitor/sink/logs?dcr=dcr-2").await.json();
    assert_eq!(logs["items"].as_array().unwrap().len(), 3);
    assert_eq!(logs["items"][0]["row"]["a"], 1);
}

#[tokio::test]
async fn looking_changes_nothing() {
    let t = start().await;
    fill(&t).await;
    let before = (t.telemetry("").await, t.admin("GET", "/").await.text());
    walk(&t.srv, "", 2).await;
    assert_eq!(before, (t.telemetry("").await, t.admin("GET", "/").await.text()));
    assert_eq!(insp(&t, "/monitor/other").await.status, 404);
    assert_eq!(insp(&t, "/monitor/sink/nothing").await.status, 404);
}
