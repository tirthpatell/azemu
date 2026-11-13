//! Deliveries waiting for a retry are bounded (per subscription and in all), the oldest are dead-lettered
//! as `PendingQueueFull`, and a webhook reply is read only up to a limit.

mod common;

use std::time::Duration;

use azemu_eventgrid::{Options, PushLimits};
use common::*;
use serde_json::{json, Value};

/// A webhook that is down and retries slowly, so deliveries stay pending for the whole test.
fn stuck(limits: PushLimits) -> Options {
    Options {
        autocreate: true,
        retry_base_ms: 60_000,
        retry_max_ms: 60_000,
        delivery_timeout_ms: 2000,
        push_limits: limits,
        ..Options::default()
    }
}

async fn stats(t: &Eg, topic: &str, sub: &str) -> Value {
    t.get_admin(&format!("/topics/{topic}/subscriptions/{sub}")).await.json()["stats"].clone()
}

async fn subscribe_to_a_dead_webhook(t: &Eg, topic: &str, sub: &str) {
    t.put_json(&format!("/topics/{topic}"), &json!({})).await;
    let r = t
        .put_json(
            &format!("/topics/{topic}/subscriptions/{sub}"),
            &json!({"webhookUrl": "http://127.0.0.1:1/hook", "skipValidation": true}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
}

async fn dead_letters(t: &Eg, topic: &str, sub: &str) -> Vec<Value> {
    t.get_admin(&format!("/topics/{topic}/subscriptions/{sub}/deadletters")).await.json()["value"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn the_oldest_pending_deliveries_are_dead_lettered_when_a_subscription_is_full() {
    let t = Eg::with(stuck(PushLimits { per_subscription: 3, ..PushLimits::default() })).await;
    subscribe_to_a_dead_webhook(&t, "full", "s").await;
    for i in 0..8 {
        assert_eq!(t.publish_eg("full", json!([eg(&i.to_string(), "T", "s")])).await.status, 200);
    }
    let st = stats(&t, "full", "s").await;
    assert_eq!(st["pendingPush"], 3, "{st}");
    assert_eq!(st["shedPush"], 5, "{st}");
    assert_eq!(st["deadLettered"], 5, "{st}");
    // the oldest five (ids 0 to 4) went to the dead-letter list, the newest three still wait
    let dead = dead_letters(&t, "full", "s").await;
    let ids: Vec<&str> = dead.iter().map(|d| d["event"]["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["0", "1", "2", "3", "4"], "{dead:?}");
    assert_eq!(dead[0]["reason"], "PendingQueueFull");
    assert!(dead[0]["description"].as_str().unwrap().contains("retry queue"));
    // a shed event is not delivered later: it is not attempted again (only the three that wait keep retrying)
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = t.get_admin("/topics/full/subscriptions/s/deliveries").await.json();
    let shed_in_log = log["value"].as_array().unwrap().iter().filter(|a| a["outcome"] == "DeadLettered").count();
    assert_eq!(shed_in_log, 5);
}

#[tokio::test]
async fn bytes_are_limited_too() {
    let t = Eg::with(stuck(PushLimits { per_subscription_bytes: 100 * 1024, ..PushLimits::default() })).await;
    subscribe_to_a_dead_webhook(&t, "big", "s").await;
    let pad = "x".repeat(30 * 1024);
    for i in 0..10 {
        let mut ev = eg(&i.to_string(), "T", "s");
        ev["data"] = json!({"pad": pad});
        assert_eq!(t.publish_eg("big", json!([ev])).await.status, 200);
    }
    let st = stats(&t, "big", "s").await;
    // 30 KiB events, at most 100 KiB pending: three fit
    assert_eq!(st["pendingPush"], 3, "{st}");
    assert!(st["pendingPushBytes"].as_u64().unwrap() <= 100 * 1024, "{st}");
    assert_eq!(st["shedPush"], 7, "{st}");
}

#[tokio::test]
async fn the_total_over_all_subscriptions_is_limited_and_the_oldest_of_all_goes_first() {
    let t = Eg::with(stuck(PushLimits { total: 6, ..PushLimits::default() })).await;
    subscribe_to_a_dead_webhook(&t, "a", "s").await;
    subscribe_to_a_dead_webhook(&t, "b", "s").await;
    // alternate topics: a0 b0 a1 b1 a2 b2 a3 b3
    for i in 0..4 {
        t.publish_eg("a", json!([eg(&format!("a{i}"), "T", "s")])).await;
        t.publish_eg("b", json!([eg(&format!("b{i}"), "T", "s")])).await;
    }
    let (sa, sb) = (stats(&t, "a", "s").await, stats(&t, "b", "s").await);
    assert_eq!(sa["pendingPush"].as_u64().unwrap() + sb["pendingPush"].as_u64().unwrap(), 6, "{sa} {sb}");
    let da: Vec<String> = dead_letters(&t, "a", "s").await.iter().map(|d| d["event"]["id"].as_str().unwrap().to_string()).collect();
    let db: Vec<String> = dead_letters(&t, "b", "s").await.iter().map(|d| d["event"]["id"].as_str().unwrap().to_string()).collect();
    assert_eq!((da, db), (vec!["a0".to_string()], vec!["b0".to_string()]));
}

#[tokio::test]
async fn deliveries_that_finish_leave_the_pending_list() {
    // a webhook that answers 200: nothing stays pending and nothing is shed
    let t = Eg::with(Options { push_limits: PushLimits { per_subscription: 2, ..PushLimits::default() }, ..fast_options() }).await;
    let hook = Hook::ok().await;
    t.put_json("/topics/ok", &json!({})).await;
    t.put_json("/topics/ok/subscriptions/s", &json!({"webhookUrl": hook.url})).await;
    for i in 0..6 {
        t.publish_eg("ok", json!([eg(&i.to_string(), "T", "s")])).await;
        let n = i + 1;
        assert!(eventually(5.0, || (hook.notifications().len() >= n).then_some(())).await.is_some());
    }
    tokio::time::sleep(Duration::from_millis(100)).await; // let the last outcome be recorded
    let s = stats(&t, "ok", "s").await;
    assert_eq!(s["pendingPush"], 0, "{s}");
    assert_eq!(s["shedPush"], 0, "{s}");
    assert_eq!(s["delivered"], 6, "{s}");
}

#[tokio::test]
async fn a_failing_webhook_with_retries_dead_letters_by_attempts_and_frees_the_pending_entry() {
    let opts = Options { autocreate: true, retry_base_ms: 5, retry_max_ms: 10, delivery_timeout_ms: 1000, ..Options::default() };
    let t = Eg::with(opts).await;
    t.put_json("/topics/fail", &json!({})).await;
    t.put_json(
        "/topics/fail/subscriptions/s",
        &json!({"webhookUrl": "http://127.0.0.1:1/hook", "skipValidation": true, "retryPolicy": {"maxDeliveryAttempts": 2}}),
    )
    .await;
    t.publish_eg("fail", json!([eg("1", "T", "s")])).await;
    let mut st = Value::Null;
    for _ in 0..200 {
        st = stats(&t, "fail", "s").await;
        if st["deadLettered"] == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(st["deadLettered"], 1, "{st}");
    assert_eq!(st["pendingPush"], 0, "{st}");
    assert_eq!(st["shedPush"], 0, "{st}");
    assert_eq!(dead_letters(&t, "fail", "s").await[0]["reason"], "MaxDeliveryAttemptsExceeded");
}
