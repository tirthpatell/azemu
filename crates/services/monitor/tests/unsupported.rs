mod common;

use common::*;

#[tokio::test]
async fn what_is_not_built_answers_501_and_names_itself() {
    let t = start().await;
    for (method, url) in [
        ("POST", format!("{}/v1/workspaces/w1/query", t.root)),
        ("POST", format!("{}/v1/apps/a1/query", t.root)),
        ("GET", format!("{}/v1/apps/a1/metrics/requests/count", t.root)),
        ("POST", format!("{}/v1/workspaces/w1/query", t.srv.url("api", "x"))),
        ("GET", format!("{}/something/else", t.ingest)),
    ] {
        let r = t.srv.send(method, &url, &[BEARER], "{}").await;
        assert_eq!(r.status, 501, "{method} {url}: {}", r.text());
        assert_eq!(r.json()["error"]["code"], "NotImplemented");
        assert!(r.json()["error"]["message"].as_str().unwrap().starts_with("monitor: "), "{}", r.text());
    }
    let u = t.srv.send("GET", &format!("{}/_azemu/unsupported", t.root), &[], "").await.json();
    let ops: Vec<String> = u["notImplemented"].as_array().unwrap().iter().map(|e| e["operation"].as_str().unwrap().to_string()).collect();
    assert!(ops.contains(&"Monitor_Unknown (POST /v1/workspaces/{}/query)".to_string()), "{ops:?}");
}

#[tokio::test]
async fn the_admin_prefix_is_listed() {
    let t = start().await;
    let j = t.srv.send("GET", &format!("{}/_azemu/", t.root), &[], "").await.json();
    assert!(j["endpoints"].as_array().unwrap().iter().any(|e| e["service"] == "monitor" && e["path"] == "/_azemu/monitor/..."), "{j}");
}
