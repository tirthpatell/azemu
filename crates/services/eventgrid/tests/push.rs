mod common;

use std::sync::Arc;
use std::time::Duration;

use azemu_core::testing::{EchoService, TestServer};
use azemu_eventgrid::{EventGridService, Options};
use common::*;
use serde_json::{json, Value};

async fn sub_json(t: &Eg, topic: &str, sub: &str) -> Value {
    t.get_admin(&format!("/topics/{topic}/subscriptions/{sub}")).await.json()
}

async fn deliveries(t: &Eg, topic: &str, sub: &str) -> Vec<Value> {
    t.get_admin(&format!("/topics/{topic}/subscriptions/{sub}/deliveries")).await.json()["value"].as_array().cloned().unwrap_or_default()
}

async fn wait_for_notifications(hook: &Hook, n: usize) -> Vec<Recorded> {
    let mut last = vec![];
    eventually(5.0, || {
        last = hook.notifications();
        (last.len() >= n).then(|| last.clone())
    })
    .await
    .unwrap_or(last)
}

#[tokio::test]
async fn event_grid_schema_handshake_and_delivery() {
    let t = Eg::new().await;
    let hook = Hook::ok().await;
    let r = t.put_json("/topics/orders", &json!({"inputSchema":"EventGridSchema"})).await;
    assert_eq!(r.status, 201);
    let r = t.put_json("/topics/orders/subscriptions/hook", &json!({"webhookUrl": hook.url})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let v = r.json();
    assert_eq!(v["deliveryMode"], "push");
    assert_eq!(v["validation"]["status"], "Succeeded");

    // the handshake request
    let seen = hook.seen.lock().clone();
    assert_eq!(seen.len(), 1);
    let hs = &seen[0];
    assert_eq!(hs.method, "POST");
    assert_eq!(hs.path, "/hook");
    assert_eq!(hs.header("aeg-event-type"), Some("SubscriptionValidation"));
    let ev = &hs.json()[0];
    assert_eq!(ev["eventType"], "Microsoft.EventGrid.SubscriptionValidationEvent");
    assert!(ev["data"]["validationCode"].as_str().unwrap().len() > 8);
    assert!(ev["data"]["validationUrl"].as_str().unwrap().contains("/_azemu/eventgrid/validate/"));
    assert!(ev["topic"].as_str().unwrap().ends_with("/topics/orders"));

    // a delivery
    assert_eq!(t.publish_eg("orders", json!([eg("evt-1", "Order.Created", "/orders/1")])).await.status, 200);
    let got = wait_for_notifications(&hook, 1).await;
    assert_eq!(got.len(), 1);
    let d = &got[0];
    assert_eq!(d.method, "POST");
    assert_eq!(d.header("aeg-event-type"), Some("Notification"));
    assert_eq!(d.header("aeg-subscription-name"), Some("HOOK"));
    assert_eq!(d.header("aeg-delivery-count"), Some("0"));
    assert_eq!(d.header("aeg-data-version"), Some("1.0"));
    assert_eq!(d.header("aeg-metadata-version"), Some("1"));
    assert!(d.header("content-type").unwrap().starts_with("application/json"));
    let body = d.json();
    assert!(body.is_array() && body.as_array().unwrap().len() == 1);
    assert_eq!(body[0]["id"], "evt-1");
    assert_eq!(body[0]["eventType"], "Order.Created");
    assert_eq!(body[0]["subject"], "/orders/1");
    assert_eq!(body[0]["dataVersion"], "1.0");
    assert_eq!(body[0]["metadataVersion"], "1");
    assert!(body[0]["topic"].as_str().unwrap().ends_with("/topics/orders"));
    assert_eq!(body[0]["data"]["n"], "evt-1");

    // the delivery log
    let l = deliveries(&t, "orders", "hook").await;
    assert_eq!(l.len(), 1);
    assert_eq!(l[0]["outcome"], "Success");
    assert_eq!(l[0]["statusCode"], 200);
    assert_eq!(l[0]["eventId"], "evt-1");
    assert_eq!(l[0]["attempt"], 1);
    let s = sub_json(&t, "orders", "hook").await;
    assert_eq!(s["stats"]["delivered"], 1);
}

#[tokio::test]
async fn cloud_events_subscription_uses_options_handshake_and_structured_mode() {
    let t = Eg::new().await;
    let hook = Hook::ok().await;
    t.put_json("/topics/ce", &json!({"inputSchema":"CloudEventSchemaV1_0"})).await;
    let r = t.put_json("/topics/ce/subscriptions/s", &json!({"webhookUrl": hook.url})).await;
    assert_eq!(r.json()["validation"]["status"], "Succeeded");
    let hs = hook.seen.lock()[0].clone();
    assert_eq!(hs.method, "OPTIONS");
    assert_eq!(hs.header("webhook-request-origin"), Some("eventgrid.azure.net"));
    assert!(hs.header("webhook-request-callback").is_some());
    assert!(hs.header("webhook-request-rate").is_some());

    t.publish_ce("ce", json!([ce("c1", "my.type")])).await;
    let got = wait_for_notifications(&hook, 1).await;
    let d = &got[0];
    assert!(d.header("content-type").unwrap().starts_with("application/cloudevents+json"));
    let body = d.json();
    assert!(body.is_object());
    assert_eq!(body["specversion"], "1.0");
    assert_eq!(body["id"], "c1");
    assert_eq!(body["type"], "my.type");
    assert_eq!(body["source"], "/test");
}

#[tokio::test]
async fn a_failed_handshake_is_recorded_and_blocks_delivery() {
    let t = Eg::new().await;
    let hook = Hook::start(|_, _| Reply::status(200)).await; // answers 200 without validationResponse
    t.put_json("/topics/hs", &json!({})).await;
    let r = t.put_json("/topics/hs/subscriptions/s", &json!({"webhookUrl": hook.url})).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["validation"]["status"], "AwaitingManualAction");
    t.publish_eg("hs", json!([eg("1", "T", "s")])).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(hook.notifications().is_empty());

    // a webhook that answers 500 to the handshake
    let bad = Hook::start(|_, _| Reply::status(500)).await;
    let r = t.put_json("/topics/hs/subscriptions/bad", &json!({"webhookUrl": bad.url})).await;
    assert_eq!(r.json()["validation"]["status"], "Failed");
    assert!(r.json()["validation"]["detail"].as_str().unwrap().contains("500"));
    // a webhook that is not listening
    let r = t.put_json("/topics/hs/subscriptions/down", &json!({"webhookUrl": "http://127.0.0.1:1/x"})).await;
    assert_eq!(r.json()["validation"]["status"], "Failed");
    t.publish_eg("hs", json!([eg("2", "T", "s")])).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(bad.notifications().is_empty());
}

#[tokio::test]
async fn manual_validation_through_the_validation_url() {
    let t = Eg::new().await;
    let hook = Hook::start(|_, _| Reply::status(200)).await;
    t.put_json("/topics/mv", &json!({})).await;
    let r = t.put_json("/topics/mv/subscriptions/s", &json!({"webhookUrl": hook.url})).await;
    assert_eq!(r.json()["validation"]["status"], "AwaitingManualAction");
    let url = hook.seen.lock()[0].json()[0]["data"]["validationUrl"].as_str().unwrap().to_string();
    // the URL points at the emulator the subscription was created through
    let r = t.srv.send("GET", &url, &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(sub_json(&t, "mv", "s").await["validation"]["status"], "Succeeded");
    t.publish_eg("mv", json!([eg("1", "T", "s")])).await;
    assert_eq!(wait_for_notifications(&hook, 1).await.len(), 1);
    // unknown code
    assert_eq!(t.srv.send("GET", &t.admin("/validate/nope"), &[], "").await.status, 404);
}

#[tokio::test]
async fn skip_validation_flag_and_revalidate() {
    let t = Eg::new().await;
    t.put_json("/topics/sv", &json!({})).await;
    // nothing listens here, but the handshake is skipped
    let r = t.put_json("/topics/sv/subscriptions/s", &json!({"webhookUrl": "http://127.0.0.1:1/x", "skipValidation": true})).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["validation"]["status"], "Skipped");
    // now run it for real against a live hook: update the subscription, then POST validate
    let hook = Hook::ok().await;
    let r = t.put_json("/topics/sv/subscriptions/s", &json!({"webhookUrl": hook.url, "skipValidation": true})).await;
    assert_eq!(r.status, 200);
    let r = t.srv.request("POST", &t.admin("/topics/sv/subscriptions/s/validate")).body("").send().await;
    assert_eq!(r.json()["validation"]["status"], "Succeeded");
}

#[tokio::test]
async fn retries_with_backoff_until_success() {
    let t = Eg::new().await;
    let hook = scripted(vec![500, 503, 200]).await;
    t.put_json("/topics/rt", &json!({})).await;
    t.put_json("/topics/rt/subscriptions/s", &json!({"webhookUrl": hook.url})).await;
    t.publish_eg("rt", json!([eg("1", "T", "s")])).await;
    let got = wait_for_notifications(&hook, 3).await;
    assert_eq!(got.len(), 3);
    let counts: Vec<_> = got.iter().map(|r| r.header("aeg-delivery-count").unwrap().to_string()).collect();
    assert_eq!(counts, vec!["0", "1", "2"]);
    // the same event each time
    assert!(got.iter().all(|r| r.json()[0]["id"] == "1"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let l = deliveries(&t, "rt", "s").await;
    let outcomes: Vec<_> = l.iter().map(|a| a["outcome"].as_str().unwrap().to_string()).collect();
    assert_eq!(outcomes, vec!["Failure", "Failure", "Success"]);
    assert_eq!(l[0]["statusCode"], 500);
    assert_eq!(l[1]["statusCode"], 503);
    let s = sub_json(&t, "rt", "s").await;
    assert_eq!(s["stats"]["delivered"], 1);
    assert_eq!(s["stats"]["failedAttempts"], 2);
}

#[tokio::test]
async fn dead_letter_after_the_maximum_attempts() {
    let t = Eg::new().await;
    let hook = scripted(vec![500]).await;
    t.put_json("/topics/dl", &json!({})).await;
    t.put_json("/topics/dl/subscriptions/s", &json!({"webhookUrl": hook.url, "retryPolicy": {"maxDeliveryAttempts": 3}})).await;
    t.publish_eg("dl", json!([eg("doomed", "T", "s")])).await;
    let ok = eventually(5.0, || {
        let n = hook.notifications().len();
        (n >= 3).then_some(n)
    })
    .await;
    assert_eq!(ok, Some(3));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hook.notifications().len(), 3, "no attempt after the maximum");
    let dl = t.get_admin("/topics/dl/subscriptions/s/deadletters").await.json();
    assert_eq!(dl["value"].as_array().unwrap().len(), 1);
    assert_eq!(dl["value"][0]["reason"], "MaxDeliveryAttemptsExceeded");
    assert_eq!(dl["value"][0]["deliveryCount"], 3);
    assert_eq!(dl["value"][0]["event"]["id"], "doomed");
    let l = deliveries(&t, "dl", "s").await;
    assert_eq!(l.last().unwrap()["outcome"], "DeadLettered");
}

#[tokio::test]
async fn a_400_answer_is_not_retried() {
    let t = Eg::new().await;
    let hook = scripted(vec![400]).await;
    t.put_json("/topics/nr", &json!({})).await;
    t.put_json("/topics/nr/subscriptions/s", &json!({"webhookUrl": hook.url})).await;
    t.publish_eg("nr", json!([eg("1", "T", "s")])).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(hook.notifications().len(), 1);
    let dl = t.get_admin("/topics/nr/subscriptions/s/deadletters").await.json();
    assert_eq!(dl["value"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_retry_intervals_and_time_to_live() {
    let t = Eg::new().await;
    let hook = scripted(vec![500]).await;
    t.put_json("/topics/ttl", &json!({})).await;
    t.put_json(
        "/topics/ttl/subscriptions/s",
        &json!({"webhookUrl": hook.url,
        "retryPolicy": {"retryIntervalsMs": [30], "eventTimeToLiveInMinutes": 0.005}}),
    )
    .await; // 0.3 s
    t.publish_eg("ttl", json!([eg("1", "T", "s")])).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    let n = hook.notifications().len();
    assert!((3..40).contains(&n), "{n}");
    let dead = t.get_admin("/topics/ttl/subscriptions/s/deadletters").await.json();
    assert_eq!(dead["value"][0]["reason"], "TimeToLiveExpired");
}

#[tokio::test]
async fn filters_apply_to_push_subscriptions() {
    let t = Eg::new().await;
    let hook = Hook::ok().await;
    t.put_json("/topics/pf", &json!({})).await;
    t.put_json(
        "/topics/pf/subscriptions/s",
        &json!({"webhookUrl": hook.url,
        "filter": {"includedEventTypes": ["Keep"], "subjectBeginsWith": "/a/",
                   "advancedFilters": [{"operatorType":"NumberLessThan","key":"data.n","value":2}]}}),
    )
    .await;
    let mk = |id: &str, ty: &str, subj: &str, n: i64| {
        let mut e = eg(id, ty, subj);
        e["data"] = json!({"n": n});
        e
    };
    t.publish_eg(
        "pf",
        json!([mk("1", "Keep", "/a/x", 1), mk("2", "Drop", "/a/x", 1), mk("3", "Keep", "/b/x", 1), mk("4", "Keep", "/a/x", 9)]),
    )
    .await;
    let got = wait_for_notifications(&hook, 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let got2 = hook.notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got2.len(), 1);
    assert_eq!(got2[0].json()[0]["id"], "1");
}

#[tokio::test]
async fn delivery_schema_conversion_event_grid_to_cloud_events() {
    let t = Eg::new().await;
    let hook = Hook::ok().await;
    t.put_json("/topics/conv", &json!({"inputSchema":"EventGridSchema"})).await;
    t.put_json("/topics/conv/subscriptions/s", &json!({"webhookUrl": hook.url, "deliverySchema": "CloudEventSchemaV1_0"})).await;
    // CloudEvents handshake because of the delivery schema
    assert_eq!(hook.seen.lock()[0].method, "OPTIONS");
    t.publish_eg("conv", json!([eg("1", "Some.Type", "/subj")])).await;
    let got = wait_for_notifications(&hook, 1).await;
    let b = got[0].json();
    assert_eq!(b["specversion"], "1.0");
    assert_eq!(b["type"], "Some.Type");
    assert_eq!(b["subject"], "/subj");
    assert_eq!(b["id"], "1");
    assert_eq!(b["data"]["n"], "1");
    assert!(got[0].header("content-type").unwrap().starts_with("application/cloudevents+json"));
}

#[tokio::test]
async fn custom_schema_events_are_passed_through() {
    let t = Eg::new().await;
    let hook = Hook::ok().await;
    t.put_json("/topics/cus", &json!({"inputSchema":"CustomEventSchema"})).await;
    t.put_json("/topics/cus/subscriptions/s", &json!({"webhookUrl": hook.url})).await;
    t.publish_eg("cus", json!([{"customId":"9","customData":{"a":1}}])).await;
    let got = wait_for_notifications(&hook, 1).await;
    assert_eq!(got[0].json(), json!([{"customId":"9","customData":{"a":1}}]));
}

#[tokio::test]
async fn domain_subscriptions_at_both_levels() {
    let t = Eg::new().await;
    let all = Hook::ok().await;
    let one = Hook::ok().await;
    t.put_json("/topics/dom", &json!({"kind":"domain"})).await;
    t.put_json("/topics/acme?domain=dom", &json!({})).await;
    assert_eq!(t.put_json("/topics/dom/subscriptions/everything", &json!({"webhookUrl": all.url})).await.status, 201);
    assert_eq!(t.put_json("/topics/acme/subscriptions/acme-only?domain=dom", &json!({"webhookUrl": one.url})).await.status, 201);
    let mk = |id: &str, topic: &str| {
        let mut e = eg(id, "T", "s");
        e["topic"] = json!(topic);
        e
    };
    t.publish_eg("dom", json!([mk("1", "acme"), mk("2", "other")])).await;
    assert_eq!(wait_for_notifications(&all, 2).await.len(), 2);
    let ones = wait_for_notifications(&one, 1).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(one.notifications().len(), 1);
    assert_eq!(ones[0].json()[0]["id"], "1");
    assert!(ones[0].json()[0]["topic"].as_str().unwrap().ends_with("/domains/dom/topics/acme"));
}

#[tokio::test]
async fn namespace_topic_push_subscription_gets_cloud_events() {
    let t = Eg::new().await;
    let hook = Hook::ok().await;
    t.put_json("/topics/jobs?namespace=nsp", &json!({})).await;
    let r = t.put_json("/topics/jobs/subscriptions/hook?namespace=nsp", &json!({"webhookUrl": hook.url})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    t.ns_publish("nsp", "jobs", json!([ce("p1", "job.created")])).await;
    let got = wait_for_notifications(&hook, 1).await;
    assert_eq!(got[0].json()["id"], "p1");
    // receive is not available on a push subscription
    let r = t.receive("nsp", "jobs", "hook", "maxWaitTime=0").await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn deleting_a_subscription_stops_retries() {
    let t = Eg::new().await;
    let hook = scripted(vec![500]).await;
    t.put_json("/topics/del", &json!({})).await;
    t.put_json("/topics/del/subscriptions/s", &json!({"webhookUrl": hook.url, "retryPolicy": {"retryIntervalsMs": [50]}})).await;
    t.publish_eg("del", json!([eg("1", "T", "s")])).await;
    wait_for_notifications(&hook, 2).await;
    assert_eq!(t.srv.request("DELETE", &t.admin("/topics/del/subscriptions/s")).send().await.status, 204);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let n = hook.notifications().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hook.notifications().len(), n);
}

#[tokio::test]
async fn reset_stops_pending_deliveries() {
    let t = Eg::new().await;
    let hook = scripted(vec![500]).await;
    t.put_json("/topics/rs", &json!({})).await;
    t.put_json("/topics/rs/subscriptions/s", &json!({"webhookUrl": hook.url, "retryPolicy": {"retryIntervalsMs": [50]}})).await;
    t.publish_eg("rs", json!([eg("1", "T", "s")])).await;
    wait_for_notifications(&hook, 2).await;
    t.srv.reset();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let n = hook.notifications().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hook.notifications().len(), n);
}

#[tokio::test]
async fn deliveries_are_made_while_other_requests_are_served() {
    // a webhook that answers slowly must not hold up publishing
    let t = Eg::new().await;
    let hook = Hook::start(|rec, _| {
        let mut r = default_reply(rec);
        if rec.header("aeg-event-type") == Some("Notification") {
            r.delay_ms = 400;
        }
        r
    })
    .await;
    t.put_json("/topics/slow", &json!({})).await;
    t.put_json("/topics/slow/subscriptions/s", &json!({"webhookUrl": hook.url})).await;
    let started = std::time::Instant::now();
    for i in 0..5 {
        assert_eq!(t.publish_eg("slow", json!([eg(&i.to_string(), "T", "s")])).await.status, 200);
    }
    assert!(started.elapsed() < Duration::from_millis(300), "{:?}", started.elapsed());
    assert_eq!(wait_for_notifications(&hook, 5).await.len(), 5);
}

#[tokio::test]
async fn https_webhooks_work_without_certificate_checks() {
    // The webhook is another azemu listener: it answers TLS on the same port.
    let svc = Arc::new(EventGridService::with_options(fast_options()));
    let srv = TestServer::start(vec![svc, Arc::new(EchoService::new())]).await;
    let t = Eg { srv };
    // the echo service answers every request with 200; its mount is `{x}.echo.localhost`,
    // and the HTTP client of the emulator sends the Host header of the URL, so use a
    // host name that routes (`localhost` resolves to the loopback)
    let url = format!("https://hook.echo.localhost:{}/hook", t.srv.port());
    t.put_json("/topics/tls", &json!({})).await;
    let r = t.put_json("/topics/tls/subscriptions/s", &json!({"webhookUrl": url, "skipValidation": true})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    t.publish_eg("tls", json!([eg("1", "T", "s")])).await;
    let mut outcome = Value::Null;
    for _ in 0..100 {
        let l = deliveries(&t, "tls", "s").await;
        if let Some(a) = l.last() {
            outcome = a.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // Resolving `*.localhost` needs the system resolver; where it cannot, the log says so.
    let detail = outcome["detail"].as_str().unwrap_or("");
    if detail.contains("connect failed") {
        eprintln!("skipping https assertion: {detail}");
        return;
    }
    assert_eq!(outcome["outcome"], "Success", "{outcome}");
    assert_eq!(outcome["statusCode"], 200);
}

#[tokio::test]
async fn publish_system_event_reaches_subscriptions() {
    let svc = Arc::new(EventGridService::with_options(fast_options()));
    let srv = TestServer::start(vec![svc.clone()]).await;
    let t = Eg { srv };
    let hook = Hook::ok().await;
    t.put_json("/topics/sys", &json!({})).await;
    t.put_json(
        "/topics/sys/subscriptions/s",
        &json!({"webhookUrl": hook.url, "filter": {"includedEventTypes": ["Microsoft.Storage.BlobCreated"]}}),
    )
    .await;
    let out = svc.publish_system_event(
        "sys",
        json!({"subject": "/blobServices/default/containers/c/blobs/b.txt", "eventType": "Microsoft.Storage.BlobCreated", "data": {"api": "PutBlob"}}),
    );
    assert!(out.accepted);
    assert_eq!(out.matched_subscriptions, 1);
    let out = svc.publish_system_event("sys", json!({"eventType": "Microsoft.Storage.BlobDeleted"}));
    assert_eq!(out.matched_subscriptions, 0);
    let got = wait_for_notifications(&hook, 1).await;
    let e = &got[0].json()[0];
    assert_eq!(e["eventType"], "Microsoft.Storage.BlobCreated");
    assert_eq!(e["data"]["api"], "PutBlob");
    assert!(e["id"].as_str().unwrap().len() > 8);
    assert!(e["eventTime"].is_string());
    // an unknown topic is created
    assert!(svc.publish_system_event("fresh", json!({"eventType": "x"})).accepted);
    // not an object
    assert!(!svc.publish_system_event("sys", json!("nope")).accepted);
}

#[tokio::test]
async fn config_file_declares_topics_and_survives_reset() {
    let dir = std::env::temp_dir().join(format!("azemu-eg-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("eventgrid.json");
    std::fs::write(
        &path,
        r#"{"topics":[{"name":"wired","inputSchema":"EventGridSchema",
              "subscriptions":[{"name":"hook","webhookUrl":"http://127.0.0.1:1/never","filter":{"includedEventTypes":["A"]}}]}],
            "namespaces":[{"name":"cfgns","topics":[{"name":"jobs","subscriptions":[{"name":"w","lockDurationSeconds":7,"maxDeliveryCount":3}]}]}]}"#,
    )
    .unwrap();
    let t = Eg::with(Options { config_path: Some(path.clone()), ..fast_options() }).await;
    for round in 0..2 {
        let r = t.get_admin("/topics/wired/subscriptions/hook").await;
        assert_eq!(r.status, 200, "round {round}");
        let v = r.json();
        assert_eq!(v["webhookUrl"], "http://127.0.0.1:1/never");
        assert_eq!(v["validation"]["status"], "Skipped");
        assert_eq!(v["filter"]["includedEventTypes"], json!(["A"]));
        let r = t.get_admin("/topics/jobs/subscriptions/w?namespace=cfgns").await.json();
        assert_eq!(r["lockDurationSeconds"], 7.0);
        assert_eq!(r["maxDeliveryCount"], 3);
        // a topic declared in the file with autocreate on still takes events
        t.publish_eg("wired", json!([eg("1", "B", "s")])).await;
        // state changes are dropped by reset, the file is loaded again
        t.put_json("/topics/extra", &json!({})).await;
        t.srv.reset();
        assert_eq!(t.get_admin("/topics/extra").await.status, 404);
    }
    std::fs::remove_dir_all(&dir).ok();
}
