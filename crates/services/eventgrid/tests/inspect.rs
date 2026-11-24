//! The inspector of Event Grid (`/_azemu/inspect/eventgrid/...`).

mod common;

use azemu_core::inspect::walk::walk;
use common::*;
use serde_json::json;

async fn fill(t: &Eg) {
    // a custom topic with a webhook that answers, and one that is down (its deliveries wait for a retry)
    let hook = Hook::ok().await;
    t.put_json("/topics/orders", &json!({"inputSchema": "EventGridSchema"})).await;
    assert_eq!(t.put_json("/topics/orders/subscriptions/hook", &json!({"webhookUrl": hook.url})).await.status, 201);
    assert_eq!(t.put_json("/topics/orders/subscriptions/down", &json!({"webhookUrl": "http://127.0.0.1:1/x"})).await.status, 201);
    assert_eq!(t.publish_eg("orders", json!([eg("e1", "t.created", "/a"), eg("e2", "t.created", "/b")])).await.status, 200);
    // a namespace topic with a pull subscription: some events available, one locked, one rejected
    t.put_json("/topics/jobs?namespace=ns1", &json!({})).await;
    t.put_json("/topics/jobs/subscriptions/w?namespace=ns1", &json!({})).await;
    t.ns_publish("ns1", "jobs", json!([ce("1", "x"), ce("2", "x"), ce("3", "x"), ce("4", "x")])).await;
    let got = t.receive("ns1", "jobs", "w", "maxWaitTime=1&maxEvents=2").await.json();
    let tok = got["value"][0]["brokerProperties"]["lockToken"].as_str().unwrap().to_string();
    t.ns_op("ns1", "jobs", "w", "reject", "", json!({"lockTokens": [tok]})).await;
    // keep the hook alive for the test
    std::mem::forget(hook);
}

async fn insp(t: &Eg, p: &str) -> azemu_core::testing::TestResponse {
    t.srv.send("GET", &format!("{}/_azemu/inspect{p}", t.srv.gateway_url()), &[], "").await
}

#[tokio::test]
async fn the_whole_tree_can_be_walked_and_paging_ends() {
    let t = Eg::new().await;
    fill(&t).await;
    for limit in [1, 2, 50] {
        let nodes = walk(&t.srv, "/eventgrid", limit).await;
        let kinds: Vec<&str> = nodes.iter().map(|n| n.node["kind"].as_str().unwrap()).collect();
        for k in ["service", "host", "subscriptions", "subscription", "deliveries", "deadletters", "events", "pending", "topics", "topic"] {
            assert!(kinds.contains(&k), "no {k} among {kinds:?}");
        }
    }
}

#[tokio::test]
async fn subscriptions_show_destination_validation_counts_and_the_logs() {
    let t = Eg::new().await;
    fill(&t).await;
    let subs = insp(&t, "/eventgrid/orders/subscriptions").await.json();
    let names: Vec<&str> = subs["items"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["down", "hook"]);
    let hook = insp(&t, "/eventgrid/orders/subscriptions/hook").await.json();
    assert_eq!(hook["properties"]["validation"]["status"], "Succeeded");
    assert_eq!(hook["properties"]["deliveryMode"], "push");
    let hook_log = insp(&t, "/eventgrid/orders/subscriptions/hook/deliveries").await;
    assert_eq!(hook_log.status, 200);
    // the pull queue: two events available, one locked, one dead-lettered
    let w = insp(&t, "/eventgrid/ns1/topics/jobs/subscriptions/w").await.json();
    assert_eq!(w["properties"]["stats"]["deadLettered"], 1);
    let ev = insp(&t, "/eventgrid/ns1/topics/jobs/subscriptions/w/events?limit=2").await.json();
    assert_eq!(ev["items"].as_array().unwrap().len(), 2);
    assert!(ev["next"].is_string());
    let states: Vec<String> = {
        let all = insp(&t, "/eventgrid/ns1/topics/jobs/subscriptions/w/events").await.json();
        all["items"].as_array().unwrap().iter().map(|i| i["state"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(states.iter().filter(|s| *s == "locked").count(), 1);
    assert_eq!(states.iter().filter(|s| *s == "available").count(), 2);
    let dead = insp(&t, "/eventgrid/ns1/topics/jobs/subscriptions/w/deadletters").await.json();
    assert_eq!(dead["items"][0]["reason"], "Rejected");
    assert_eq!(dead["items"][0]["event"]["id"], "1");
    // no lock token anywhere in the walk
    let text = insp(&t, "/eventgrid/ns1/topics/jobs/subscriptions/w/events").await.text();
    assert!(!text.contains("lockToken"));
}

#[tokio::test]
async fn looking_changes_nothing_and_a_missing_host_is_not_made() {
    let t = Eg::new().await;
    fill(&t).await;
    let view = || async {
        (
            t.get_admin("/topics/jobs/subscriptions/w?namespace=ns1").await.json()["stats"].clone(),
            t.get_admin("/topics/jobs/subscriptions/w/events?namespace=ns1").await.json(),
        )
    };
    let before = view().await;
    walk(&t.srv, "", 1).await;
    assert_eq!(before, view().await);
    for p in ["/eventgrid/ghost", "/eventgrid/orders/subscriptions/none", "/eventgrid/orders/other", "/eventgrid/ns1/topics/none"] {
        assert_eq!(insp(&t, p).await.status, 404, "{p}");
    }
    let n = insp(&t, "/eventgrid").await.json()["properties"]["accounts"].clone();
    assert_eq!(n, 2);
}
