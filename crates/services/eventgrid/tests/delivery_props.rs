//! Delivery properties, dead letters written as blobs, Azure Function destinations and identities on destinations.
//! What Azure did (November 2026) is described in `delivery_props.rs`, `dead_letter.rs` and `docs/services/eventgrid.md`.

mod common;

use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_eventgrid::{DeadLetterBlob, DestinationDelivery, DestinationKind, EventGridService};
use common::*;
use parking_lot::Mutex;
use serde_json::{json, Value};

type Seen = Arc<Mutex<Vec<DestinationDelivery>>>;

async fn start() -> (Eg, Arc<EventGridService>) {
    let svc = Arc::new(EventGridService::with_options(fast_options()));
    let srv = TestServer::start(vec![svc.clone()]).await;
    (Eg { srv }, svc)
}

fn recorder(svc: &EventGridService, kind: DestinationKind) -> Seen {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    svc.register_destination(
        kind,
        Arc::new(move |d| {
            s2.lock().push(d.clone());
            Ok(())
        }),
    );
    seen
}

fn event(id: &str, subject: &str, data: Value) -> Value {
    json!({"id":id,"subject":subject,"eventType":"T","eventTime":"2024-05-23T22:06:35.487928Z","dataVersion":"1.0","data":data})
}

fn mappings() -> Value {
    json!([
        {"name":"x-static","type":"Static","properties":{"value":"abc","isSecret":false}},
        {"name":"x-secret","type":"Static","properties":{"value":"hush","isSecret":true}},
        {"name":"x-dyn","type":"Dynamic","properties":{"sourceField":"data.key1"}}])
}

#[tokio::test]
async fn webhook_headers_come_from_static_and_dynamic_mappings() {
    let (t, _svc) = start().await;
    let hook = Hook::ok().await;
    t.put_json("/topics/dp", &json!({})).await;
    let put = t
        .put_json(
            "/topics/dp/subscriptions/s",
            &json!({"destination":{"endpointType":"WebHook","properties":{"endpointUrl":hook.url,"deliveryAttributeMappings":mappings()}}}),
        )
        .await;
    assert_eq!(put.status, 201, "{}", put.text());
    t.publish_eg(
        "dp",
        json!([event("a", "s", json!({"key1":"v1"})), event("b", "s", json!({"other":1})), event("c", "s", json!({"key1":7}))]),
    )
    .await;
    let got = eventually(5.0, || (hook.notifications().len() == 3).then(|| hook.notifications())).await.expect("three deliveries");
    let by_id = |id: &str| got.iter().find(|r| r.json()[0]["id"] == id).unwrap().clone();
    let a = by_id("a");
    assert_eq!(a.header("x-static"), Some("abc"));
    assert_eq!(a.header("x-secret"), Some("hush"));
    assert_eq!(a.header("x-dyn"), Some("v1"));
    assert_eq!(a.header("aeg-event-type"), Some("Notification"));
    // a missing dynamic source leaves the header out and the delivery still happens
    let b = by_id("b");
    assert_eq!(b.header("x-dyn"), None);
    assert_eq!(b.header("x-static"), Some("abc"));
    assert_eq!(by_id("c").header("x-dyn"), Some("7"));

    // the subscription shows no secret value, and the mappings are in the flat form
    let v = t.get_admin("/topics/dp/subscriptions/s").await.json();
    let list = v["deliveryAttributeMappings"].as_array().unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0]["value"], "abc");
    assert!(list[1].get("value").is_none() && list[1]["isSecret"] == true);
    assert_eq!(list[2]["sourceField"], "data.key1");
    assert!(v["destination"].is_null(), "a webhook destination becomes webhookUrl: {v}");
}

#[tokio::test]
async fn service_bus_gets_system_properties_and_application_properties() {
    let (t, svc) = start().await;
    let seen = recorder(&svc, DestinationKind::ServiceBusQueue);
    t.put_json("/topics/sb", &json!({})).await;
    let r = t
        .put_json(
            "/topics/sb/subscriptions/s",
            &json!({"destination":{"endpointType":"ServiceBusQueue","namespace":"ns","queueName":"q","deliveryAttributeMappings":[
                {"name":"MessageId","type":"Dynamic","sourceField":"data.mid"},
                {"name":"sessionid","type":"Dynamic","sourceField":"data.sid"},
                {"name":"PartitionKey","type":"Static","value":"pk"},
                {"name":"custom","type":"Dynamic","sourceField":"data.c"}]}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    t.publish_eg("sb", json!([event("e1", "s", json!({"mid":"m-1","sid":"s-1","c":true})), event("e2", "s", json!({"sid":{"a":1}}))]))
        .await;
    let got = eventually(5.0, || {
        let v = seen.lock().clone();
        (v.len() == 2).then_some(v)
    })
    .await
    .expect("two messages");
    let one = got.iter().find(|d| d.event_id == "e1").unwrap();
    let sys = |d: &DestinationDelivery, n: &str| d.system.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert_eq!(sys(one, "MessageId").as_deref(), Some("m-1"));
    assert_eq!(sys(one, "SessionId").as_deref(), Some("s-1"));
    assert_eq!(sys(one, "PartitionKey").as_deref(), Some("pk"));
    assert!(one.properties.iter().any(|(k, v)| k == "custom" && v == "True"));
    assert!(!one.properties.iter().any(|(k, _)| k == "MessageId"), "a system property is not also an application property");
    assert!(one.properties.iter().any(|(k, _)| k == "aeg-subscription-name"));
    // an object is no value: only the static property is set
    let two = got.iter().find(|d| d.event_id == "e2").unwrap();
    assert_eq!(sys(two, "SessionId"), None);
    assert_eq!(sys(two, "MessageId"), None);
    assert_eq!(sys(two, "PartitionKey").as_deref(), Some("pk"));
}

#[tokio::test]
async fn event_hub_takes_the_partition_key_and_the_storage_queue_takes_no_properties() {
    let (t, svc) = start().await;
    let hub = recorder(&svc, DestinationKind::EventHub);
    let queue = recorder(&svc, DestinationKind::StorageQueue);
    t.put_json("/topics/eh", &json!({})).await;
    let maps = json!([{"name":"PartitionKey","type":"Dynamic","sourceField":"data.pk"},{"name":"MessageId","type":"Static","value":"m"},{"name":"x","type":"Static","value":"y"}]);
    for (name, dest) in [
        ("hub", json!({"endpointType":"EventHub","namespace":"n","eventHubName":"h","deliveryAttributeMappings":maps})),
        (
            "queue",
            json!({"endpointType":"StorageQueue","storageAccount":"a","queueName":"q","queueMessageTimeToLiveInSeconds":60,"deliveryAttributeMappings":maps}),
        ),
    ] {
        let r = t.put_json(&format!("/topics/eh/subscriptions/{name}"), &json!({ "destination": dest })).await;
        assert_eq!(r.status, 201, "{}", r.text());
    }
    t.publish_eg("eh", json!([event("e1", "s", json!({"pk":"key-1"}))])).await;
    let h = eventually(5.0, || hub.lock().first().cloned()).await.expect("event hub delivery");
    assert_eq!(h.system, vec![("PartitionKey".to_string(), "key-1".to_string())]);
    // MessageId is not an event hub property: it is an application property there
    assert!(h.properties.iter().any(|(k, v)| k == "MessageId" && v == "m"));
    assert!(h.properties.iter().any(|(k, v)| k == "x" && v == "y"));
    let q = eventually(5.0, || queue.lock().first().cloned()).await.expect("queue delivery");
    assert!(q.system.is_empty());
    assert!(!q.properties.iter().any(|(k, _)| k == "x"));
    assert_eq!(q.ttl_seconds, Some(60));
}

#[tokio::test]
async fn limits_and_reserved_names_are_refused() {
    let (t, svc) = start().await;
    recorder(&svc, DestinationKind::ServiceBusQueue);
    t.put_json("/topics/lim", &json!({})).await;
    let put = |maps: Value| {
        let t = &t;
        async move {
            t.put_json(
                "/topics/lim/subscriptions/s",
                &json!({"destination":{"endpointType":"ServiceBusQueue","namespace":"n","queueName":"q","deliveryAttributeMappings":maps}}),
            )
            .await
        }
    };
    let ten: Vec<Value> = (0..10).map(|i| json!({"name":format!("h{i}"),"type":"Static","value":"v"})).collect();
    assert!(put(Value::Array(ten.clone())).await.status < 300);
    let mut eleven = ten;
    eleven.push(json!({"name":"h10","type":"Static","value":"v"}));
    let r = put(Value::Array(eleven)).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("at most 10"), "{}", r.text());
    assert_eq!(put(json!([{"name":"aeg-mine","type":"Static","value":"v"}])).await.status, 400);
    for name in [
        "Host",
        "content-length",
        "Transfer-Encoding",
        "Connection",
        "Upgrade",
        "TE",
        "Trailer",
        "Expect",
        "Keep-Alive",
        "Proxy-Authorization",
    ] {
        let r = put(json!([{"name":name,"type":"Static","value":"v"}])).await;
        assert_eq!(r.status, 400, "{name}");
        assert!(r.text().contains("header of the connection"), "{name}: {}", r.text());
    }
    assert_eq!(put(json!([{"name":"a","type":"Static","value":"v".repeat(4097)}])).await.status, 400);
    assert_eq!(put(json!([{"name":"a","type":"Dynamic"}])).await.status, 400);
}

#[tokio::test]
async fn dead_letters_are_written_as_blobs() {
    let (t, svc) = start().await;
    let blobs: Arc<Mutex<Vec<DeadLetterBlob>>> = Arc::default();
    let b2 = blobs.clone();
    svc.register_dead_letter_writer(Arc::new(move |b| {
        b2.lock().push(b.clone());
        Ok(())
    }));
    let hook = scripted(vec![500]).await;
    t.put_json("/topics/Orders", &json!({})).await;
    let r = t
        .put_json(
            "/topics/Orders/subscriptions/Dead",
            &json!({"webhookUrl":hook.url,"retryPolicy":{"maxDeliveryAttempts":2},
                    "deadLetterDestination":{"endpointType":"StorageBlob","properties":{
                        "resourceId":"/subscriptions/s/resourceGroups/r/providers/Microsoft.Storage/storageAccounts/acct1","blobContainerName":"dl"}}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    t.publish_eg("Orders", json!([eg("doomed", "T.X", "dead/1")])).await;
    let blob = eventually(5.0, || blobs.lock().first().cloned()).await.expect("a dead-letter blob");
    assert_eq!((blob.account.as_str(), blob.container.as_str()), ("acct1", "dl"));
    assert_eq!(blob.content_type, "application/json");
    let parts: Vec<&str> = blob.name.split('/').collect();
    assert_eq!((parts[0], parts[1]), ("Orders", "DEAD"), "{}", blob.name);
    assert_eq!(parts.len(), 7);
    let body: Value = serde_json::from_slice(&blob.body).unwrap();
    let ev = &body[0];
    assert_eq!(ev["id"], "doomed");
    assert_eq!(ev["deadLetterReason"], "MaxDeliveryAttemptsExceeded");
    assert_eq!(ev["deliveryAttempts"], 2);
    assert_eq!(ev["lastDeliveryOutcome"], "Error");
    assert!(ev["publishTime"].as_str().unwrap().ends_with('Z') && ev["lastDeliveryAttemptTime"].as_str().unwrap().ends_with('Z'));
    assert_eq!(ev["data"]["n"], "doomed");
    // the emulator's own list keeps it too
    let dl = t.get_admin("/topics/Orders/subscriptions/Dead/deadletters").await.json();
    assert_eq!(dl["value"].as_array().unwrap().len(), 1);
    assert_eq!(blobs.lock().len(), 1);
}

#[tokio::test]
async fn a_blob_dead_letter_destination_needs_a_writer_and_a_container() {
    let (t, svc) = start().await;
    t.put_json("/topics/nw", &json!({})).await;
    let body = json!({"webhookUrl":"http://127.0.0.1:9/x","skipValidation":true,
        "deadLetterDestination":{"endpointType":"StorageBlob","storageAccount":"a","blobContainerName":"c"}});
    assert_eq!(t.put_json("/topics/nw/subscriptions/s", &body).await.status, 501);
    svc.register_dead_letter_writer(Arc::new(|_| Ok(())));
    assert_eq!(t.put_json("/topics/nw/subscriptions/s", &body).await.status, 201);
    let no_container = json!({"webhookUrl":"http://127.0.0.1:9/x","skipValidation":true,
        "deadLetterDestination":{"endpointType":"StorageBlob","storageAccount":"a"}});
    let r = t.put_json("/topics/nw/subscriptions/s2", &no_container).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("blobContainerName"));
    let wrong = json!({"webhookUrl":"http://127.0.0.1:9/x","skipValidation":true,"deadLetterDestination":{"endpointType":"Nope"}});
    assert_eq!(t.put_json("/topics/nw/subscriptions/s3", &wrong).await.status, 400);
}

#[tokio::test]
async fn identities_are_stored_shown_and_delivery_is_the_same() {
    let (t, svc) = start().await;
    let seen = recorder(&svc, DestinationKind::ServiceBusQueue);
    let blobs: Arc<Mutex<Vec<DeadLetterBlob>>> = Arc::default();
    let b2 = blobs.clone();
    svc.register_dead_letter_writer(Arc::new(move |b| {
        b2.lock().push(b.clone());
        Ok(())
    }));
    t.put_json("/topics/id", &json!({})).await;
    let r = t
        .put_json(
            "/topics/id/subscriptions/s",
            &json!({
                "deliveryWithResourceIdentity":{"identity":{"type":"SystemAssigned"},
                    "destination":{"endpointType":"ServiceBusQueue","properties":{
                        "resourceId":"/subscriptions/s/resourceGroups/r/providers/Microsoft.ServiceBus/namespaces/n/queues/q"}}},
                "deadLetterWithResourceIdentity":{"identity":{"type":"UserAssigned","userAssignedIdentity":"/subscriptions/s/resourceGroups/r/providers/Microsoft.ManagedIdentity/userAssignedIdentities/u"},
                    "deadLetterDestination":{"endpointType":"StorageBlob","properties":{"resourceId":"/subscriptions/s/resourceGroups/r/providers/Microsoft.Storage/storageAccounts/a","blobContainerName":"c"}}}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let v = r.json();
    assert_eq!(v["deliveryWithResourceIdentity"]["identity"]["type"], "SystemAssigned");
    assert_eq!(v["deliveryWithResourceIdentity"]["destination"]["endpointType"], "ServiceBusQueue");
    assert_eq!(v["deadLetterWithResourceIdentity"]["identity"]["type"], "UserAssigned");
    assert_eq!(v["deadLetterWithResourceIdentity"]["deadLetterDestination"]["endpointType"], "StorageBlob");
    t.publish_eg("id", json!([eg("e", "T", "s")])).await;
    let d = eventually(5.0, || seen.lock().first().cloned()).await.expect("delivery as for the plain form");
    assert_eq!((d.target.namespace.as_str(), d.target.entity.as_str()), ("n", "q"));
    assert!(blobs.lock().is_empty());

    for bad in [
        json!({"deliveryWithResourceIdentity":{"identity":{"type":"Nope"},"destination":{"endpointType":"EventHub","namespace":"n","eventHubName":"h"}}}),
        json!({"deliveryWithResourceIdentity":{"identity":{"type":"UserAssigned"},"destination":{"endpointType":"EventHub","namespace":"n","eventHubName":"h"}}}),
        json!({"deliveryWithResourceIdentity":{"identity":{"type":"SystemAssigned"}}}),
        json!({"destination":{"endpointType":"EventHub","namespace":"n","eventHubName":"h"},
               "deliveryWithResourceIdentity":{"identity":{"type":"SystemAssigned"},"destination":{"endpointType":"EventHub","namespace":"n","eventHubName":"h"}}}),
    ] {
        assert_eq!(t.put_json("/topics/id/subscriptions/bad", &bad).await.status, 400, "{bad}");
    }
}

#[tokio::test]
async fn azure_function_destination_goes_to_the_mapped_host() {
    let (t, _svc) = start().await;
    let host = Hook::ok().await;
    let base = host.url.trim_end_matches("/hook").to_string();
    t.put_json("/topics/fn", &json!({})).await;
    let sub = json!({"destination":{"endpointType":"AzureFunction","properties":{
        "resourceId":"/subscriptions/s/resourceGroups/r/providers/Microsoft.Web/sites/MyApp/functions/OnEvent",
        "deliveryAttributeMappings":[{"name":"x-static","type":"Static","properties":{"value":"abc"}}]}},
        "retryPolicy":{"maxDeliveryAttempts":2}});
    let r = t.put_json("/topics/fn/subscriptions/s", &sub).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["validation"]["status"], "NotRequired", "no handshake for a function");

    // no host is known yet: the delivery fails and says why
    t.publish_eg("fn", json!([event("e0", "s", json!({}))])).await;
    let log = eventually_async(&t, "/topics/fn/subscriptions/s/deadletters", |v| v["value"].as_array().is_some_and(|a| a.len() == 1)).await;
    assert_eq!(log["value"][0]["reason"], "MaxDeliveryAttemptsExceeded");
    let d = t.get_admin("/topics/fn/subscriptions/s/deliveries").await.json();
    let detail = d["value"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("MyApp") && detail.contains("AZEMU_EVENTGRID_FUNCTION_HOSTS"), "{detail}");

    // a host is set through the admin API (case does not matter)
    let put = t.put_json("/functions/myapp", &json!({"baseUrl": base})).await;
    assert_eq!(put.status, 201, "{}", put.text());
    assert_eq!(t.get_admin("/functions").await.json()["value"][0]["app"], "myapp");
    t.publish_eg("fn", json!([event("e1", "s", json!({}))])).await;
    let got = eventually(5.0, || host.notifications().into_iter().find(|r| r.json()[0]["id"] == "e1")).await.expect("delivery to the host");
    assert_eq!(got.method, "POST");
    assert_eq!(got.path, "/runtime/webhooks/EventGrid?functionName=OnEvent");
    assert_eq!(got.header("aeg-event-type"), Some("Notification"));
    assert_eq!(got.header("x-static"), Some("abc"));
    assert!(got.json().is_array());

    // an address that is not local is refused, as for a webhook
    let bad = t.put_json("/functions/other", &json!({"baseUrl":"http://example.com:7071"})).await;
    assert_eq!(bad.status, 400);
    assert!(bad.text().contains("AZEMU_ALLOW_WEBHOOK_HOST") || bad.text().contains("allow-webhook-host"), "{}", bad.text());
}

/// Poll an admin path until `ok` holds.
async fn eventually_async(t: &Eg, path: &str, ok: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..300 {
        let v = t.get_admin(path).await.json();
        if ok(&v) {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
    panic!("{path} never matched");
}
