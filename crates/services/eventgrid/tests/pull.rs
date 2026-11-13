mod common;

use std::time::{Duration, Instant};

use azemu_eventgrid::Options;
use common::*;
use serde_json::{json, Value};

const NS: &str = "ns1";

/// Create a namespace topic and a pull subscription through the admin API.
async fn setup(t: &Eg, topic: &str, sub: &str, cfg: Value) {
    let r = t.put_json(&format!("/topics/{topic}?namespace={NS}"), &json!({})).await;
    assert!(r.status == 201 || r.status == 200, "{}", r.text());
    let r = t.put_json(&format!("/topics/{topic}/subscriptions/{sub}?namespace={NS}"), &cfg).await;
    assert!(r.status == 201 || r.status == 200, "{}", r.text());
}

fn ids(r: &azemu_core::testing::TestResponse) -> Vec<String> {
    r.json()["value"].as_array().unwrap().iter().map(|v| v["event"]["id"].as_str().unwrap().to_string()).collect()
}

fn tokens(r: &azemu_core::testing::TestResponse) -> Vec<String> {
    r.json()["value"].as_array().unwrap().iter().map(|v| v["brokerProperties"]["lockToken"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn publish_receive_acknowledge_roundtrip() {
    let t = Eg::new().await;
    setup(&t, "jobs", "w", json!({})).await;
    let r = t.ns_publish(NS, "jobs", json!([ce("a", "x.y")])).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json(), json!({}));
    let r = t.receive(NS, "jobs", "w", "maxWaitTime=1").await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["value"][0]["event"]["id"], "a");
    assert_eq!(v["value"][0]["event"]["type"], "x.y");
    assert_eq!(v["value"][0]["event"]["data"]["n"], "a");
    assert_eq!(v["value"][0]["brokerProperties"]["deliveryCount"], 1);
    let tok = tokens(&r);
    let r = t.ns_op(NS, "jobs", "w", "acknowledge", "", json!({"lockTokens": tok})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["succeededLockTokens"], json!(tok));
    assert_eq!(r.json()["failedLockTokens"], json!([]));
    assert_eq!(r.header("api-supported-versions"), Some("2024-06-01"));
    // nothing left
    let r = t.receive(NS, "jobs", "w", "maxWaitTime=0").await;
    assert_eq!(r.json(), json!({"value": []}));
}

#[tokio::test]
async fn single_cloud_event_and_binary_mode_publish() {
    let t = Eg::new().await;
    setup(&t, "jobs", "w", json!({})).await;
    let url = format!("{}/topics/jobs:publish?api-version=2024-06-01", t.host_url(NS));
    let r = t
        .srv
        .request("POST", &url)
        .header(KEY.0, KEY.1)
        .header("Content-Type", "application/cloudevents+json; charset=utf-8")
        .body(ce("single", "t").to_string())
        .send()
        .await;
    assert_eq!(r.status, 200);
    let r = t
        .srv
        .request("POST", &url)
        .header(KEY.0, KEY.1)
        .header("ce-specversion", "1.0")
        .header("ce-id", "bin")
        .header("ce-source", "/s")
        .header("ce-type", "t.bin")
        .header("Content-Type", "application/json")
        .body(r#"{"k":"v"}"#)
        .send()
        .await;
    assert_eq!(r.status, 200);
    let r = t.receive(NS, "jobs", "w", "maxEvents=10&maxWaitTime=1").await;
    assert_eq!(ids(&r), vec!["single", "bin"]);
    assert_eq!(r.json()["value"][1]["event"]["data"], json!({"k":"v"}));
    assert_eq!(r.json()["value"][1]["event"]["datacontenttype"], "application/json");
}

#[tokio::test]
async fn publish_validation_and_content_types() {
    let t = Eg::new().await;
    let url = format!("{}/topics/jobs:publish?api-version=2024-06-01", t.host_url(NS));
    let send = |ct: &'static str, body: String| {
        let t = &t;
        let url = url.clone();
        async move { t.srv.request("POST", &url).header(KEY.0, KEY.1).header("Content-Type", ct).body(body).send().await }
    };
    assert_eq!(send("application/json", "[]".into()).await.status, 415);
    assert_eq!(send("application/cloudevents-batch+json", json!([{"id":"1"}]).to_string()).await.status, 400);
    assert_eq!(send("application/cloudevents-batch+json", "[]".into()).await.status, 400);
    assert_eq!(send("application/cloudevents+json", json!([ce("1", "t")]).to_string()).await.status, 400);
    assert_eq!(send("application/cloudevents-batch+json", "oops".into()).await.status, 400);
    // no credential
    let r = t.srv.request("POST", &url).header("Content-Type", "application/cloudevents+json").body(ce("1", "t").to_string()).send().await;
    assert_eq!(r.status, 401);
}

#[tokio::test]
async fn fifo_order_and_max_events() {
    let t = Eg::new().await;
    setup(&t, "q", "s", json!({})).await;
    t.ns_publish(NS, "q", json!([ce("1", "t"), ce("2", "t"), ce("3", "t"), ce("4", "t")])).await;
    // default maxEvents is 1
    assert_eq!(ids(&t.receive(NS, "q", "s", "maxWaitTime=1").await), vec!["1"]);
    assert_eq!(ids(&t.receive(NS, "q", "s", "maxEvents=2&maxWaitTime=1").await), vec!["2", "3"]);
    assert_eq!(ids(&t.receive(NS, "q", "s", "maxEvents=100&maxWaitTime=1").await), vec!["4"]);
    assert_eq!(t.receive(NS, "q", "s", "maxEvents=0").await.status, 400);
    assert_eq!(t.receive(NS, "q", "s", "maxEvents=101").await.status, 400);
    assert_eq!(t.receive(NS, "q", "s", "maxEvents=abc").await.status, 400);
    assert_eq!(t.receive(NS, "q", "s", "maxWaitTime=121").await.status, 400);
}

#[tokio::test]
async fn long_poll_returns_when_an_event_arrives() {
    let t = std::sync::Arc::new(Eg::new().await);
    setup(&t, "lp", "s", json!({})).await;
    let started = Instant::now();
    let t2 = t.clone();
    let rx = tokio::spawn(async move { t2.receive(NS, "lp", "s", "maxWaitTime=20").await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    t.ns_publish(NS, "lp", json!([ce("late", "t")])).await;
    let r = rx.await.unwrap();
    let took = started.elapsed();
    assert_eq!(ids(&r), vec!["late"]);
    assert!(took >= Duration::from_millis(250) && took < Duration::from_secs(5), "{took:?}");
}

#[tokio::test]
async fn long_poll_times_out_with_an_empty_list() {
    let t = Eg::new().await;
    setup(&t, "lp", "s", json!({})).await;
    let started = Instant::now();
    let r = t.receive(NS, "lp", "s", "maxWaitTime=1").await;
    let took = started.elapsed();
    assert_eq!(r.json(), json!({"value": []}));
    assert!(took >= Duration::from_millis(900) && took < Duration::from_secs(4), "{took:?}");
}

#[tokio::test]
async fn long_polls_do_not_block_other_requests() {
    let t = std::sync::Arc::new(Eg::new().await);
    setup(&t, "lp", "s", json!({})).await;
    let mut waiters = Vec::new();
    for _ in 0..8 {
        let t2 = t.clone();
        waiters.push(tokio::spawn(async move { t2.receive(NS, "lp", "s", "maxWaitTime=2").await }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    let r = t.get_admin("/topics").await;
    assert_eq!(r.status, 200);
    assert!(started.elapsed() < Duration::from_millis(500));
    // one event wakes exactly one of the waiters
    t.ns_publish(NS, "lp", json!([ce("only", "t")])).await;
    let mut got = 0;
    for w in waiters {
        if !ids(&w.await.unwrap()).is_empty() {
            got += 1;
        }
    }
    assert_eq!(got, 1);
}

#[tokio::test]
async fn release_makes_the_event_available_again_with_a_higher_delivery_count() {
    let t = Eg::new().await;
    setup(&t, "r", "s", json!({})).await;
    t.ns_publish(NS, "r", json!([ce("1", "t")])).await;
    let r = t.receive(NS, "r", "s", "maxWaitTime=1").await;
    let tok = tokens(&r);
    // locked: not visible to another consumer
    assert!(ids(&t.receive(NS, "r", "s", "maxWaitTime=0").await).is_empty());
    let r = t.ns_op(NS, "r", "s", "release", "", json!({"lockTokens": tok})).await;
    assert_eq!(r.json()["succeededLockTokens"], json!(tok));
    let r = t.receive(NS, "r", "s", "maxWaitTime=1").await;
    assert_eq!(r.json()["value"][0]["brokerProperties"]["deliveryCount"], 2);
    assert_ne!(tokens(&r), tok);
}

#[tokio::test]
async fn release_with_delay_hides_the_event_for_that_long() {
    let t = Eg::new().await;
    setup(&t, "r", "s", json!({})).await;
    t.ns_publish(NS, "r", json!([ce("1", "t")])).await;
    let tok = tokens(&t.receive(NS, "r", "s", "maxWaitTime=1").await);
    let r = t.ns_op(NS, "r", "s", "release", "releaseDelayInSeconds=1", json!({"lockTokens": tok})).await;
    assert_eq!(r.status, 200);
    let started = Instant::now();
    assert!(ids(&t.receive(NS, "r", "s", "maxWaitTime=0").await).is_empty());
    // a long poll sleeps until the delay is over
    let r = t.receive(NS, "r", "s", "maxWaitTime=10").await;
    assert_eq!(ids(&r), vec!["1"]);
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(900) && took < Duration::from_secs(5), "{took:?}");
    assert_eq!(t.ns_op(NS, "r", "s", "release", "releaseDelayInSeconds=banana", json!({"lockTokens": ["x"]})).await.status, 400);
}

#[tokio::test]
async fn reject_dead_letters_the_event() {
    let t = Eg::new().await;
    setup(&t, "d", "s", json!({})).await;
    t.ns_publish(NS, "d", json!([ce("1", "t")])).await;
    let tok = tokens(&t.receive(NS, "d", "s", "maxWaitTime=1").await);
    let r = t.ns_op(NS, "d", "s", "reject", "", json!({"lockTokens": tok})).await;
    assert_eq!(r.json()["succeededLockTokens"], json!(tok));
    assert!(ids(&t.receive(NS, "d", "s", "maxWaitTime=0").await).is_empty());
    let dl = t.get_admin(&format!("/topics/d/subscriptions/s/deadletters?namespace={NS}")).await.json();
    assert_eq!(dl["value"][0]["event"]["id"], "1");
    assert_eq!(dl["value"][0]["reason"], "Rejected");
}

#[tokio::test]
async fn lock_expiry_redelivers_with_increasing_count_and_old_token_is_lost() {
    let t = Eg::new().await;
    setup(&t, "x", "s", json!({"lockDurationSeconds": 0.4})).await;
    t.ns_publish(NS, "x", json!([ce("1", "t")])).await;
    let r1 = t.receive(NS, "x", "s", "maxWaitTime=1").await;
    let tok1 = tokens(&r1);
    // blocks until the lock expires, then gets the same event again
    let r2 = t.receive(NS, "x", "s", "maxWaitTime=10").await;
    assert_eq!(r2.json()["value"][0]["brokerProperties"]["deliveryCount"], 2);
    let tok2 = tokens(&r2);
    assert_ne!(tok1, tok2);
    // the first token is no longer valid
    let r = t.ns_op(NS, "x", "s", "acknowledge", "", json!({"lockTokens": [tok1[0], tok2[0]]})).await;
    let j = r.json();
    assert_eq!(j["succeededLockTokens"], json!([tok2[0]]));
    assert_eq!(j["failedLockTokens"][0]["lockToken"], tok1[0]);
    assert_eq!(j["failedLockTokens"][0]["error"]["code"], "TokenLost");
}

#[tokio::test]
async fn renew_lock_extends_the_lock() {
    let t = Eg::new().await;
    setup(&t, "x", "s", json!({"lockDurationSeconds": 0.6})).await;
    t.ns_publish(NS, "x", json!([ce("1", "t")])).await;
    let tok = tokens(&t.receive(NS, "x", "s", "maxWaitTime=1").await);
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(350)).await;
        let r = t.ns_op(NS, "x", "s", "renewLock", "", json!({"lockTokens": tok})).await;
        assert_eq!(r.json()["succeededLockTokens"], json!(tok));
    }
    // 1 s have passed, longer than one lock; the event is still ours
    assert!(ids(&t.receive(NS, "x", "s", "maxWaitTime=0").await).is_empty());
    let r = t.ns_op(NS, "x", "s", "acknowledge", "", json!({"lockTokens": tok})).await;
    assert_eq!(r.json()["succeededLockTokens"], json!(tok));
    // renewing a finished token fails
    let r = t.ns_op(NS, "x", "s", "renewLock", "", json!({"lockTokens": tok})).await;
    assert_eq!(r.json()["failedLockTokens"][0]["error"]["code"], "TokenLost");
}

#[tokio::test]
async fn bad_tokens_are_reported_per_token_not_as_an_http_error() {
    let t = Eg::new().await;
    setup(&t, "x", "s", json!({})).await;
    for op in ["acknowledge", "release", "reject", "renewLock"] {
        let r = t.ns_op(NS, "x", "s", op, "", json!({"lockTokens": ["CgMKATESCQoHdG9rZW4tMQ=="]})).await;
        assert_eq!(r.status, 200, "{op}");
        let j = r.json();
        assert_eq!(j["succeededLockTokens"], json!([]));
        assert_eq!(j["failedLockTokens"][0]["lockToken"], "CgMKATESCQoHdG9rZW4tMQ==");
        assert_eq!(j["failedLockTokens"][0]["error"]["code"], "BadToken", "{op}");
    }
    // malformed requests are HTTP 400
    assert_eq!(t.ns_op(NS, "x", "s", "acknowledge", "", json!({})).await.status, 400);
    assert_eq!(t.ns_op(NS, "x", "s", "acknowledge", "", json!({"lockTokens": []})).await.status, 400);
    assert_eq!(t.ns_op(NS, "x", "s", "acknowledge", "", json!({"lockTokens": [1]})).await.status, 400);
}

#[tokio::test]
async fn max_delivery_count_dead_letters_instead_of_redelivering() {
    let t = Eg::new().await;
    setup(&t, "m", "s", json!({"lockDurationSeconds": 0.2, "maxDeliveryCount": 2})).await;
    t.ns_publish(NS, "m", json!([ce("1", "t")])).await;
    let r = t.receive(NS, "m", "s", "maxWaitTime=1").await;
    assert_eq!(r.json()["value"][0]["brokerProperties"]["deliveryCount"], 1);
    let r = t.receive(NS, "m", "s", "maxWaitTime=5").await;
    assert_eq!(r.json()["value"][0]["brokerProperties"]["deliveryCount"], 2);
    // third time: the event is dead-lettered after the second lock expires
    let r = t.receive(NS, "m", "s", "maxWaitTime=1").await;
    assert_eq!(r.json(), json!({"value": []}));
    let dl = t.get_admin(&format!("/topics/m/subscriptions/s/deadletters?namespace={NS}")).await.json();
    assert_eq!(dl["value"].as_array().unwrap().len(), 1);
    assert_eq!(dl["value"][0]["reason"], "MaxDeliveryCountExceeded");
    assert_eq!(dl["value"][0]["deliveryCount"], 2);
    assert_eq!(dl["value"][0]["event"]["id"], "1");
    let sub = t.get_admin(&format!("/topics/m/subscriptions/s?namespace={NS}")).await.json();
    assert_eq!(sub["stats"]["deadLettered"], 1);
    assert_eq!(sub["stats"]["available"], 0);
}

#[tokio::test]
async fn event_time_to_live_expires_unreceived_events() {
    let t = Eg::new().await;
    setup(&t, "ttl", "s", json!({"eventTimeToLiveSeconds": 0.3})).await;
    t.ns_publish(NS, "ttl", json!([ce("1", "t")])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(ids(&t.receive(NS, "ttl", "s", "maxWaitTime=0").await).is_empty());
    let dl = t.get_admin(&format!("/topics/ttl/subscriptions/s/deadletters?namespace={NS}")).await.json();
    assert_eq!(dl["value"][0]["reason"], "TimeToLiveExpired");
}

#[tokio::test]
async fn subscriptions_only_get_events_published_after_they_exist_and_each_gets_a_copy() {
    let t = Eg::new().await;
    setup(&t, "fan", "first", json!({})).await;
    t.ns_publish(NS, "fan", json!([ce("before", "t")])).await;
    let r = t.put_json(&format!("/topics/fan/subscriptions/second?namespace={NS}"), &json!({})).await;
    assert_eq!(r.status, 201);
    t.ns_publish(NS, "fan", json!([ce("after", "t")])).await;
    assert_eq!(ids(&t.receive(NS, "fan", "first", "maxEvents=10&maxWaitTime=1").await), vec!["before", "after"]);
    assert_eq!(ids(&t.receive(NS, "fan", "second", "maxEvents=10&maxWaitTime=1").await), vec!["after"]);
}

#[tokio::test]
async fn filters_decide_what_a_subscription_receives() {
    let t = Eg::new().await;
    t.put_json(&format!("/topics/f?namespace={NS}"), &json!({})).await;
    let put = |name: &'static str, filter: Value| {
        let t = &t;
        async move {
            let r = t.put_json(&format!("/topics/f/subscriptions/{name}?namespace={NS}"), &json!({"filter": filter})).await;
            assert_eq!(r.status, 201, "{}", r.text());
        }
    };
    put("types", json!({"includedEventTypes": ["Order.Created"]})).await;
    put("subject", json!({"subjectBeginsWith": "/orders/eu", "subjectEndsWith": ".json"})).await;
    put(
        "adv",
        json!({"advancedFilters": [
        {"operatorType":"StringIn","key":"data.color","values":["red","BLUE"]},
        {"operatorType":"NumberGreaterThan","key":"data.qty","value":5}]}),
    )
    .await;
    let mk = |id: &str, ty: &str, subject: &str, color: &str, qty: i64| json!({"specversion":"1.0","id":id,"source":"/s","type":ty,"subject":subject,"data":{"color":color,"qty":qty}});
    t.ns_publish(
        NS,
        "f",
        json!([
            mk("e1", "Order.Created", "/orders/eu/1.json", "red", 10),
            mk("e2", "Order.Deleted", "/orders/us/2.json", "blue", 3),
            mk("e3", "Order.Created", "/orders/EU/3.json", "green", 9),
            mk("e4", "Other", "/x", "Blue", 6),
        ]),
    )
    .await;
    let got = |name: &'static str| {
        let t = &t;
        async move { ids(&t.receive(NS, "f", name, "maxEvents=100&maxWaitTime=0").await) }
    };
    assert_eq!(got("types").await, vec!["e1", "e3"]);
    assert_eq!(got("subject").await, vec!["e1", "e3"]); // subject match is case-insensitive by default
    assert_eq!(got("adv").await, vec!["e1", "e4"]);
}

#[tokio::test]
async fn autocreate_creates_topic_and_subscription_on_first_use() {
    let t = Eg::new().await;
    // receive creates the namespace topic and the subscription
    let r = t.receive("auto", "newtopic", "newsub", "maxWaitTime=0").await;
    assert_eq!(r.status, 200);
    let r = t.get_admin("/topics/newtopic/subscriptions/newsub?namespace=auto").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["deliveryMode"], "pull");
    t.ns_publish("auto", "newtopic", json!([ce("1", "t")])).await;
    assert_eq!(ids(&t.receive("auto", "newtopic", "newsub", "maxWaitTime=1").await), vec!["1"]);
}

#[tokio::test]
async fn autocreate_off_gives_the_not_found_errors() {
    let t = Eg::with(Options { autocreate: false, ..fast_options() }).await;
    let r = t.receive(NS, "nope", "s", "maxWaitTime=0").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["code"], "NotFound");
    assert_eq!(t.ns_publish(NS, "nope", json!([ce("1", "t")])).await.status, 404);
    t.put_json(&format!("/topics/nope?namespace={NS}"), &json!({})).await;
    let r = t.receive(NS, "nope", "nosub", "maxWaitTime=0").await;
    assert_eq!(r.status, 404);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("nosub"));
    assert_eq!(t.ns_op(NS, "nope", "nosub", "acknowledge", "", json!({"lockTokens": ["x"]})).await.status, 404);
    // publishing to a declared topic without subscriptions just drops the event
    assert_eq!(t.ns_publish(NS, "nope", json!([ce("1", "t")])).await.status, 200);
}

#[tokio::test]
async fn names_are_case_insensitive() {
    let t = Eg::new().await;
    setup(&t, "Jobs", "Worker", json!({})).await;
    t.ns_publish(NS, "JOBS", json!([ce("1", "t")])).await;
    assert_eq!(ids(&t.receive(NS, "jobs", "WORKER", "maxWaitTime=1").await), vec!["1"]);
}

#[tokio::test]
async fn reset_clears_queues_and_wakes_nothing_into_the_old_state() {
    let t = Eg::new().await;
    setup(&t, "r", "s", json!({})).await;
    t.ns_publish(NS, "r", json!([ce("1", "t")])).await;
    t.srv.reset();
    let r = t.receive(NS, "r", "s", "maxWaitTime=0").await;
    assert_eq!(r.json(), json!({"value": []}));
}
