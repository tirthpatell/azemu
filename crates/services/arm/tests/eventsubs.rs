//! Event subscriptions through ARM with the real wiring (`azemu::full_stack()`): a blob written through the Blob API
//! raises an event that arrives in a Storage queue made through ARM.

mod common;

use base64::Engine;
use common::{full, Arm, SUB};
use serde_json::{json, Value};

fn rid(rg: &str, ns: &str, rest: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/{rg}/providers/{ns}/{rest}")
}

async fn account_with_queue(a: &Arm, rg: &str, acct: &str) {
    a.group(rg).await;
    let r = a
        .put(
            &rid(rg, "Microsoft.Storage", &format!("storageAccounts/{acct}")),
            json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = a
        .put(
            &rid(rg, "Microsoft.Storage", &format!("storageAccounts/{acct}/queueServices/default/queues/events")),
            json!({"properties": {}}),
        )
        .await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text());
    let r = a
        .put(
            &rid(rg, "Microsoft.Storage", &format!("storageAccounts/{acct}/blobServices/default/containers/inbox")),
            json!({"properties": {}}),
        )
        .await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text());
}

fn queue_destination(rg: &str, acct: &str) -> Value {
    json!({"endpointType": "StorageQueue", "properties": {"resourceId": rid(rg, "Microsoft.Storage", &format!("storageAccounts/{acct}")), "queueName": "events"}})
}

async fn upload(a: &Arm, acct: &str, blob: &str) {
    let url = format!("{}/inbox/{blob}", a.dp("blob", acct));
    let r = a.dp_send("PUT", &url, &[("x-ms-blob-type", "BlockBlob")], "hello").await;
    assert_eq!(r.status, 201, "{}", r.text());
}

/// The decoded events waiting in the queue `events` of the account.
async fn queued(a: &Arm, acct: &str) -> Vec<Value> {
    let url = format!("{}/events/messages?numofmessages=32", a.dp("queue", acct));
    let r = a.dp_send("GET", &url, &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let xml = r.text();
    let mut out = Vec::new();
    for part in xml.split("<MessageText>").skip(1) {
        let text = part.split("</MessageText>").next().unwrap_or("");
        let raw = base64::engine::general_purpose::STANDARD.decode(text).expect("the event is base64");
        out.push(serde_json::from_slice(&raw).expect("the event is JSON"));
    }
    out
}

async fn wait_for_events(a: &Arm, acct: &str, n: usize) -> Vec<Value> {
    for _ in 0..100 {
        let got = queued(a, acct).await;
        if got.len() >= n {
            return got;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no event arrived");
}

#[tokio::test]
async fn blob_created_event_reaches_the_queue() {
    let a = full().await;
    account_with_queue(&a, "ev", "evacct1").await;
    let acct_id = rid("ev", "Microsoft.Storage", "storageAccounts/evacct1");
    let sub = format!("{acct_id}/providers/Microsoft.EventGrid/eventSubscriptions/to-queue");
    let r = a.put(&sub, json!({"properties": {"destination": queue_destination("ev", "evacct1"), "filter": {"includedEventTypes": ["Microsoft.Storage.BlobCreated"]}}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let j = r.json();
    assert_eq!(j["type"], "Microsoft.EventGrid/eventSubscriptions");
    assert_eq!(j["properties"]["topic"], acct_id);
    assert_eq!(j["properties"]["provisioningState"], "Succeeded");
    assert_eq!(j["properties"]["eventDeliverySchema"], "EventGridSchema");
    assert_eq!(j["properties"]["retryPolicy"]["maxDeliveryAttempts"], 30);

    upload(&a, "evacct1", "a.txt").await;
    let events = wait_for_events(&a, "evacct1", 1).await;
    assert_eq!(events[0]["eventType"], "Microsoft.Storage.BlobCreated");
    assert_eq!(events[0]["topic"], acct_id);
    assert_eq!(events[0]["subject"], "/blobServices/default/containers/inbox/blobs/a.txt");
}

#[tokio::test]
async fn deleting_the_subscription_or_the_group_stops_delivery() {
    let a = full().await;
    account_with_queue(&a, "ev2", "evacct2").await;
    let acct_id = rid("ev2", "Microsoft.Storage", "storageAccounts/evacct2");
    let sub = format!("{acct_id}/providers/Microsoft.EventGrid/eventSubscriptions/gone");
    assert_eq!(a.put(&sub, json!({"properties": {"destination": queue_destination("ev2", "evacct2")}})).await.status, 201);
    let list = a.get(&format!("{acct_id}/providers/Microsoft.EventGrid/eventSubscriptions")).await.json();
    assert_eq!(list["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(a.delete(&sub).await.status, 200);
    assert_eq!(a.get(&sub).await.status, 404);
    assert_eq!(a.delete(&sub).await.status, 204);
    upload(&a, "evacct2", "b.txt").await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(queued(&a, "evacct2").await.is_empty());

    // the group takes the subscription with it
    assert_eq!(a.put(&sub, json!({"properties": {"destination": queue_destination("ev2", "evacct2")}})).await.status, 201);
    let admin = format!("http://localhost:{}/_azemu/eventgrid/sources/storage/evacct2/subscriptions", a.srv.port());
    assert_eq!(a.dp_send("GET", &admin, &[], "").await.json()["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(a.delete(&format!("/subscriptions/{SUB}/resourcegroups/ev2")).await.status, 202);
    assert_eq!(a.dp_send("GET", &admin, &[], "").await.json()["value"].as_array().map(Vec::len), Some(0));
    assert_eq!(a.get(&sub).await.status, 404);
}

#[tokio::test]
async fn system_topic_and_its_subscription() {
    let a = full().await;
    account_with_queue(&a, "ev3", "evacct3").await;
    let acct_id = rid("ev3", "Microsoft.Storage", "storageAccounts/evacct3");
    let st = rid("ev3", "Microsoft.EventGrid", "systemTopics/st-one");
    // a wrong topic type, a missing source
    let bad = a.put(&st, json!({"location": "global", "properties": {"source": acct_id, "topicType": "Microsoft.KeyVault.vaults"}})).await;
    assert_eq!(bad.status, 400, "{}", bad.text());
    assert_eq!(
        a.put(&st, json!({"location": "global", "properties": {"topicType": "Microsoft.Storage.StorageAccounts"}})).await.status,
        400
    );
    let r = a
        .put(&st, json!({"location": "global", "properties": {"source": acct_id, "topicType": "Microsoft.Storage.StorageAccounts"}}))
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["provisioningState"], "Succeeded");
    assert_eq!(a.get(&st).await.json()["properties"]["source"], acct_id);
    let sub = format!("{st}/eventSubscriptions/s-one");
    let r = a.put(&sub, json!({"properties": {"destination": queue_destination("ev3", "evacct3")}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["type"], "Microsoft.EventGrid/systemTopics/eventSubscriptions");
    assert_eq!(a.get(&format!("{st}/eventSubscriptions")).await.json()["value"].as_array().map(Vec::len), Some(1));
    upload(&a, "evacct3", "c.txt").await;
    let ev = wait_for_events(&a, "evacct3", 1).await;
    assert_eq!(ev[0]["topic"], acct_id);
    // deleting the system topic removes its subscriptions
    assert_eq!(a.delete(&st).await.status, 200);
    assert_eq!(a.get(&sub).await.status, 404);
}

#[tokio::test]
async fn subscriptions_at_subscription_scope_are_not_built() {
    let a = full().await;
    account_with_queue(&a, "ev4", "evacct4").await;
    let r = a
        .put(
            &format!("/subscriptions/{SUB}/providers/Microsoft.EventGrid/eventSubscriptions/arm-events"),
            json!({"properties": {"destination": queue_destination("ev4", "evacct4")}}),
        )
        .await;
    assert_eq!(r.status, 501, "{}", r.text());
    assert!(r.text().contains("Resource Manager events"));
    // a resource type nothing raises events for
    a.put(&rid("ev4", "Microsoft.Compute", "virtualMachines/vm1"), json!({"location": "eastus", "properties": {}})).await;
    let r = a
        .put(
            &format!(
                "{}/providers/Microsoft.EventGrid/eventSubscriptions/vm-events",
                rid("ev4", "Microsoft.Compute", "virtualMachines/vm1")
            ),
            json!({"properties": {"destination": queue_destination("ev4", "evacct4")}}),
        )
        .await;
    assert_eq!(r.status, 501, "{}", r.text());
    // the resource group listing does not show subscriptions as resources
    let listed = a.get(&format!("/subscriptions/{SUB}/resourceGroups/ev4/resources")).await.json();
    assert!(listed["value"].as_array().unwrap().iter().all(|r| !r["type"].as_str().unwrap_or("").contains("eventSubscriptions")));
}
