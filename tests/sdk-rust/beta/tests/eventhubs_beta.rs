//! Event Hubs through `azure_messaging_eventhubs` 0.16.0 (with `azure_core_amqp` 1.2.0-beta.3 and fe2o3-amqp 0.18, rustls).
//! Connection strings, the WebSocket transport, the buffered producer, and the SDK's own receiver (the 0.15 receiver
//! cannot read from azemu, see docs/sdk-notes.md).

#[allow(dead_code)]
#[path = "../../src/lib.rs"]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use azure_core::time::Duration;
use azure_messaging_eventhubs::models::{AmqpTransport, EventData, ReceivedEventData};
use azure_messaging_eventhubs::{
    BufferedProducerClient, ConsumerClient, EnqueueEventOptions, OpenReceiverOptions, ProducerClient, SendEventOptions, StartLocation,
    StartPosition,
};
use common::{cred, emu, uniq};
use futures::StreamExt;

struct Hub {
    host: String,
    name: String,
}

/// A new event hub, made through the emulator's admin API before anything uses it (the emulator does not create hubs by being
/// named, as Azure does not).
fn hub() -> Hub {
    let h = Hub { host: format!("{}.eventhub.localhost:{}", uniq("ns"), emu().port), name: uniq("hub") };
    common::create_eventhub(h.host.split('.').next().unwrap(), &h.name);
    h
}

/// The connection string as Azure writes it: no port. The SDK builds `amqps://{namespace}/{hub}` from it and dials port 5671, so the
/// emulator is reached with `with_custom_endpoint` (the SDK's way to name another address to dial).
fn conn_string(h: &Hub) -> String {
    let ns = h.host.split(':').next().unwrap();
    format!("Endpoint=sb://{ns}/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;EntityPath={}", h.name)
}

fn dial(h: &Hub) -> String {
    format!("amqps://{}", h.host)
}

/// Events stored in a partition, from the emulator's admin API (`GET /_azemu/eventhubs/{ns}/hubs/{hub}/partitions/{n}`).
fn stored(h: &Hub, partition: &str) -> u64 {
    let ns = h.host.split('.').next().unwrap();
    let (st, body) = common::http_call("GET", &format!("/_azemu/eventhubs/{ns}/hubs/{}/partitions/{partition}", h.name), "");
    assert_eq!(st, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["eventCount"].as_u64().unwrap()
}

fn ev(s: &str) -> EventData {
    EventData::builder().with_body(s.as_bytes().to_vec()).build()
}

fn to(p: &str) -> Option<SendEventOptions> {
    Some(SendEventOptions { partition_id: Some(p.into()) })
}

fn earliest() -> Option<OpenReceiverOptions> {
    Some(OpenReceiverOptions {
        start_position: Some(StartPosition { location: StartLocation::Earliest, inclusive: false }),
        receive_timeout: Some(Duration::seconds(10)),
        ..Default::default()
    })
}

async fn read(c: &ConsumerClient, partition: &str, n: usize, wait: StdDuration) -> Vec<ReceivedEventData> {
    let r = c.open_receiver_on_partition(partition.to_string(), earliest()).await.expect("open receiver");
    let mut out = vec![];
    {
        let mut s = r.stream_events();
        while out.len() < n {
            match tokio::time::timeout(wait, s.next()).await {
                Ok(Some(Ok(e))) => out.push(e),
                Ok(Some(Err(e))) => panic!("receive error: {e:?}"),
                Ok(None) | Err(_) => break,
            }
        }
    }
    let _ = r.close().await;
    out
}

fn body(e: &ReceivedEventData) -> String {
    String::from_utf8(e.event_data().body().unwrap_or_default().to_vec()).unwrap()
}

#[tokio::test]
async fn sdk_receiver_reads_an_event_sent_earlier() {
    let h = hub();
    let p = ProducerClient::builder().open(&h.host, &h.name, cred()).await.unwrap();
    p.send_event(ev("hello"), to("0")).await.unwrap();
    let c = ConsumerClient::builder().open(&h.host, h.name.clone(), cred()).await.unwrap();
    let got = read(&c, "0", 1, StdDuration::from_secs(5)).await;
    assert_eq!(got.len(), 1, "the 0.16 receiver delivers nothing if azemu still ignores a session window of 4294967295");
    assert_eq!(body(&got[0]), "hello");
}

#[tokio::test]
async fn connection_string_producer_sends_an_event_that_the_hub_stores() {
    let h = hub();
    let p = ProducerClient::builder().with_custom_endpoint(dial(&h)).open_with_connection_string(&conn_string(&h), None).await.unwrap();
    p.send_event(ev("via connection string"), to("1")).await.unwrap();
    assert_eq!(stored(&h, "1"), 1);
}

#[tokio::test]
async fn eventhub_properties_through_a_connection_string_client() {
    let h = hub();
    let c = ConsumerClient::builder().with_custom_endpoint(dial(&h)).open_with_connection_string(&conn_string(&h), None).await.unwrap();
    let p = c.get_eventhub_properties().await.unwrap();
    assert_eq!(p.name, h.name);
    assert_eq!(p.partition_ids.len(), 4);
}

#[tokio::test]
async fn partition_properties_of_an_empty_partition() {
    let h = hub();
    let c = ConsumerClient::builder().open(&h.host, h.name.clone(), cred()).await.unwrap();
    let p = c.get_partition_properties("0").await.unwrap();
    assert!(p.is_empty);
}

#[tokio::test]
async fn websocket_transport_sends_through_the_emulator() {
    // AMQP over secure WebSocket; the custom endpoint names the port the emulator accepts WebSockets on.
    let h = hub();
    let p = ProducerClient::builder()
        .with_transport(AmqpTransport::WebSocket)
        .with_custom_endpoint(format!("https://{}", h.host))
        .open(&h.host, &h.name, cred())
        .await
        .unwrap();
    p.send_event(ev("over websocket"), to("0")).await.unwrap();
    assert_eq!(stored(&h, "0"), 1);
    let props = p.get_eventhub_properties().await.unwrap();
    assert_eq!(props.partition_ids.len(), 4);
}

#[tokio::test]
async fn buffered_producer_delivers_enqueued_events_on_flush_and_reports_success() {
    let h = hub();
    let sent = Arc::new(Mutex::new(0usize));
    let failed = Arc::new(Mutex::new(0usize));
    let (s2, f2) = (sent.clone(), failed.clone());
    let bp: BufferedProducerClient = BufferedProducerClient::builder()
        .with_on_send_succeeded(move |ctx| {
            let s = s2.clone();
            async move {
                *s.lock().unwrap() += ctx.events.len();
            }
        })
        .with_on_send_failed(move |_ctx| {
            let f = f2.clone();
            async move {
                *f.lock().unwrap() += 1;
            }
        })
        .open(&h.host, &h.name, cred())
        .await
        .unwrap();
    for i in 0..10 {
        bp.enqueue_event(ev(&format!("buf{i}")), Some(EnqueueEventOptions { partition_id: Some("2".into()), ..Default::default() }))
            .await
            .unwrap();
    }
    bp.flush().await.unwrap();
    bp.close().await.unwrap();
    assert_eq!(*failed.lock().unwrap(), 0);
    assert_eq!(*sent.lock().unwrap(), 10);
    assert_eq!(stored(&h, "2"), 10);
}
