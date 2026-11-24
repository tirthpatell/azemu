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

/// The events in the queue without taking them (`peekonly`): they stay visible.
async fn peeked(a: &Arm, acct: &str) -> Vec<Value> {
    let url = format!("{}/events/messages?peekonly=true&numofmessages=32", a.dp("queue", acct));
    let r = a.dp_send("GET", &url, &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.text()
        .split("<MessageText>")
        .skip(1)
        .map(|part| {
            let text = part.split("</MessageText>").next().unwrap_or("");
            let raw = base64::engine::general_purpose::STANDARD.decode(text).expect("the event is base64");
            serde_json::from_slice(&raw).expect("the event is JSON")
        })
        .collect()
}

/// What the account's container `name` holds, as the blob names listed.
async fn blob_names(a: &Arm, acct: &str, container: &str) -> Vec<String> {
    let url = format!("{}/{container}?restype=container&comp=list", a.dp("blob", acct));
    let r = a.dp_send("GET", &url, &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.text().split("<Name>").skip(1).map(|p| p.split("</Name>").next().unwrap_or("").to_string()).collect()
}

#[tokio::test]
async fn delivery_properties_dead_letter_blob_and_identity_through_arm() {
    let a = full().await;
    account_with_queue(&a, "ev5", "evacct5").await;
    let acct_id = rid("ev5", "Microsoft.Storage", "storageAccounts/evacct5");
    let r = a
        .put(
            &rid("ev5", "Microsoft.Storage", "storageAccounts/evacct5/blobServices/default/containers/deadletters"),
            json!({"properties": {}}),
        )
        .await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text());
    let base = format!("{acct_id}/providers/Microsoft.EventGrid/eventSubscriptions");

    // A subscription whose queue does not exist, with delivery properties and a blob dead-letter container.
    let missing = json!({"endpointType": "StorageQueue", "properties": {
        "resourceId": acct_id, "queueName": "no-such-queue",
        "deliveryAttributeMappings": [
            {"name": "x-static", "type": "Static", "properties": {"value": "abc", "isSecret": false}},
            {"name": "x-secret", "type": "Static", "properties": {"value": "hush", "isSecret": true}},
            {"name": "x-dyn", "type": "Dynamic", "properties": {"sourceField": "data.key1"}}]}});
    let dead = json!({"endpointType": "StorageBlob", "properties": {"resourceId": acct_id, "blobContainerName": "deadletters"}});
    let r = a
        .put(
            &format!("{base}/dead"),
            json!({"properties": {"destination": missing, "deadLetterDestination": dead, "retryPolicy": {"maxDeliveryAttempts": 1}}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let p = &r.json()["properties"];
    assert_eq!(p["deadLetterDestination"]["endpointType"], "StorageBlob");
    assert_eq!(p["deadLetterDestination"]["properties"]["blobContainerName"], "deadletters");
    let maps = &p["destination"]["properties"]["deliveryAttributeMappings"];
    assert_eq!(maps[0]["properties"]["value"], "abc");
    assert!(maps[1]["properties"].get("value").is_none(), "a secret value is not shown: {maps}");
    assert_eq!(maps[2]["properties"]["sourceField"], "data.key1");
    // the secret is given by getDeliveryAttributes
    let got = a.post(&format!("{base}/dead/getDeliveryAttributes"), json!({})).await;
    assert_eq!(got.status, 200, "{}", got.text());
    assert_eq!(got.json()["value"][1]["properties"]["value"], "hush");

    // A second subscription on the same account, to a queue that exists: it must see the upload and not the dead letter blob.
    let r = a
        .put(
            &format!("{base}/watch"),
            json!({"properties": {"destination": queue_destination("ev5", "evacct5"), "filter": {"includedEventTypes": ["Microsoft.Storage.BlobCreated"]}}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    upload(&a, "evacct5", "a.txt").await;
    let mut names = Vec::new();
    for _ in 0..100 {
        names = blob_names(&a, "evacct5", "deadletters").await;
        if !names.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(names.len(), 1, "one dead-letter blob: {names:?}");
    let parts: Vec<&str> = names[0].split('/').collect();
    assert_eq!(parts[1], "DEAD", "{}", names[0]);
    assert_eq!(parts.len(), 7);
    let body = a.dp_send("GET", &format!("{}/deadletters/{}", a.dp("blob", "evacct5"), names[0]), &[], "").await;
    assert_eq!(body.status, 200);
    let v = body.json();
    assert_eq!(v[0]["deadLetterReason"], "MaxDeliveryAttemptsExceeded");
    assert_eq!(v[0]["deliveryAttempts"], 1);
    assert_eq!(v[0]["lastDeliveryOutcome"], "NotFound");
    assert_eq!(v[0]["subject"], "/blobServices/default/containers/inbox/blobs/a.txt");
    // the watcher got exactly the upload: writing the dead letter raised no blob event
    for _ in 0..100 {
        if !peeked(&a, "evacct5").await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let ev = peeked(&a, "evacct5").await;
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0]["subject"], "/blobServices/default/containers/inbox/blobs/a.txt");

    // The same subscription, delivering with a managed identity: accepted, read back as written, delivered as before.
    let r = a
        .put(
            &format!("{base}/ident"),
            json!({"properties": {
                "deliveryWithResourceIdentity": {"identity": {"type": "SystemAssigned"}, "destination": queue_destination("ev5", "evacct5")},
                "deadLetterWithResourceIdentity": {"identity": {"type": "SystemAssigned"}, "deadLetterDestination": dead},
                "filter": {"subjectEndsWith": "id.txt"}}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let p = &r.json()["properties"];
    assert_eq!(p["deliveryWithResourceIdentity"]["identity"]["type"], "SystemAssigned");
    assert_eq!(p["deliveryWithResourceIdentity"]["destination"]["endpointType"], "StorageQueue");
    assert_eq!(p["deadLetterWithResourceIdentity"]["deadLetterDestination"]["properties"]["blobContainerName"], "deadletters");
    assert!(p.get("destination").is_none());
    assert_eq!(
        a.get(&format!("{base}/ident")).await.json()["properties"]["deliveryWithResourceIdentity"]["identity"]["type"],
        "SystemAssigned"
    );
    upload(&a, "evacct5", "id.txt").await;
    let mut all = Vec::new();
    for _ in 0..100 {
        all = peeked(&a, "evacct5").await;
        if all.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(all.iter().any(|e| e["subject"].as_str().is_some_and(|s| s.ends_with("id.txt"))), "{all:?}");

    // A bad identity is a 400; an Azure Function destination is accepted and read back.
    let bad = a
        .put(&format!("{base}/badid"), json!({"properties": {"deliveryWithResourceIdentity": {"identity": {"type": "Nope"}, "destination": queue_destination("ev5", "evacct5")}}}))
        .await;
    assert_eq!(bad.status, 400, "{}", bad.text());
    let func = json!({"endpointType": "AzureFunction", "properties": {
        "resourceId": rid("ev5", "Microsoft.Web", "sites/app1/functions/fn1")}});
    let r = a.put(&format!("{base}/func"), json!({"properties": {"destination": func}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let p = &r.json()["properties"]["destination"];
    assert_eq!(p["endpointType"], "AzureFunction");
    assert_eq!(p["properties"]["maxEventsPerBatch"], 1);
}
