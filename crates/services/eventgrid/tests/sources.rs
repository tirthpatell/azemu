//! Event sources of other services: subscriptions on a storage account, the Rust API for them,
//! and delivery to registered destination handlers.

mod common;

use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_eventgrid::{DestinationDelivery, DestinationKind, EventGridService, Options, SourceEvent, SourceRef, SubscriptionConfig};
use common::*;
use parking_lot::Mutex;
use serde_json::{json, Value};

fn blob_created(subject: &str) -> SourceEvent {
    SourceEvent {
        subject: subject.to_string(),
        event_type: "Microsoft.Storage.BlobCreated".into(),
        data: json!({"api": "PutBlob", "contentLength": 5, "url": "http://x/c/b"}),
        data_version: String::new(),
        id: Some("evt-1".into()),
        time: None,
    }
}

fn cfg(v: Value) -> SubscriptionConfig {
    serde_json::from_value(v).unwrap()
}

type Seen = Arc<Mutex<Vec<DestinationDelivery>>>;

fn recording_handler(svc: &EventGridService, kind: DestinationKind, fail_first: usize) -> Seen {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    svc.register_destination(
        kind,
        Arc::new(move |d| {
            let mut v = s2.lock();
            v.push(d.clone());
            if v.len() <= fail_first {
                Err("not yet".into())
            } else {
                Ok(())
            }
        }),
    );
    seen
}

#[tokio::test]
async fn webhook_subscription_on_a_storage_account_receives_the_system_event() {
    let svc = EventGridService::with_options(fast_options());
    let hook = Hook::ok().await;
    let src = SourceRef::storage_account("Acct1");
    assert!(!svc.wants_source(&src));
    assert_eq!(svc.raise_source_event(&src, blob_created("/s")), 0);

    let out = svc.put_source_subscription(&src, "S1", cfg(json!({"webhookUrl": hook.url})), None).await.unwrap();
    assert!(out.created);
    assert_eq!(out.view["validation"]["status"], "Skipped"); // no validation URL given
    assert!(svc.wants_source(&src) && svc.wants_storage_account("acct1"));
    assert!(!svc.wants_storage_account("other"));
    assert_eq!(svc.raise_source_event(&src, blob_created("/blobServices/default/containers/c/blobs/b")), 1);

    let got = eventually(5.0, || {
        let n = hook.notifications();
        (!n.is_empty()).then_some(n)
    })
    .await
    .expect("delivery");
    let body = got[0].json();
    let ev = &body[0];
    assert_eq!(ev["eventType"], "Microsoft.Storage.BlobCreated");
    assert_eq!(
        ev["topic"],
        "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/azemu/providers/Microsoft.Storage/storageAccounts/Acct1"
    );
    assert_eq!(ev["id"], "evt-1");
    assert_eq!(ev["dataVersion"], "");
    assert_eq!(ev["metadataVersion"], "1");
    assert_eq!(ev["data"]["api"], "PutBlob");
    assert_eq!(got[0].header("aeg-subscription-name"), Some("S1"));

    assert_eq!(svc.list_source_subscriptions(&src).len(), 1);
    assert_eq!(svc.get_source_subscription(&src, "s1").unwrap()["name"], "S1");
    assert!(svc.delete_source_subscription(&src, "S1"));
    assert!(!svc.delete_source_subscription(&src, "S1"));
    assert!(!svc.wants_source(&src));
}

#[tokio::test]
async fn a_real_resource_id_becomes_the_topic_of_the_events() {
    let svc = EventGridService::with_options(fast_options());
    let rid = "/subscriptions/11111111-2222-3333-4444-555555555555/resourceGroups/rg1/providers/Microsoft.Storage/storageAccounts/acctx";
    let by_id = SourceRef::from_resource_id(rid).unwrap();
    let hook = Hook::ok().await;
    svc.put_source_subscription(&by_id, "s", cfg(json!({"webhookUrl": hook.url, "deliverySchema": "CloudEventSchemaV1_0"})), None)
        .await
        .unwrap();
    // the storage side only knows the account name and still finds the source
    assert!(svc.wants_storage_account("acctx"));
    svc.raise_source_event(&SourceRef::storage_account("acctx"), blob_created("/blobServices/default/containers/c/blobs/b"));
    let got = eventually(5.0, || {
        let n = hook.notifications();
        (!n.is_empty()).then_some(n)
    })
    .await
    .expect("delivery");
    let ev = got[0].json();
    assert_eq!(ev["source"], rid);
    assert_eq!(ev["specversion"], "1.0");
    assert!(ev.get("dataversion").is_none(), "an empty dataVersion is not sent as a CloudEvents extension");
    assert_eq!(got[0].header("content-type"), Some("application/cloudevents+json; charset=utf-8"));
}

#[tokio::test]
async fn filters_apply_to_source_events() {
    let svc = EventGridService::with_options(fast_options());
    let src = SourceRef::storage_account("acct");
    let seen = recording_handler(&svc, DestinationKind::StorageQueue, 0);
    let dest = json!({"endpointType": "StorageQueue", "storageAccount": "acct", "queueName": "q"});
    svc.put_source_subscription(
        &src,
        "f",
        cfg(json!({"destination": dest, "filter": {"subjectBeginsWith": "/blobServices/default/containers/keep/", "includedEventTypes": ["Microsoft.Storage.BlobCreated"]}})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(svc.raise_source_event(&src, blob_created("/blobServices/default/containers/other/blobs/x")), 0);
    assert_eq!(svc.raise_source_event(&src, blob_created("/blobServices/default/containers/keep/blobs/x")), 1);
    eventually(3.0, || (seen.lock().len() == 1).then_some(())).await.expect("one delivery");
}

#[tokio::test]
async fn destination_handler_gets_a_finished_message_and_failures_are_retried() {
    let svc = EventGridService::with_options(fast_options());
    let src = SourceRef::storage_account("acct");
    let seen = recording_handler(&svc, DestinationKind::ServiceBusQueue, 2);
    let dest = json!({"endpointType": "ServiceBusQueue", "properties": {"resourceId":
        "/subscriptions/s/resourceGroups/r/providers/Microsoft.ServiceBus/namespaces/ns1/queues/q1"}});
    let out = svc.put_source_subscription(&src, "Sb", cfg(json!({"destination": dest})), None).await.unwrap();
    assert_eq!(out.view["validation"]["status"], "NotRequired");
    assert_eq!(out.view["deliveryMode"], "push");
    svc.raise_source_event(&src, blob_created("/blobServices/default/containers/c/blobs/b"));
    let calls = eventually(5.0, || {
        let v = seen.lock().clone();
        (v.len() >= 3).then_some(v)
    })
    .await
    .expect("three attempts");
    assert_eq!(calls.iter().map(|d| d.attempt).collect::<Vec<_>>(), vec![1, 2, 3]);
    let d = &calls[2];
    assert_eq!((d.target.namespace.as_str(), d.target.entity.as_str()), ("ns1", "q1"));
    assert_eq!(d.kind, DestinationKind::ServiceBusQueue);
    assert_eq!(d.content_type, "application/json");
    assert_eq!(d.event_id, "evt-1");
    let body: Value = serde_json::from_slice(&d.body).unwrap();
    assert!(body.is_object(), "one event, not an array");
    assert_eq!(body["eventType"], "Microsoft.Storage.BlobCreated");
    let prop = |n: &str| d.properties.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    assert_eq!(prop("aeg-delivery-count"), Some("2"));
    assert_eq!(prop("aeg-subscription-name"), Some("Sb"));
    assert_eq!(prop("aeg-event-type"), Some("Notification"));
    assert_eq!(prop("aeg-metadata-version"), Some("1"));
    assert_eq!(prop("aeg-output-event-id"), Some("evt-1"));
    // the third attempt worked: delivered, nothing dead-lettered
    let v = eventually(3.0, || {
        let v = svc.get_source_subscription(&src, "Sb")?;
        (v["stats"]["delivered"] == 1).then_some(v)
    })
    .await
    .expect("delivered");
    assert_eq!(v["stats"]["deadLettered"], 0);
    assert_eq!(v["stats"]["failedAttempts"], 2);
}

#[tokio::test]
async fn exhausted_attempts_dead_letter() {
    let svc = EventGridService::with_options(fast_options());
    let src = SourceRef::storage_account("acct");
    recording_handler(&svc, DestinationKind::EventHub, usize::MAX);
    svc.put_source_subscription(
        &src,
        "eh",
        cfg(json!({"destination": {"endpointType": "EventHub", "namespace": "n", "eventHubName": "h"}, "retryPolicy": {"maxDeliveryAttempts": 2}})),
        None,
    )
    .await
    .unwrap();
    svc.raise_source_event(&src, blob_created("/s"));
    let v = eventually(5.0, || {
        let v = svc.get_source_subscription(&src, "eh")?;
        (v["stats"]["deadLettered"] == 1).then_some(v)
    })
    .await
    .expect("dead letter");
    assert_eq!(v["stats"]["failedAttempts"], 2);
}

#[tokio::test]
async fn subscribing_without_a_handler_is_501_and_bad_input_is_400() {
    let svc = EventGridService::with_options(fast_options());
    let src = SourceRef::storage_account("acct");
    let e = svc
        .put_source_subscription(
            &src,
            "s",
            cfg(json!({"destination": {"endpointType": "StorageQueue", "storageAccount": "a", "queueName": "q"}})),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(e.status, 501);
    assert!(e.message.contains("StorageQueue"));
    let e = svc.put_source_subscription(&src, "s", cfg(json!({})), None).await.unwrap_err();
    assert_eq!(e.status, 400); // nothing to deliver to / pull is for namespaces
    let e = svc.put_source_subscription(&src, "s", cfg(json!({"destination": {"endpointType": "Bogus"}})), None).await.unwrap_err();
    assert_eq!(e.status, 400);
    let e = svc.put_source_subscription(&src, "a/b", cfg(json!({"webhookUrl": "http://127.0.0.1:1/x"})), None).await.unwrap_err();
    assert_eq!(e.status, 400);
    assert!(!svc.wants_source(&src));
}

#[tokio::test]
async fn reset_drops_sources_but_keeps_handlers() {
    let svc = EventGridService::with_options(fast_options());
    let src = SourceRef::storage_account("acct");
    let seen = recording_handler(&svc, DestinationKind::StorageQueue, 0);
    let dest = json!({"destination": {"endpointType": "StorageQueue", "storageAccount": "acct", "queueName": "q"}});
    svc.put_source_subscription(&src, "s", cfg(dest.clone()), None).await.unwrap();
    svc.reset();
    assert!(!svc.wants_source(&src));
    assert_eq!(svc.raise_source_event(&src, blob_created("/s")), 0);
    svc.put_source_subscription(&src, "s", cfg(dest), None).await.unwrap(); // the handler is still registered
    svc.raise_source_event(&src, blob_created("/s"));
    eventually(3.0, || (seen.lock().len() == 1).then_some(())).await.expect("delivery");
}

#[tokio::test]
async fn admin_api_and_config_file_for_storage_accounts() {
    let dir = std::env::temp_dir().join(format!("azemu-eg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("eventgrid.json");
    std::fs::write(
        &path,
        json!({"storageAccounts": [{"name": "Files", "subscriptions": [
            {"name": "toq", "destination": {"endpointType": "StorageQueue", "storageAccount": "files", "queueName": "ev"},
             "filter": {"includedEventTypes": ["Microsoft.Storage.BlobDeleted"]}},
            {"name": "bad", "destination": {"endpointType": "StorageQueue"}}]}]})
        .to_string(),
    )
    .unwrap();
    let svc = Arc::new(EventGridService::with_options(Options { config_path: Some(path), ..fast_options() }));
    let seen = recording_handler(&svc, DestinationKind::StorageQueue, 0);
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let t = Eg { srv };
    let r = t.get_admin("/sources/storage/files/subscriptions").await;
    assert_eq!(r.status, 200);
    let list = r.json();
    assert_eq!(list["value"].as_array().unwrap().len(), 1, "the invalid entry is skipped: {list}");
    assert_eq!(list["value"][0]["filter"]["includedEventTypes"][0], "Microsoft.Storage.BlobDeleted");
    // PUT, GET, DELETE through the admin API
    let r = t
        .put_json(
            "/sources/storage/files/subscriptions/two",
            &json!({"destination": {"endpointType": "StorageQueue", "storageAccount": "files", "queueName": "q2"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(t.get_admin("/sources/storage/files/subscriptions/two").await.status, 200);
    assert_eq!(t.get_admin("/sources").await.json()["value"][0]["subscriptions"], json!(["toq", "two"]));
    assert_eq!(t.get_admin("/sources/unknown/files/subscriptions").await.status, 404);
    svc.raise_source_event(&SourceRef::storage_account("files"), blob_created("/s")); // type does not match "toq"
    eventually(3.0, || (seen.lock().len() == 1).then_some(())).await.expect("delivery to two");
    assert_eq!(seen.lock()[0].target.entity, "q2");
    // reset reloads the config file
    t.srv.reset();
    assert_eq!(t.get_admin("/sources/storage/files/subscriptions").await.json()["value"].as_array().unwrap().len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}
