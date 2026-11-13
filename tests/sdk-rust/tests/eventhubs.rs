//! Event Hubs through `azure_messaging_eventhubs` 0.15.0 (its own AMQP stack: fe2o3-amqp with rustls).
//! The client dials `amqps://{ns}.eventhub.localhost:{port}/{hub}`: AMQP inside TLS on the emulator's one port.

use std::collections::{HashMap, HashSet};
use std::time::{Duration as StdDuration, SystemTime};

use azemu_sdk_rust_tests::{cred, emu, uniq};
use azure_core::http::Url;
use azure_core::time::Duration;
use azure_core_amqp::message::AmqpSourceFilter;
use azure_core_amqp::{
    AmqpConnection, AmqpConnectionApis, AmqpDeliveryApis, AmqpDescribed, AmqpOrderedMap, AmqpReceiver, AmqpReceiverApis,
    AmqpReceiverOptions, AmqpSession, AmqpSessionApis, AmqpSessionOptions, AmqpSource, AmqpSymbol, AmqpValue, ReceiverCreditMode,
};
use azure_messaging_eventhubs::models::{EventData, ReceivedEventData};
use azure_messaging_eventhubs::{
    ConsumerClient, EventDataBatchOptions, OpenReceiverOptions, ProducerClient, SendEventOptions, StartLocation, StartPosition,
};
use futures::StreamExt;

struct Hub {
    host: String,
    name: String,
}

/// A new event hub, made through the emulator's admin API before anything uses it: the emulator does not create hubs by being
/// named (`AZEMU_EVENTHUBS_AUTOCREATE` is off by default), as Azure does not. It has the consumer group `analytics` besides
/// `$Default`. A plain HTTP request over `std::net`, so that this stays an ordinary function.
fn create_hub(h: &Hub) {
    use std::io::{Read, Write};
    let ns = h.host.split('.').next().unwrap_or_default();
    let body = r#"{"consumerGroups":["analytics"]}"#;
    let request = format!(
        "PUT /_azemu/eventhubs/{ns}/hubs/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        h.name,
        body.len(),
        port = emu().port
    );
    let mut s = std::net::TcpStream::connect(("127.0.0.1", emu().port)).expect("connect to the emulator");
    s.write_all(request.as_bytes()).expect("send the admin request");
    let mut answer = String::new();
    s.read_to_string(&mut answer).expect("read the admin answer");
    assert!(answer.starts_with("HTTP/1.1 201") || answer.starts_with("HTTP/1.1 200"), "creating the event hub: {answer}");
}

fn hub() -> Hub {
    let h = Hub { host: format!("{}.eventhub.localhost:{}", uniq("ns"), emu().port), name: uniq("hub") };
    create_hub(&h);
    h
}

async fn producer(h: &Hub) -> ProducerClient {
    ProducerClient::builder().open(&h.host, &h.name, cred()).await.expect("open producer")
}

async fn consumer(h: &Hub) -> ConsumerClient {
    ConsumerClient::builder().open(&h.host, h.name.clone(), cred()).await.expect("open consumer")
}

fn ev(body: &str) -> EventData {
    EventData::builder().with_body(body.as_bytes().to_vec()).build()
}

fn to_partition(p: &str) -> Option<SendEventOptions> {
    Some(SendEventOptions { partition_id: Some(p.to_string()) })
}

/// How a test reads events.
///
/// * `Sdk`: the Event Hubs SDK's own `ConsumerClient` / `EventReceiver`. Its AMQP layer begins every session with
///   an incoming window of 4294967295 (`AmqpSessionOptions::with_unbounded_windows()`), and the emulator then never
///   delivers a message (see docs/sdk-notes.md). These tests are ignored for that reason.
/// * `Amqp`: `azure_core_amqp` (the SDK's own lower layer) with a session window of 2^31-1, which the emulator
///   accepts. This checks the same behaviour (start positions, owner level, ...) without the window problem.
#[derive(Clone, Copy, Debug)]
enum Via {
    Sdk,
    Amqp,
}

fn expression(location: &StartLocation, inclusive: bool) -> String {
    let gt = if inclusive { ">=" } else { ">" };
    match location {
        StartLocation::Offset(o) => format!("amqp.annotation.x-opt-offset {gt}'{o}'"),
        StartLocation::SequenceNumber(n) => format!("amqp.annotation.x-opt-sequence-number {gt}'{n}'"),
        StartLocation::EnqueuedTime(t) => {
            format!("amqp.annotation.x-opt-enqueued-time {gt}'{}'", t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis())
        }
        StartLocation::Earliest => "amqp.annotation.x-opt-offset > '-1'".into(),
        StartLocation::Latest => "amqp.annotation.x-opt-offset > '@latest'".into(),
    }
}

/// An AMQP receiver link on one partition made with `azure_core_amqp` (window 2^31-1).
struct RawReceiver {
    _conn: AmqpConnection,
    _sess: AmqpSession,
    rx: AmqpReceiver,
}

async fn raw_receiver(
    h: &Hub,
    group: &str,
    partition: &str,
    location: &StartLocation,
    inclusive: bool,
    owner_level: Option<i64>,
) -> azure_core::Result<RawReceiver> {
    let conn = AmqpConnection::new();
    conn.open("azemu-rust-test".into(), Url::parse(&format!("amqps://{}", h.host)).unwrap(), None).await?;
    let sess = AmqpSession::new();
    sess.begin(
        &conn,
        Some(AmqpSessionOptions { incoming_window: Some(i32::MAX as u32), outgoing_window: Some(i32::MAX as u32), ..Default::default() }),
    )
    .await?;
    let rx = AmqpReceiver::new();
    let source = AmqpSource::builder()
        .with_address(format!("amqps://{}/{}/ConsumerGroups/{}/Partitions/{}", h.host, h.name, group, partition))
        .add_to_filter(
            AmqpSourceFilter::selector_filter().description().into(),
            Box::new(AmqpDescribed::new(AmqpSourceFilter::selector_filter().code(), expression(location, inclusive))),
        )
        .build();
    let mut props: AmqpOrderedMap<AmqpSymbol, AmqpValue> = AmqpOrderedMap::new();
    if let Some(level) = owner_level {
        props.insert("com.microsoft:epoch".into(), AmqpValue::from(level));
    }
    let opts = AmqpReceiverOptions {
        credit_mode: Some(ReceiverCreditMode::Auto(300)),
        auto_accept: true,
        properties: Some(props),
        ..Default::default()
    };
    rx.attach(&sess, source, Some(opts)).await?;
    Ok(RawReceiver { _conn: conn, _sess: sess, rx })
}

impl RawReceiver {
    async fn next(&self, wait: StdDuration) -> Option<azure_core::Result<ReceivedEventData>> {
        match tokio::time::timeout(wait, self.rx.receive_delivery()).await {
            Err(_) => None,
            Ok(Ok(d)) => Some(Ok(ReceivedEventData::from(d.message().clone()))),
            Ok(Err(e)) => Some(Err(e.into())),
        }
    }
}

/// Read up to `n` events from one partition, giving up after `wait` without a new event.
async fn rd(
    h: &Hub,
    via: Via,
    group: &str,
    partition: &str,
    location: StartLocation,
    inclusive: bool,
    n: usize,
    wait: StdDuration,
) -> Vec<ReceivedEventData> {
    let mut out = vec![];
    match via {
        Via::Amqp => {
            let r = raw_receiver(h, group, partition, &location, inclusive, None).await.expect("attach");
            while out.len() < n {
                match r.next(wait).await {
                    Some(Ok(e)) => out.push(e),
                    Some(Err(e)) => panic!("receive error: {e:?}"),
                    None => break,
                }
            }
        }
        Via::Sdk => {
            let c = ConsumerClient::builder()
                .with_consumer_group(group.to_string())
                .open(&h.host, h.name.clone(), cred())
                .await
                .expect("open consumer");
            let opts = OpenReceiverOptions {
                start_position: Some(StartPosition { location, inclusive }),
                receive_timeout: Some(Duration::seconds(10)),
                ..Default::default()
            };
            let r = c.open_receiver_on_partition(partition.to_string(), Some(opts)).await.expect("open receiver");
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
        }
    }
    out
}

async fn from_start(h: &Hub, via: Via, partition: &str, n: usize, wait: StdDuration) -> Vec<ReceivedEventData> {
    rd(h, via, "$Default", partition, StartLocation::Earliest, false, n, wait).await
}

fn body(e: &ReceivedEventData) -> String {
    String::from_utf8(e.event_data().body().unwrap_or_default().to_vec()).unwrap()
}

/// Events per partition, read from the start of every partition (through the AMQP layer, see `Via::Amqp`).
async fn counts(h: &Hub) -> HashMap<String, usize> {
    let c = consumer(h).await;
    let props = c.get_eventhub_properties().await.unwrap();
    let mut m = HashMap::new();
    for p in props.partition_ids {
        let got = from_start(h, Via::Amqp, &p, 1000, StdDuration::from_millis(800)).await;
        m.insert(p, got.len());
    }
    m
}

/// Declares a test that runs with the AMQP-layer reader and a twin that runs with the SDK reader.
macro_rules! both {
    ($amqp:ident, $sdk:ident, $body:ident) => {
        #[tokio::test]
        async fn $amqp() {
            $body(Via::Amqp).await
        }
        #[tokio::test]
        async fn $sdk() {
            $body(Via::Sdk).await
        }
    };
}

// ---- hub and partition properties ----------------------------------------------------------

#[tokio::test]
async fn eventhub_properties_report_name_and_four_partition_ids() {
    let h = hub();
    let c = consumer(&h).await;
    let p = c.get_eventhub_properties().await.unwrap();
    assert_eq!(p.name, h.name);
    let mut ids = p.partition_ids.clone();
    ids.sort();
    assert_eq!(ids, vec!["0", "1", "2", "3"]);
    assert!(p.created_on.is_some());
}

#[tokio::test]
async fn producer_reads_the_same_eventhub_properties_as_the_consumer() {
    let h = hub();
    let p = producer(&h).await;
    let props = p.get_eventhub_properties().await.unwrap();
    assert_eq!(props.partition_ids.len(), 4);
}

#[tokio::test]
async fn empty_partition_reports_is_empty_and_minus_one_sequence_numbers() {
    let h = hub();
    let c = consumer(&h).await;
    let p = c.get_partition_properties("0").await.unwrap();
    assert_eq!(p.id, "0");
    assert!(p.is_empty);
    assert_eq!(p.last_enqueued_sequence_number, -1);
}

#[tokio::test]
async fn partition_properties_follow_the_last_event_sent() {
    let h = hub();
    let p = producer(&h).await;
    for t in ["a", "b", "c"] {
        p.send_event(ev(t), to_partition("1")).await.unwrap();
    }
    let props = p.get_partition_properties("1").await.unwrap();
    assert!(!props.is_empty);
    assert_eq!(props.last_enqueued_sequence_number - props.beginning_sequence_number, 2);
    assert!(props.last_enqueued_time_utc.is_some());
    assert!(!props.last_enqueued_offset.is_empty());
    assert!(consumer(&h).await.get_partition_properties("2").await.unwrap().is_empty);
}

#[tokio::test]
async fn partition_properties_of_an_unknown_partition_is_an_error() {
    let h = hub();
    let c = consumer(&h).await;
    assert!(c.get_partition_properties("99").await.is_err());
}

// ---- send and receive ----------------------------------------------------------------------

async fn single_event(via: Via) {
    let h = hub();
    producer(&h).await.send_event(ev("hello"), to_partition("0")).await.unwrap();
    let got = from_start(&h, via, "0", 1, StdDuration::from_secs(5)).await;
    assert_eq!(got.len(), 1);
    assert_eq!(body(&got[0]), "hello");
}
both!(
    single_event_sent_to_a_partition_is_received_from_the_beginning,
    sdk_single_event_sent_to_a_partition_is_received_from_the_beginning,
    single_event
);

#[tokio::test]
async fn event_sent_to_one_partition_is_not_seen_on_the_others() {
    let h = hub();
    producer(&h).await.send_event(ev("only on 2"), to_partition("2")).await.unwrap();
    for part in ["0", "1", "3"] {
        assert!(from_start(&h, Via::Amqp, part, 1, StdDuration::from_millis(800)).await.is_empty(), "partition {part}");
    }
    assert_eq!(from_start(&h, Via::Amqp, "2", 1, StdDuration::from_secs(5)).await.len(), 1);
}

async fn order_and_sequence(via: Via) {
    let h = hub();
    let p = producer(&h).await;
    for i in 0..10 {
        p.send_event(ev(&format!("e{i}")), to_partition("0")).await.unwrap();
    }
    let got = from_start(&h, via, "0", 10, StdDuration::from_secs(5)).await;
    assert_eq!(got.iter().map(body).collect::<Vec<_>>(), (0..10).map(|i| format!("e{i}")).collect::<Vec<_>>());
    let seq: Vec<i64> = got.iter().map(|e| e.sequence_number().unwrap()).collect();
    for w in seq.windows(2) {
        assert_eq!(w[1], w[0] + 1);
    }
    let offsets: Vec<String> = got.iter().map(|e| e.offset().clone().unwrap()).collect();
    assert_eq!(offsets.iter().collect::<HashSet<_>>().len(), 10, "offsets are unique");
    for e in &got {
        assert!(e.enqueued_time().unwrap() <= SystemTime::now() + StdDuration::from_secs(5));
    }
}
both!(
    events_in_a_partition_keep_their_send_order_and_sequence_numbers_count_up,
    sdk_events_in_a_partition_keep_their_send_order_and_sequence_numbers_count_up,
    order_and_sequence
);

#[tokio::test]
async fn event_without_partition_id_lands_in_some_partition_of_the_hub() {
    let h = hub();
    producer(&h).await.send_event(ev("anywhere"), None).await.unwrap();
    let total: usize = counts(&h).await.values().sum();
    assert_eq!(total, 1);
}

#[tokio::test]
async fn events_sent_without_partition_id_are_spread_over_partitions() {
    let h = hub();
    let p = producer(&h).await;
    for i in 0..16 {
        p.send_event(ev(&format!("r{i}")), None).await.unwrap();
    }
    let c = counts(&h).await;
    assert_eq!(c.values().sum::<usize>(), 16);
    assert!(c.values().filter(|n| **n > 0).count() >= 2, "{c:?}");
}

async fn properties_roundtrip(via: Via) {
    let h = hub();
    let e = EventData::builder()
        .with_body(b"{\"a\":1}".to_vec())
        .with_content_type("application/json".to_string())
        .with_message_id("msg-1".to_string())
        .with_correlation_id("corr-1".to_string())
        .add_property("str".to_string(), "value")
        .add_property("int".to_string(), 42i64)
        .add_property("bool".to_string(), true)
        .add_property("float".to_string(), 1.5f64)
        .build();
    producer(&h).await.send_event(e, to_partition("0")).await.unwrap();
    let got = from_start(&h, via, "0", 1, StdDuration::from_secs(5)).await;
    let ed = got[0].event_data();
    assert_eq!(ed.content_type(), Some("application/json"));
    assert_eq!(
        format!("{:?}", ed.message_id().unwrap()),
        format!("{:?}", azure_messaging_eventhubs::models::MessageId::from("msg-1".to_string()))
    );
    assert_eq!(
        format!("{:?}", ed.correlation_id().unwrap()),
        format!("{:?}", azure_messaging_eventhubs::models::MessageId::from("corr-1".to_string()))
    );
    let props = ed.properties().unwrap();
    assert_eq!(props.len(), 4);
    assert_eq!(format!("{:?}", props["str"]), "String(\"value\")");
    assert_eq!(format!("{:?}", props["int"]), "Long(42)");
    assert_eq!(format!("{:?}", props["bool"]), "Boolean(true)");
    assert_eq!(format!("{:?}", props["float"]), "Double(1.5)");
}
both!(event_properties_content_type_and_ids_roundtrip, sdk_event_properties_content_type_and_ids_roundtrip, properties_roundtrip);

async fn binary_body(via: Via) {
    let h = hub();
    let bytes: Vec<u8> = (0..=255u8).collect();
    producer(&h).await.send_event(EventData::builder().with_body(bytes.clone()).build(), to_partition("0")).await.unwrap();
    let got = from_start(&h, via, "0", 1, StdDuration::from_secs(5)).await;
    assert_eq!(got[0].event_data().body().unwrap(), &bytes[..]);
}
both!(binary_body_of_all_byte_values_roundtrips, sdk_binary_body_of_all_byte_values_roundtrips, binary_body);

async fn big_event(via: Via) {
    let h = hub();
    let bytes: Vec<u8> = (0..900 * 1024).map(|i| (i % 251) as u8).collect();
    producer(&h).await.send_event(EventData::builder().with_body(bytes.clone()).build(), to_partition("0")).await.unwrap();
    let got = from_start(&h, via, "0", 1, StdDuration::from_secs(10)).await;
    assert!(got[0].event_data().body().unwrap() == &bytes[..]);
}
both!(event_of_900_kib_roundtrips, sdk_event_of_900_kib_roundtrips, big_event);

#[tokio::test]
async fn event_larger_than_the_hub_limit_is_refused() {
    // Azure's limit is 1 MB per event (Standard); the emulator advertises 1 MiB.
    let h = hub();
    let p = producer(&h).await;
    let r = p.send_event(EventData::builder().with_body(vec![7u8; 1_100_000]).build(), to_partition("0")).await;
    assert!(r.is_err(), "a 1.1 MB event must not be accepted");
}

// ---- batches -------------------------------------------------------------------------------

#[tokio::test]
async fn batch_of_events_to_a_partition_arrives_in_order() {
    let h = hub();
    let p = producer(&h).await;
    let batch = p.create_batch(Some(EventDataBatchOptions { partition_id: Some("3".into()), ..Default::default() })).await.unwrap();
    for i in 0..25 {
        assert!(batch.try_add_event_data(ev(&format!("b{i}")), None).unwrap());
    }
    assert_eq!(batch.len(), 25);
    p.send_batch(batch, None).await.unwrap();
    let got = from_start(&h, Via::Amqp, "3", 25, StdDuration::from_secs(5)).await;
    assert_eq!(got.iter().map(body).collect::<Vec<_>>(), (0..25).map(|i| format!("b{i}")).collect::<Vec<_>>());
}

#[tokio::test]
async fn batch_with_a_partition_key_puts_every_event_in_one_partition() {
    let h = hub();
    let p = producer(&h).await;
    let batch =
        p.create_batch(Some(EventDataBatchOptions { partition_key: Some("customer-7".into()), ..Default::default() })).await.unwrap();
    for i in 0..12 {
        assert!(batch.try_add_event_data(ev(&format!("k{i}")), None).unwrap());
    }
    p.send_batch(batch, None).await.unwrap();
    let counts = counts(&h).await;
    let non_empty: Vec<_> = counts.iter().filter(|(_, n)| **n > 0).collect();
    assert_eq!(non_empty.len(), 1, "{counts:?}");
    assert_eq!(*non_empty[0].1, 12);
}

#[tokio::test]
async fn received_events_carry_the_partition_key_of_the_batch() {
    let h = hub();
    let p = producer(&h).await;
    let batch = p.create_batch(Some(EventDataBatchOptions { partition_key: Some("pk-x".into()), ..Default::default() })).await.unwrap();
    batch.try_add_event_data(ev("x"), None).unwrap();
    p.send_batch(batch, None).await.unwrap();
    let mut found = None;
    for part in ["0", "1", "2", "3"] {
        if let Some(e) = from_start(&h, Via::Amqp, part, 1, StdDuration::from_millis(800)).await.into_iter().next() {
            found = Some(e);
        }
    }
    let e = found.expect("one event somewhere");
    assert_eq!(e.partition_key().as_deref(), Some("pk-x"));
}

#[tokio::test]
async fn same_partition_key_in_two_batches_goes_to_the_same_partition() {
    let h = hub();
    let p = producer(&h).await;
    for round in 0..2 {
        let batch =
            p.create_batch(Some(EventDataBatchOptions { partition_key: Some("stable-key".into()), ..Default::default() })).await.unwrap();
        batch.try_add_event_data(ev(&format!("r{round}")), None).unwrap();
        p.send_batch(batch, None).await.unwrap();
    }
    let c = counts(&h).await;
    assert_eq!(c.values().filter(|n| **n == 2).count(), 1, "{c:?}");
}

#[tokio::test]
async fn different_partition_keys_spread_over_more_than_one_partition() {
    let h = hub();
    let p = producer(&h).await;
    for k in 0..16 {
        let batch =
            p.create_batch(Some(EventDataBatchOptions { partition_key: Some(format!("key-{k}")), ..Default::default() })).await.unwrap();
        batch.try_add_event_data(ev("x"), None).unwrap();
        p.send_batch(batch, None).await.unwrap();
    }
    let c = counts(&h).await;
    assert_eq!(c.values().sum::<usize>(), 16);
    assert!(c.values().filter(|n| **n > 0).count() >= 2, "16 keys should not all hash to one partition: {c:?}");
}

#[tokio::test]
async fn batch_is_full_at_the_link_max_message_size_of_one_mebibyte() {
    // The SDK takes the batch limit from the sender link's max-message-size (the option `max_size_in_bytes` is
    // overwritten when the batch attaches; docs/sdk-notes.md). Azure's Standard tier advertises 1 048 576.
    let h = hub();
    let p = producer(&h).await;
    let batch = p.create_batch(Some(EventDataBatchOptions { partition_id: Some("0".into()), ..Default::default() })).await.unwrap();
    let mut added = 0;
    for _ in 0..30 {
        if batch.try_add_event_data(EventData::builder().with_body(vec![1u8; 100_000]).build(), None).unwrap() {
            added += 1;
        } else {
            break;
        }
    }
    // The SDK counts the first event twice, so it stops at 9 (10 would fit on the wire).
    assert!((9..=10).contains(&added), "added {added}");
    assert!(batch.size() <= 1_048_576);
    p.send_batch(batch, None).await.unwrap();
    assert_eq!(from_start(&h, Via::Amqp, "0", 30, StdDuration::from_secs(5)).await.len(), added);
}

// ---- start positions -----------------------------------------------------------------------

async fn seeded(n: usize) -> Hub {
    let h = hub();
    let p = producer(&h).await;
    for i in 0..n {
        p.send_event(ev(&format!("s{i}")), to_partition("0")).await.unwrap();
    }
    h
}

async fn latest(via: Via) {
    let h = seeded(3).await;
    let p = producer(&h).await;
    match via {
        Via::Amqp => {
            let r = raw_receiver(&h, "$Default", "0", &StartLocation::Latest, false, None).await.unwrap();
            assert!(r.next(StdDuration::from_millis(1000)).await.is_none(), "the three old events are not delivered");
            p.send_event(ev("fresh"), to_partition("0")).await.unwrap();
            let e = r.next(StdDuration::from_secs(5)).await.expect("event arrives").unwrap();
            assert_eq!(body(&e), "fresh");
        }
        Via::Sdk => {
            let c = consumer(&h).await;
            let opts = OpenReceiverOptions {
                start_position: Some(StartPosition { location: StartLocation::Latest, inclusive: false }),
                receive_timeout: Some(Duration::seconds(10)),
                ..Default::default()
            };
            let r = c.open_receiver_on_partition("0".into(), Some(opts)).await.unwrap();
            let mut s = r.stream_events();
            assert!(tokio::time::timeout(StdDuration::from_millis(1000), s.next()).await.is_err());
            p.send_event(ev("fresh"), to_partition("0")).await.unwrap();
            let e = tokio::time::timeout(StdDuration::from_secs(5), s.next()).await.expect("event arrives").unwrap().unwrap();
            assert_eq!(body(&e), "fresh");
        }
    }
}
both!(receiver_from_latest_sees_only_events_sent_after_it_opened, sdk_receiver_from_latest_sees_only_events_sent_after_it_opened, latest);

async fn seq_exclusive(via: Via) {
    let h = seeded(5).await;
    let all = from_start(&h, Via::Amqp, "0", 5, StdDuration::from_secs(5)).await;
    let seq = all[1].sequence_number().unwrap();
    let got = rd(&h, via, "$Default", "0", StartLocation::SequenceNumber(seq), false, 10, StdDuration::from_millis(1500)).await;
    assert_eq!(got.iter().map(body).collect::<Vec<_>>(), vec!["s2", "s3", "s4"]);
}
both!(
    receiver_from_sequence_number_exclusive_skips_that_event,
    sdk_receiver_from_sequence_number_exclusive_skips_that_event,
    seq_exclusive
);

async fn seq_inclusive(via: Via) {
    let h = seeded(5).await;
    let all = from_start(&h, Via::Amqp, "0", 5, StdDuration::from_secs(5)).await;
    let seq = all[1].sequence_number().unwrap();
    let got = rd(&h, via, "$Default", "0", StartLocation::SequenceNumber(seq), true, 10, StdDuration::from_millis(1500)).await;
    assert_eq!(got.iter().map(body).collect::<Vec<_>>(), vec!["s1", "s2", "s3", "s4"]);
}
both!(
    receiver_from_sequence_number_inclusive_includes_that_event,
    sdk_receiver_from_sequence_number_inclusive_includes_that_event,
    seq_inclusive
);

async fn offset_exclusive(via: Via) {
    let h = seeded(4).await;
    let all = from_start(&h, Via::Amqp, "0", 4, StdDuration::from_secs(5)).await;
    let off = all[2].offset().clone().unwrap();
    let got = rd(&h, via, "$Default", "0", StartLocation::Offset(off), false, 10, StdDuration::from_millis(1500)).await;
    assert_eq!(got.iter().map(body).collect::<Vec<_>>(), vec!["s3"]);
}
both!(receiver_from_offset_exclusive_starts_after_that_event, sdk_receiver_from_offset_exclusive_starts_after_that_event, offset_exclusive);

async fn enqueued_time(via: Via) {
    let h = hub();
    let p = producer(&h).await;
    p.send_event(ev("old"), to_partition("0")).await.unwrap();
    tokio::time::sleep(StdDuration::from_millis(1500)).await;
    let cut = SystemTime::now();
    tokio::time::sleep(StdDuration::from_millis(500)).await;
    p.send_event(ev("new1"), to_partition("0")).await.unwrap();
    p.send_event(ev("new2"), to_partition("0")).await.unwrap();
    let got = rd(&h, via, "$Default", "0", StartLocation::EnqueuedTime(cut), false, 10, StdDuration::from_millis(1500)).await;
    assert_eq!(got.iter().map(body).collect::<Vec<_>>(), vec!["new1", "new2"]);
}
both!(
    receiver_from_enqueued_time_returns_events_enqueued_after_that_time,
    sdk_receiver_from_enqueued_time_returns_events_enqueued_after_that_time,
    enqueued_time
);

#[tokio::test]
async fn receiver_from_a_sequence_number_beyond_the_end_is_refused() {
    // Azure: a start position past the end of the partition is an error (argument out of range).
    let h = seeded(2).await;
    let r = raw_receiver(&h, "$Default", "0", &StartLocation::SequenceNumber(1000), false, None).await;
    match r {
        Err(_) => {}
        Ok(r) => match r.next(StdDuration::from_secs(5)).await {
            Some(Err(_)) => {}
            other => panic!("expected the link to fail, got {other:?}"),
        },
    }
}

// ---- consumer groups and owner level -------------------------------------------------------

async fn groups(via: Via) {
    let h = hub();
    let p = producer(&h).await;
    for i in 0..3 {
        p.send_event(ev(&format!("g{i}")), to_partition("0")).await.unwrap();
    }
    for group in ["$Default", "analytics"] {
        let got = rd(&h, via, group, "0", StartLocation::Earliest, false, 3, StdDuration::from_secs(5)).await;
        assert_eq!(got.len(), 3, "consumer group {group}");
    }
}
both!(two_consumer_groups_each_read_every_event, sdk_two_consumer_groups_each_read_every_event, groups);

async fn reading_keeps_events(via: Via) {
    let h = seeded(3).await;
    for _ in 0..2 {
        assert_eq!(from_start(&h, via, "0", 3, StdDuration::from_secs(5)).await.len(), 3);
    }
}
both!(reading_does_not_remove_events, sdk_reading_does_not_remove_events, reading_keeps_events);

#[tokio::test]
async fn receive_timeout_ends_the_wait_when_no_event_arrives() {
    let h = hub();
    let c = consumer(&h).await;
    let r = c
        .open_receiver_on_partition(
            "0".into(),
            Some(OpenReceiverOptions {
                receive_timeout: Some(Duration::seconds(1)),
                start_position: Some(StartPosition { location: StartLocation::Earliest, inclusive: false }),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let mut s = r.stream_events();
    let t = std::time::Instant::now();
    let _first = tokio::time::timeout(StdDuration::from_secs(10), s.next()).await.expect("the SDK's own receive timeout must fire first");
    assert!(t.elapsed() < StdDuration::from_secs(5));
}

#[tokio::test]
async fn hubs_in_different_namespaces_are_separate() {
    let a = hub();
    let b = Hub { host: format!("{}.eventhub.localhost:{}", uniq("ns"), emu().port), name: a.name.clone() };
    create_hub(&b);
    producer(&a).await.send_event(ev("in a"), to_partition("0")).await.unwrap();
    assert!(from_start(&b, Via::Amqp, "0", 1, StdDuration::from_millis(800)).await.is_empty());
    assert_eq!(from_start(&a, Via::Amqp, "0", 1, StdDuration::from_secs(5)).await.len(), 1);
}
