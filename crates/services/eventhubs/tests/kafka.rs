//! The Kafka endpoint through a small client that speaks the wire protocol with `azemu-kafka`: every API the broker
//! serves, the mapping onto event hubs, idempotent producers, group coordination, and the bounds.

use std::sync::Arc;
use std::time::Duration;

use azemu_core::testing::TestServer;
use azemu_eventhubs::kafka::{endpoint_with, KafkaConfig, KafkaEndpoint};
use azemu_eventhubs::state::{now_ms, Shared};
use azemu_kafka::api::{self, err};
use azemu_kafka::header::{read_response, write_request};
use azemu_kafka::records::{decode_batch, encode_batch, Batch, Header, Record};
use azemu_kafka::{RequestHeader, Struct, Value};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Env {
    srv: TestServer,
    shared: Arc<Shared>,
    #[allow(dead_code)]
    kafka: Arc<KafkaEndpoint>,
}

async fn env_with(shared: Arc<Shared>, cfg: KafkaConfig) -> Env {
    let (svc, amqp) = azemu_eventhubs::pair_with(shared.clone());
    let kafka = endpoint_with(shared.clone(), cfg);
    let srv = TestServer::start_with(vec![svc, kafka.clone()], vec![amqp, kafka.clone()]).await;
    Env { srv, shared, kafka }
}

async fn env() -> Env {
    env_with(Shared::new(true, None), KafkaConfig::default()).await
}

struct K {
    s: TcpStream,
    corr: i32,
}

impl K {
    async fn connect(env: &Env) -> K {
        K { s: TcpStream::connect(("127.0.0.1", env.srv.port())).await.unwrap(), corr: 0 }
    }

    async fn call(&mut self, key: i16, version: i16, body: Struct) -> Struct {
        self.corr += 1;
        let h = RequestHeader { api_key: key, version, correlation_id: self.corr, client_id: Some("rust-test".into()) };
        self.s.write_all(&write_request(&h, &body)).await.unwrap();
        let mut len = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(20), self.s.read_exact(&mut len)).await.expect("answer in time").unwrap();
        let mut frame = vec![0u8; u32::from_be_bytes(len) as usize];
        self.s.read_exact(&mut frame).await.unwrap();
        let (corr, resp) = read_response(&h, &frame).unwrap();
        assert_eq!(corr, self.corr);
        resp
    }

    async fn closed_after(&mut self, bytes: &[u8]) -> bool {
        let _ = self.s.write_all(bytes).await;
        let mut b = [0u8; 1];
        matches!(tokio::time::timeout(Duration::from_secs(5), self.s.read(&mut b)).await, Ok(Ok(0)) | Ok(Err(_)))
    }
}

fn batch(base: i64, values: &[&str]) -> Batch {
    Batch {
        base_offset: base,
        partition_leader_epoch: -1,
        attributes: 0,
        last_offset_delta: values.len() as i32 - 1,
        first_timestamp: now_ms(),
        max_timestamp: now_ms(),
        producer_id: -1,
        producer_epoch: -1,
        base_sequence: -1,
        records: values
            .iter()
            .enumerate()
            .map(|(i, v)| Record {
                offset_delta: i as i32,
                timestamp_delta: 0,
                key: None,
                value: Some(Bytes::from(v.to_string())),
                headers: vec![],
            })
            .collect(),
        wire_len: 0,
    }
}

fn produce_body(topic: &str, partition: i32, records: Vec<u8>) -> Struct {
    Struct::new().with("TransactionalId", Value::Null).with("Acks", -1).with("TimeoutMs", 5000).with(
        "TopicData",
        vec![Struct::new()
            .with("Name", topic.to_string())
            .with("PartitionData", vec![Struct::new().with("Index", partition).with("Records", records)])],
    )
}

async fn produce(k: &mut K, topic: &str, partition: i32, b: &Batch) -> (i16, i64) {
    let r = k.call(api::PRODUCE, 7, produce_body(topic, partition, encode_batch(b, false))).await;
    let t = r.structs("Responses").next().unwrap();
    let p = t.structs("PartitionResponses").next().unwrap();
    (p.i16("ErrorCode"), p.i64("BaseOffset"))
}

fn fetch_body(topic: &str, partition: i32, offset: i64, max_wait: i32, min_bytes: i32) -> Struct {
    Struct::new()
        .with("MaxWaitMs", max_wait)
        .with("MinBytes", min_bytes)
        .with("MaxBytes", 1_000_000)
        .with("IsolationLevel", 0)
        .with("SessionId", 0)
        .with("SessionEpoch", -1)
        .with(
            "Topics",
            vec![Struct::new().with("Topic", topic.to_string()).with(
                "Partitions",
                vec![Struct::new()
                    .with("Partition", partition)
                    .with("FetchOffset", offset)
                    .with("LogStartOffset", -1i64)
                    .with("PartitionMaxBytes", 1_000_000)],
            )],
        )
}

/// (error, high watermark, log start, the records of the batch)
async fn fetch(k: &mut K, topic: &str, partition: i32, offset: i64, max_wait: i32, min_bytes: i32) -> (i16, i64, i64, Vec<(i64, Record)>) {
    let r = k.call(api::FETCH, 11, fetch_body(topic, partition, offset, max_wait, min_bytes)).await;
    let t = r.structs("Responses").next().unwrap();
    let p = t.structs("Partitions").next().unwrap();
    let mut recs = Vec::new();
    if let Some(b) = p.bytes("Records") {
        let mut data = &b[..];
        while !data.is_empty() {
            let (batch, used) = decode_batch(data, 1 << 24).unwrap();
            for r in batch.records {
                recs.push((batch.base_offset + r.offset_delta as i64, r));
            }
            data = &data[used..];
        }
    }
    (p.i16("ErrorCode"), p.i64("HighWatermark"), p.i64("LogStartOffset"), recs)
}

fn topics_of(names: &[&str]) -> Value {
    Value::Array(names.iter().map(|n| Value::Struct(Struct::new().with("Name", n.to_string()))).collect())
}

async fn metadata(k: &mut K, names: Option<&[&str]>, allow_auto: bool) -> Struct {
    k.call(
        api::METADATA,
        9,
        Struct::new()
            .with("Topics", names.map(topics_of).unwrap_or(Value::Null))
            .with("AllowAutoTopicCreation", allow_auto)
            .with("IncludeClusterAuthorizedOperations", false)
            .with("IncludeTopicAuthorizedOperations", false),
    )
    .await
}

// ---- ApiVersions, Metadata -------------------------------------------------------------------------------------

#[tokio::test]
async fn api_versions_lists_what_is_served_and_nothing_else() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    for v in 0..=4 {
        let r = k.call(api::API_VERSIONS, v, Struct::new().with("ClientSoftwareName", "t").with("ClientSoftwareVersion", "1")).await;
        assert_eq!(r.i16("ErrorCode"), 0);
        let keys: Vec<(i16, i16, i16)> =
            r.structs("ApiKeys").map(|a| (a.i16("ApiKey"), a.i16("MinVersion"), a.i16("MaxVersion"))).collect();
        assert_eq!(keys.len(), api::SERVED.len());
        for a in api::SERVED {
            assert!(keys.contains(&(a.key, a.min, a.max)), "{} missing", a.name);
        }
    }
    // not offered: transactions, ACLs, DeleteRecords, offset for leader epoch, ...
    let served: Vec<i16> = api::SERVED.iter().map(|a| a.key).collect();
    for key in [21, 23, 24, 25, 26, 27, 28, 29, 30, 31, 33, 34, 38, 42, 44, 47, 60] {
        assert!(!served.contains(&key), "api key {key} must not be offered");
    }
}

#[tokio::test]
async fn api_versions_above_the_served_ones_is_answered_in_v0_with_the_error_and_the_list() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    // the client sends v5 (flexible, like v3 and v4); the broker answers with the v0 layout: error 35 and the key list
    let h = RequestHeader { api_key: api::API_VERSIONS, version: 3, correlation_id: 9, client_id: Some("new-client".into()) };
    let mut frame = write_request(&h, &Struct::new().with("ClientSoftwareName", "t").with("ClientSoftwareVersion", "1"));
    frame[4..6].copy_from_slice(&api::API_VERSIONS.to_be_bytes());
    frame[6..8].copy_from_slice(&5i16.to_be_bytes());
    k.s.write_all(&frame).await.unwrap();
    let mut len = [0u8; 4];
    k.s.read_exact(&mut len).await.unwrap();
    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
    k.s.read_exact(&mut buf).await.unwrap();
    let (corr, r) = read_response(&RequestHeader { version: 0, ..h }, &buf).unwrap();
    assert_eq!(corr, 9);
    assert_eq!(r.i16("ErrorCode"), err::UNSUPPORTED_VERSION);
    assert!(r.structs("ApiKeys").any(|a| a.i16("ApiKey") == api::PRODUCE));
}

#[tokio::test]
async fn metadata_advertises_the_address_the_connection_came_in_on() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    let r = metadata(&mut k, Some(&[]), false).await;
    let b = r.structs("Brokers").next().unwrap();
    assert_eq!((b.str("Host"), b.i32("Port")), (Some("127.0.0.1"), env.srv.port() as i32));
    assert_eq!(b.i32("NodeId"), 0);
    assert_eq!(r.structs("Topics").count(), 0);
}

#[tokio::test]
async fn the_advertised_host_and_port_can_be_set() {
    let cfg = KafkaConfig { advertised_host: Some("kafka.example.test".into()), advertised_port: Some(9093) };
    let env = env_with(Shared::new(true, None), cfg).await;
    let mut k = K::connect(&env).await;
    let r = metadata(&mut k, Some(&[]), false).await;
    let b = r.structs("Brokers").next().unwrap();
    assert_eq!((b.str("Host"), b.i32("Port")), (Some("kafka.example.test"), 9093));
    let c = k.call(api::FIND_COORDINATOR, 3, Struct::new().with("Key", "g").with("KeyType", 0)).await;
    assert_eq!((c.str("Host"), c.i32("Port"), c.i16("ErrorCode")), (Some("kafka.example.test"), 9093, 0));
}

#[tokio::test]
async fn metadata_creates_a_missing_topic_only_when_auto_creation_is_on_and_allowed() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    let r = metadata(&mut k, Some(&["made"]), false).await;
    assert_eq!(r.structs("Topics").next().unwrap().i16("ErrorCode"), err::UNKNOWN_TOPIC_OR_PARTITION);
    assert!(env.shared.state.lock().hub("default", "made").is_none());
    let r = metadata(&mut k, Some(&["made"]), true).await;
    let t = r.structs("Topics").next().unwrap();
    assert_eq!((t.i16("ErrorCode"), t.structs("Partitions").count()), (0, 4));
    assert!(env.shared.state.lock().hub("default", "made").is_some());
    // all topics
    let r = metadata(&mut k, None, false).await;
    assert_eq!(r.structs("Topics").count(), 1);
    // an invalid name
    let r = metadata(&mut k, Some(&["bad name"]), true).await;
    assert_eq!(r.structs("Topics").next().unwrap().i16("ErrorCode"), err::INVALID_TOPIC_EXCEPTION);

    let off = env_with(Shared::new(false, None), KafkaConfig::default()).await;
    let mut k = K::connect(&off).await;
    let r = metadata(&mut k, Some(&["nope"]), true).await;
    assert_eq!(r.structs("Topics").next().unwrap().i16("ErrorCode"), err::UNKNOWN_TOPIC_OR_PARTITION);
    assert!(off.shared.state.lock().hub("default", "nope").is_none());
}

#[tokio::test]
async fn metadata_older_versions() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 2, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    for v in [1, 4, 8] {
        let r = k.call(api::METADATA, v, Struct::new().with("Topics", topics_of(&["t"])).with("AllowAutoTopicCreation", false)).await;
        assert_eq!(r.structs("Topics").next().unwrap().structs("Partitions").count(), 2, "v{v}");
    }
}

// ---- produce, fetch, list offsets ------------------------------------------------------------------------------

#[tokio::test]
async fn produce_then_fetch_returns_the_records_with_offsets_equal_to_sequence_numbers() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 2, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    assert_eq!(produce(&mut k, "t", 1, &batch(0, &["a", "b", "c"])).await, (0, 0));
    assert_eq!(produce(&mut k, "t", 1, &batch(0, &["d"])).await, (0, 3));
    assert_eq!(produce(&mut k, "t", 0, &batch(0, &["x"])).await, (0, 0));
    let (e, high, start, recs) = fetch(&mut k, "t", 1, 0, 0, 1).await;
    assert_eq!((e, high, start), (0, 4, 0));
    let vals: Vec<_> = recs.iter().map(|(o, r)| (*o, String::from_utf8_lossy(r.value.as_ref().unwrap()).into_owned())).collect();
    assert_eq!(vals, vec![(0, "a".into()), (1, "b".into()), (2, "c".into()), (3, "d".into())]);
    // from the middle, and at the end
    assert_eq!(fetch(&mut k, "t", 1, 2, 0, 1).await.3.len(), 2);
    let (e, high, _, recs) = fetch(&mut k, "t", 1, 4, 0, 1).await;
    assert_eq!((e, high, recs.len()), (0, 4, 0));
    // beyond the end, and a partition that does not exist
    assert_eq!(fetch(&mut k, "t", 1, 5, 0, 1).await.0, err::OFFSET_OUT_OF_RANGE);
    assert_eq!(fetch(&mut k, "t", 7, 0, 0, 1).await.0, err::UNKNOWN_TOPIC_OR_PARTITION);
    assert_eq!(fetch(&mut k, "missing", 0, 0, 0, 1).await.0, err::UNKNOWN_TOPIC_OR_PARTITION);
    // produce to a topic or partition that does not exist
    assert_eq!(produce(&mut k, "missing", 0, &batch(0, &["a"])).await.0, err::UNKNOWN_TOPIC_OR_PARTITION);
    assert_eq!(produce(&mut k, "t", 5, &batch(0, &["a"])).await.0, err::UNKNOWN_TOPIC_OR_PARTITION);
    // the hub holds events: sequence number = offset
    let st = env.shared.state.lock();
    let p = &st.hub("default", "t").unwrap().partitions[1];
    assert_eq!((p.first_seq(), p.last_seq(), p.events.len()), (0, 3, 4));
}

#[tokio::test]
async fn records_become_events_and_events_become_records() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let mut b = batch(0, &["body"]);
    b.records[0].key = Some(Bytes::from_static(b"the-key"));
    b.records[0].headers = vec![Header { key: "h".into(), value: Some(Bytes::from_static(b"\x00\x01")) }];
    assert_eq!(produce(&mut k, "t", 0, &b).await, (0, 0));
    {
        let st = env.shared.state.lock();
        let ev = st.hub("default", "t").unwrap().partitions[0].events[0].clone();
        assert_eq!(ev.message.annotation("x-opt-partition-key").and_then(|v| v.as_str()), Some("the-key"));
        assert_eq!(ev.message.application_properties.as_ref().unwrap()[0].0.as_str(), Some("h"));
        assert_eq!(ev.partition_key.as_deref(), Some("the-key"));
    }
    // an event put in by another path (what AMQP does) comes out as a record: partition key, body, properties
    env.shared
        .enqueue_event(
            "default",
            "t",
            azemu_eventhubs::enqueue::EnqueueEvent {
                body: b"from amqp".to_vec(),
                content_type: Some("text/plain".into()),
                application_properties: vec![("p".into(), "str".into())],
            },
        )
        .unwrap();
    let (_, _, _, recs) = fetch(&mut k, "t", 0, 0, 0, 1).await;
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].1.key.as_deref(), Some(&b"the-key"[..]));
    assert_eq!(recs[0].1.headers, vec![Header { key: "h".into(), value: Some(Bytes::from_static(b"\x00\x01")) }]);
    assert_eq!(recs[1].1.value.as_deref(), Some(&b"from amqp"[..]));
    let h: std::collections::HashMap<_, _> = recs[1].1.headers.iter().map(|h| (h.key.clone(), h.value.clone().unwrap())).collect();
    assert_eq!(&h["content-type"][..], b"text/plain");
    assert_eq!(&h["p"][..], &[0xA1, 3, b's', b't', b'r']); // AMQP encoded string, type byte included
}

#[tokio::test]
async fn gzip_batches_are_stored_event_by_event_and_other_codecs_are_refused() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let r = k.call(api::PRODUCE, 7, produce_body("t", 0, encode_batch(&batch(0, &["one", "two", "three"]), true))).await;
    assert_eq!(r.structs("Responses").next().unwrap().structs("PartitionResponses").next().unwrap().i16("ErrorCode"), 0);
    let (_, high, _, recs) = fetch(&mut k, "t", 0, 0, 0, 1).await;
    assert_eq!((high, recs.len()), (3, 3));
    // a batch with the snappy bit set (the bytes are not snappy; the codec is refused first): the error Event Hubs gives
    let mut wire = encode_batch(&batch(0, &["x"]), false);
    wire[22] |= 2;
    let crc = {
        // recompute the CRC-32C over attributes..end
        azemu_kafka::crc32c::crc32c(&wire[21..])
    };
    wire[17..21].copy_from_slice(&crc.to_be_bytes());
    let r = k.call(api::PRODUCE, 7, produce_body("t", 0, wire)).await;
    assert_eq!(
        r.structs("Responses").next().unwrap().structs("PartitionResponses").next().unwrap().i16("ErrorCode"),
        err::UNSUPPORTED_FOR_MESSAGE_FORMAT
    );
    // a corrupt batch
    let mut wire = encode_batch(&batch(0, &["x"]), false);
    let n = wire.len();
    wire[n - 1] ^= 0xFF;
    let r = k.call(api::PRODUCE, 7, produce_body("t", 0, wire)).await;
    assert_eq!(r.structs("Responses").next().unwrap().structs("PartitionResponses").next().unwrap().i16("ErrorCode"), err::CORRUPT_MESSAGE);
    // nothing of the refused batches was stored
    assert_eq!(fetch(&mut k, "t", 0, 0, 0, 1).await.1, 3);
}

#[tokio::test]
async fn a_batch_over_the_event_size_limit_is_refused() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let big = "x".repeat(1_100_000);
    assert_eq!(produce(&mut k, "t", 0, &batch(0, &[&big])).await.0, err::MESSAGE_TOO_LARGE);
    assert_eq!(fetch(&mut k, "t", 0, 0, 0, 1).await.1, 0);
}

#[tokio::test]
async fn acks_zero_gets_no_answer() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let mut body = produce_body("t", 0, encode_batch(&batch(0, &["fire and forget"]), false));
    body.set("Acks", 0);
    // no response is written; the next request on the connection is answered
    k.corr += 1;
    let h = RequestHeader { api_key: api::PRODUCE, version: 7, correlation_id: k.corr, client_id: None };
    k.s.write_all(&write_request(&h, &body)).await.unwrap();
    let (_, high, _, _) = {
        // fetch waits for it: the order of requests on one connection is kept
        fetch(&mut k, "t", 0, 0, 0, 1).await
    };
    assert_eq!(high, 1);
}

#[tokio::test]
async fn fetch_waits_for_data_without_holding_the_state_lock() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let started = std::time::Instant::now();
    let waiter = tokio::spawn(async move {
        let r = fetch(&mut k, "t", 0, 0, 10_000, 1).await;
        (r, started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    // the state lock is free while the fetch waits: another connection (and the admin side) works at once
    let mut other = K::connect(&env).await;
    let t = std::time::Instant::now();
    assert_eq!(produce(&mut other, "t", 0, &batch(0, &["wake up"])).await, (0, 0));
    assert!(t.elapsed() < Duration::from_secs(2));
    let ((e, high, _, recs), took) = waiter.await.unwrap();
    assert_eq!((e, high, recs.len()), (0, 1, 1));
    assert!(took < Duration::from_secs(5), "the fetch returned when the event arrived, not at max wait: {took:?}");
}

#[tokio::test]
async fn fetch_returns_empty_at_max_wait_and_honours_min_bytes() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let t = std::time::Instant::now();
    let (e, high, _, recs) = fetch(&mut k, "t", 0, 0, 400, 1).await;
    assert_eq!((e, high, recs.len()), (0, 0, 0));
    assert!(t.elapsed() >= Duration::from_millis(350) && t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}

#[tokio::test]
async fn fetch_limits_the_bytes_but_always_returns_one_event() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let value = "y".repeat(10_000);
    let vals: Vec<&str> = (0..50).map(|_| value.as_str()).collect();
    produce(&mut k, "t", 0, &batch(0, &vals)).await;
    let mut body = fetch_body("t", 0, 0, 0, 1);
    body.set("MaxBytes", 35_000);
    let r = k.call(api::FETCH, 11, body).await;
    let p = r.structs("Responses").next().unwrap().structs("Partitions").next().unwrap().clone();
    let (b, _) = decode_batch(p.bytes("Records").unwrap(), 1 << 24).unwrap();
    assert!(b.records.len() >= 3 && b.records.len() <= 4, "{} records for 35000 bytes of 10000-byte events", b.records.len());
    // a limit smaller than one event still returns that event, so the reader can move on
    let mut body = fetch_body("t", 0, 0, 0, 1);
    body.set("MaxBytes", 10);
    let r = k.call(api::FETCH, 11, body).await;
    let p = r.structs("Responses").next().unwrap().structs("Partitions").next().unwrap().clone();
    assert_eq!(decode_batch(p.bytes("Records").unwrap(), 1 << 24).unwrap().0.records.len(), 1);
}

#[tokio::test]
async fn list_offsets_earliest_latest_and_by_time() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let list = |ts: i64| {
        Struct::new().with("ReplicaId", -1).with("IsolationLevel", 0).with(
            "Topics",
            vec![Struct::new().with("Name", "t").with("Partitions", vec![Struct::new().with("PartitionIndex", 0).with("Timestamp", ts)])],
        )
    };
    let one = |r: &Struct| {
        let p = r.structs("Topics").next().unwrap().structs("Partitions").next().unwrap().clone();
        (p.i16("ErrorCode"), p.i64("Offset"), p.i64("Timestamp"))
    };
    assert_eq!(one(&k.call(api::LIST_OFFSETS, 5, list(-2)).await), (0, 0, -1));
    assert_eq!(one(&k.call(api::LIST_OFFSETS, 5, list(-1)).await), (0, 0, -1));
    let before = now_ms() - 10_000;
    produce(&mut k, "t", 0, &batch(0, &["a", "b"])).await;
    assert_eq!(one(&k.call(api::LIST_OFFSETS, 5, list(-1)).await).1, 2);
    assert_eq!(one(&k.call(api::LIST_OFFSETS, 5, list(-2)).await).1, 0);
    assert_eq!(one(&k.call(api::LIST_OFFSETS, 5, list(before)).await).1, 0);
    assert_eq!(one(&k.call(api::LIST_OFFSETS, 5, list(now_ms() + 3_600_000)).await), (0, -1, -1));
}

#[tokio::test]
async fn retention_moves_the_log_start_and_a_reader_below_it_is_out_of_range() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 1, now_ms()).unwrap(); // one millisecond
    let mut k = K::connect(&env).await;
    produce(&mut k, "t", 0, &batch(0, &["a", "b"])).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (e, high, start, _) = fetch(&mut k, "t", 0, 0, 0, 1).await;
    assert_eq!((e, high, start), (err::OFFSET_OUT_OF_RANGE, 2, 2), "the events expired; sequence numbers go on");
    assert_eq!(produce(&mut k, "t", 0, &batch(0, &["c"])).await.1, 2);
}

// ---- idempotent producers ---------------------------------------------------------------------------------------

fn idem(base_seq: i32, epoch: i16, values: &[&str]) -> Batch {
    Batch { producer_id: 77, producer_epoch: epoch, base_sequence: base_seq, ..batch(0, values) }
}

#[tokio::test]
async fn init_producer_id_hands_out_ids_and_refuses_transactions() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    let a = k.call(api::INIT_PRODUCER_ID, 3, Struct::new().with("TransactionalId", Value::Null).with("TransactionTimeoutMs", 60000)).await;
    let b = k.call(api::INIT_PRODUCER_ID, 3, Struct::new().with("TransactionalId", Value::Null).with("TransactionTimeoutMs", 60000)).await;
    assert_eq!((a.i16("ErrorCode"), a.i16("ProducerEpoch")), (0, 0));
    assert_ne!(a.i64("ProducerId"), b.i64("ProducerId"));
    // a bump of the epoch keeps the id
    let c = k
        .call(
            api::INIT_PRODUCER_ID,
            3,
            Struct::new()
                .with("TransactionalId", Value::Null)
                .with("TransactionTimeoutMs", 60000)
                .with("ProducerId", a.i64("ProducerId"))
                .with("ProducerEpoch", 0),
        )
        .await;
    assert_eq!((c.i64("ProducerId"), c.i16("ProducerEpoch")), (a.i64("ProducerId"), 1));
    let t = k.call(api::INIT_PRODUCER_ID, 3, Struct::new().with("TransactionalId", "tx-1").with("TransactionTimeoutMs", 60000)).await;
    assert_eq!(t.i16("ErrorCode"), err::UNSUPPORTED_VERSION);
}

#[tokio::test]
async fn idempotent_producer_duplicates_are_dropped_and_answered_with_the_original_offset() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    assert_eq!(produce(&mut k, "t", 0, &idem(0, 0, &["a", "b"])).await, (0, 0));
    assert_eq!(produce(&mut k, "t", 0, &idem(2, 0, &["c"])).await, (0, 2));
    // the client retries the first batch (the answer was lost): same offset, nothing stored twice
    assert_eq!(produce(&mut k, "t", 0, &idem(0, 0, &["a", "b"])).await, (0, 0));
    assert_eq!(produce(&mut k, "t", 0, &idem(2, 0, &["c"])).await, (0, 2));
    assert_eq!(fetch(&mut k, "t", 0, 0, 0, 1).await.1, 3);
    // a gap in the sequence
    assert_eq!(produce(&mut k, "t", 0, &idem(9, 0, &["x"])).await.0, err::OUT_OF_ORDER_SEQUENCE_NUMBER);
    // the next in line is still accepted
    assert_eq!(produce(&mut k, "t", 0, &idem(3, 0, &["d"])).await, (0, 3));
    // a lower epoch is fenced, a higher one starts again
    assert_eq!(produce(&mut k, "t", 0, &idem(4, 1, &["e"])).await, (0, 4));
    assert_eq!(produce(&mut k, "t", 0, &idem(5, 0, &["old epoch"])).await.0, err::INVALID_PRODUCER_EPOCH);
    assert_eq!(fetch(&mut k, "t", 0, 0, 0, 1).await.1, 5);
}

#[tokio::test]
async fn sequences_are_per_partition_and_per_producer() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 2, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    assert_eq!(produce(&mut k, "t", 0, &idem(0, 0, &["a"])).await, (0, 0));
    assert_eq!(produce(&mut k, "t", 1, &idem(0, 0, &["b"])).await, (0, 0)); // partition 1 starts at 0 too
    let other = Batch { producer_id: 78, ..idem(0, 0, &["c"]) };
    assert_eq!(produce(&mut k, "t", 0, &other).await, (0, 1)); // another producer id
}

// ---- groups -------------------------------------------------------------------------------------------------------

fn join_body(group: &str, member: &str, protocols: &[&str], session_ms: i32) -> Struct {
    Struct::new()
        .with("GroupId", group.to_string())
        .with("SessionTimeoutMs", session_ms)
        .with("RebalanceTimeoutMs", 10_000)
        .with("MemberId", member.to_string())
        .with("GroupInstanceId", Value::Null)
        .with("ProtocolType", "consumer")
        .with(
            "Protocols",
            protocols
                .iter()
                .map(|p| Struct::new().with("Name", p.to_string()).with("Metadata", format!("meta-{p}").into_bytes()))
                .collect::<Vec<_>>(),
        )
}

fn sync_body(group: &str, generation: i32, member: &str, assignments: &[(&str, &str)]) -> Struct {
    Struct::new()
        .with("GroupId", group.to_string())
        .with("GenerationId", generation)
        .with("MemberId", member.to_string())
        .with("GroupInstanceId", Value::Null)
        .with(
            "Assignments",
            assignments
                .iter()
                .map(|(m, a)| Struct::new().with("MemberId", m.to_string()).with("Assignment", a.as_bytes().to_vec()))
                .collect::<Vec<_>>(),
        )
}

async fn join_with_id(env: &Env, group: &str, protocols: &[&str]) -> (K, Struct) {
    let mut k = K::connect(env).await;
    // before v4 the broker does not ask the client to come back with its id
    let r = k.call(api::JOIN_GROUP, 3, join_body(group, "", protocols, 10_000)).await;
    (k, r)
}

#[tokio::test]
async fn one_member_joins_syncs_heartbeats_and_leaves() {
    let env = env().await;
    let (mut k, j) = join_with_id(&env, "g1", &["range"]).await;
    assert_eq!(j.i16("ErrorCode"), 0);
    let member = j.str("MemberId").unwrap().to_string();
    assert_eq!((j.i32("GenerationId"), j.str("ProtocolName"), j.str("Leader")), (1, Some("range"), Some(member.as_str())));
    assert_eq!(j.structs("Members").count(), 1);
    assert_eq!(j.structs("Members").next().unwrap().bytes("Metadata").unwrap().as_ref(), b"meta-range");
    let s = k.call(api::SYNC_GROUP, 3, sync_body("g1", 1, &member, &[(&member, "all partitions")])).await;
    assert_eq!((s.i16("ErrorCode"), s.bytes("Assignment").unwrap().as_ref()), (0, &b"all partitions"[..]));
    let hb = |g, m: &str| Struct::new().with("GroupId", "g1").with("GenerationId", g).with("MemberId", m.to_string());
    assert_eq!(k.call(api::HEARTBEAT, 3, hb(1, &member)).await.i16("ErrorCode"), 0);
    assert_eq!(k.call(api::HEARTBEAT, 3, hb(7, &member)).await.i16("ErrorCode"), err::ILLEGAL_GENERATION);
    assert_eq!(k.call(api::HEARTBEAT, 3, hb(1, "ghost")).await.i16("ErrorCode"), err::UNKNOWN_MEMBER_ID);
    let d = k.call(api::DESCRIBE_GROUPS, 4, Struct::new().with("Groups", vec!["g1".to_string()])).await;
    let g = d.structs("Groups").next().unwrap();
    assert_eq!((g.str("GroupState"), g.str("ProtocolType"), g.str("ProtocolData")), (Some("Stable"), Some("consumer"), Some("range")));
    assert_eq!(g.structs("Members").next().unwrap().bytes("MemberAssignment").unwrap().as_ref(), b"all partitions");
    let l = k.call(api::LIST_GROUPS, 3, Struct::new()).await;
    assert_eq!(l.structs("Groups").next().unwrap().str("GroupId"), Some("g1"));
    let lv = k
        .call(
            api::LEAVE_GROUP,
            3,
            Struct::new()
                .with("GroupId", "g1")
                .with("Members", vec![Struct::new().with("MemberId", member.clone()).with("GroupInstanceId", Value::Null)]),
        )
        .await;
    assert_eq!(lv.structs("Members").next().unwrap().i16("ErrorCode"), 0);
    let d = k.call(api::DESCRIBE_GROUPS, 4, Struct::new().with("Groups", vec!["g1".to_string()])).await;
    assert_eq!(d.structs("Groups").next().unwrap().str("GroupState"), Some("Empty"));
}

#[tokio::test]
async fn join_v4_asks_a_new_member_to_come_back_with_its_id() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    let r = k.call(api::JOIN_GROUP, 6, join_body("g2", "", &["range"], 10_000)).await;
    assert_eq!(r.i16("ErrorCode"), err::MEMBER_ID_REQUIRED);
    let id = r.str("MemberId").unwrap().to_string();
    assert!(!id.is_empty());
    let r = k.call(api::JOIN_GROUP, 6, join_body("g2", &id, &["range"], 10_000)).await;
    assert_eq!((r.i16("ErrorCode"), r.str("MemberId")), (0, Some(id.as_str())));
    // an id the broker never handed out is unknown
    let r = k.call(api::JOIN_GROUP, 6, join_body("g2", "made-up", &["range"], 10_000)).await;
    assert_eq!(r.i16("ErrorCode"), err::UNKNOWN_MEMBER_ID);
}

#[tokio::test]
async fn join_validates_the_group_and_the_timeouts_and_the_protocols() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    assert_eq!(k.call(api::JOIN_GROUP, 3, join_body("", "", &["range"], 10_000)).await.i16("ErrorCode"), err::INVALID_GROUP_ID);
    assert_eq!(
        k.call(api::JOIN_GROUP, 3, join_body(&"g".repeat(257), "", &["range"], 10_000)).await.i16("ErrorCode"),
        err::INVALID_GROUP_ID
    );
    assert_eq!(k.call(api::JOIN_GROUP, 3, join_body("g", "", &["range"], 100)).await.i16("ErrorCode"), err::INVALID_SESSION_TIMEOUT);
    assert_eq!(k.call(api::JOIN_GROUP, 3, join_body("g", "", &["range"], 99_999_999)).await.i16("ErrorCode"), err::INVALID_SESSION_TIMEOUT);
    assert_eq!(k.call(api::JOIN_GROUP, 3, join_body("g", "", &[], 10_000)).await.i16("ErrorCode"), err::INCONSISTENT_GROUP_PROTOCOL);
}

#[tokio::test]
async fn two_members_rebalance_and_the_leader_assignment_is_passed_through() {
    let env = env().await;
    let (mut k1, j1) = join_with_id(&env, "g3", &["range", "roundrobin"]).await;
    let m1 = j1.str("MemberId").unwrap().to_string();
    assert_eq!(j1.i32("GenerationId"), 1);
    let s1 = k1.call(api::SYNC_GROUP, 3, sync_body("g3", 1, &m1, &[(&m1, "p-all")])).await;
    assert_eq!(s1.i16("ErrorCode"), 0);
    // member 2 joins: the broker starts a rebalance; member 1 learns of it from its heartbeat and rejoins
    let mut k2 = K::connect(&env).await;
    let join2 = tokio::spawn(async move {
        let r = k2.call(api::JOIN_GROUP, 3, join_body("g3", "", &["roundrobin", "range"], 10_000)).await;
        (k2, r)
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let hb = k1.call(api::HEARTBEAT, 3, Struct::new().with("GroupId", "g3").with("GenerationId", 1).with("MemberId", m1.clone())).await;
    assert_eq!(hb.i16("ErrorCode"), err::REBALANCE_IN_PROGRESS);
    let j1b = k1.call(api::JOIN_GROUP, 3, join_body("g3", &m1, &["range", "roundrobin"], 10_000)).await;
    let (mut k2, j2) = join2.await.unwrap();
    let m2 = j2.str("MemberId").unwrap().to_string();
    assert_eq!((j1b.i32("GenerationId"), j2.i32("GenerationId")), (2, 2));
    // both prefer different protocols; they agree on one both support (each votes for its first among the common ones)
    assert_eq!(j1b.str("ProtocolName"), j2.str("ProtocolName"));
    assert_eq!(j1b.str("Leader"), Some(m1.as_str()), "the earlier leader stays leader");
    assert_eq!(j1b.structs("Members").count(), 2);
    assert_eq!(j2.structs("Members").count(), 0, "only the leader gets the member list");
    // the follower syncs first and waits for the leader's assignment
    let follower = tokio::spawn({
        let m2 = m2.clone();
        async move {
            let s = k2.call(api::SYNC_GROUP, 3, sync_body("g3", 2, &m2, &[])).await;
            (k2, s)
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let s1 = k1.call(api::SYNC_GROUP, 3, sync_body("g3", 2, &m1, &[(&m1, "p0,p1"), (&m2, "p2,p3")])).await;
    assert_eq!(s1.bytes("Assignment").unwrap().as_ref(), b"p0,p1");
    let (mut k2, s2) = follower.await.unwrap();
    assert_eq!((s2.i16("ErrorCode"), s2.bytes("Assignment").unwrap().as_ref()), (0, &b"p2,p3"[..]));
    // member 2 leaves: member 1 is told to rejoin and gets the generation after
    let lv = k2
        .call(
            api::LEAVE_GROUP,
            3,
            Struct::new()
                .with("GroupId", "g3")
                .with("Members", vec![Struct::new().with("MemberId", m2.clone()).with("GroupInstanceId", Value::Null)]),
        )
        .await;
    assert_eq!(lv.i16("ErrorCode"), 0);
    let hb = k1.call(api::HEARTBEAT, 3, Struct::new().with("GroupId", "g3").with("GenerationId", 2).with("MemberId", m1.clone())).await;
    assert_eq!(hb.i16("ErrorCode"), err::REBALANCE_IN_PROGRESS);
    let j = k1.call(api::JOIN_GROUP, 3, join_body("g3", &m1, &["range", "roundrobin"], 10_000)).await;
    assert_eq!((j.i16("ErrorCode"), j.i32("GenerationId"), j.structs("Members").count()), (0, 3, 1));
}

#[tokio::test]
async fn offsets_commit_fetch_and_survive_a_restart_of_the_consumer() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 2, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let commit = |gen: i32, member: &str, topic: &str, part: i32, off: i64, meta: Option<&str>| {
        Struct::new()
            .with("GroupId", "g4")
            .with("GenerationIdOrMemberEpoch", gen)
            .with("MemberId", member.to_string())
            .with("GroupInstanceId", Value::Null)
            .with(
                "Topics",
                vec![Struct::new().with("Name", topic.to_string()).with(
                    "Partitions",
                    vec![Struct::new()
                        .with("PartitionIndex", part)
                        .with("CommittedOffset", off)
                        .with("CommittedLeaderEpoch", -1)
                        .with("CommittedMetadata", meta.map(str::to_string))],
                )],
            )
    };
    let code = |r: &Struct| r.structs("Topics").next().unwrap().structs("Partitions").next().unwrap().i16("ErrorCode");
    // a commit without a member (an admin tool, or a consumer that assigns by itself)
    assert_eq!(code(&k.call(api::OFFSET_COMMIT, 7, commit(-1, "", "t", 0, 5, Some("meta"))).await), 0);
    assert_eq!(code(&k.call(api::OFFSET_COMMIT, 7, commit(-1, "", "t", 9, 5, None)).await), err::UNKNOWN_TOPIC_OR_PARTITION);
    assert_eq!(code(&k.call(api::OFFSET_COMMIT, 7, commit(-1, "", "nope", 0, 5, None)).await), err::UNKNOWN_TOPIC_OR_PARTITION);
    assert_eq!(
        code(&k.call(api::OFFSET_COMMIT, 7, commit(-1, "", "t", 0, 5, Some(&"m".repeat(1025)))).await),
        err::OFFSET_METADATA_TOO_LARGE
    );
    let fetch_offsets = |topics: Value| Struct::new().with("GroupId", "g4").with("Topics", topics);
    let r = k
        .call(
            api::OFFSET_FETCH,
            6,
            fetch_offsets(Value::Array(vec![Value::Struct(Struct::new().with("Name", "t").with("PartitionIndexes", vec![0i32, 1]))])),
        )
        .await;
    let ps: Vec<_> = r
        .structs("Topics")
        .next()
        .unwrap()
        .structs("Partitions")
        .map(|p| (p.i32("PartitionIndex"), p.i64("CommittedOffset"), p.str("Metadata").map(str::to_string)))
        .collect();
    assert_eq!(ps, vec![(0, 5, Some("meta".to_string())), (1, -1, Some(String::new()))]);
    // every committed partition of the group
    let r = k.call(api::OFFSET_FETCH, 6, fetch_offsets(Value::Null)).await;
    assert_eq!(r.structs("Topics").next().unwrap().structs("Partitions").count(), 1);
    // a member commit needs the member and the generation
    let (mut kk, j) = join_with_id(&env, "g4", &["range"]).await;
    let m = j.str("MemberId").unwrap().to_string();
    assert_eq!(code(&kk.call(api::OFFSET_COMMIT, 7, commit(1, &m, "t", 1, 3, None)).await), 0);
    assert_eq!(code(&kk.call(api::OFFSET_COMMIT, 7, commit(5, &m, "t", 1, 4, None)).await), err::ILLEGAL_GENERATION);
    assert_eq!(code(&kk.call(api::OFFSET_COMMIT, 7, commit(1, "ghost", "t", 1, 4, None)).await), err::UNKNOWN_MEMBER_ID);
    // the member leaves: the offsets stay
    kk.call(api::LEAVE_GROUP, 2, Struct::new().with("GroupId", "g4").with("MemberId", m)).await;
    let r = k.call(api::OFFSET_FETCH, 6, fetch_offsets(Value::Null)).await;
    assert_eq!(r.structs("Topics").next().unwrap().structs("Partitions").count(), 2);
    // deleting the topic drops its offsets
    k.call(api::DELETE_TOPICS, 4, Struct::new().with("TopicNames", vec!["t".to_string()]).with("TimeoutMs", 1000)).await;
    let r = k.call(api::OFFSET_FETCH, 6, fetch_offsets(Value::Null)).await;
    assert_eq!(r.structs("Topics").count(), 0);
}

#[tokio::test]
async fn offsets_go_when_the_hub_or_the_namespace_is_deleted_by_any_door_not_only_kafka() {
    // ARM and the admin API delete through `Shared::delete_hub` / `delete_namespace`; a hub made again must not inherit old offsets
    let env = env().await;
    for ns in ["default", "other"] {
        env.shared.state.lock().create_hub(ns, "t", 1, 3_600_000, now_ms()).unwrap();
    }
    let mut k = K::connect(&env).await;
    let commit = |topic: &str| {
        Struct::new()
            .with("GroupId", "g9")
            .with("GenerationIdOrMemberEpoch", -1)
            .with("MemberId", "")
            .with("GroupInstanceId", Value::Null)
            .with(
                "Topics",
                vec![Struct::new().with("Name", topic.to_string()).with(
                    "Partitions",
                    vec![Struct::new()
                        .with("PartitionIndex", 0)
                        .with("CommittedOffset", 7i64)
                        .with("CommittedLeaderEpoch", -1)
                        .with("CommittedMetadata", Value::Null)],
                )],
            )
    };
    let committed = || env.shared.kafka.all_offsets("default", "g9").len();
    k.call(api::OFFSET_COMMIT, 7, commit("t")).await;
    assert_eq!(committed(), 1);
    assert!(env.shared.delete_hub("default", "t"));
    assert_eq!(committed(), 0, "the offsets of a deleted hub are gone");
    // a namespace takes its hubs and its groups
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    k.call(api::OFFSET_COMMIT, 7, commit("t")).await;
    env.shared.delete_namespace("default");
    assert!(env.shared.kafka.list("default").is_empty());
    assert!(env.shared.state.lock().hub("other", "t").is_some(), "another namespace is not touched");
}

#[tokio::test]
async fn find_coordinator_checks_the_key() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    assert_eq!(k.call(api::FIND_COORDINATOR, 3, Struct::new().with("Key", "g").with("KeyType", 0)).await.i16("ErrorCode"), 0);
    assert_eq!(
        k.call(api::FIND_COORDINATOR, 3, Struct::new().with("Key", "").with("KeyType", 0)).await.i16("ErrorCode"),
        err::INVALID_GROUP_ID
    );
    // transactions are not built
    assert_eq!(
        k.call(api::FIND_COORDINATOR, 3, Struct::new().with("Key", "tx").with("KeyType", 1)).await.i16("ErrorCode"),
        err::COORDINATOR_NOT_AVAILABLE
    );
}

// ---- admin ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_and_delete_topics_and_describe_configs() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    let create = |name: &str, parts: i32, cfg: Vec<(&str, &str)>| {
        Struct::new().with("TimeoutMs", 1000).with("validateOnly", false).with(
            "Topics",
            vec![Struct::new()
                .with("Name", name.to_string())
                .with("NumPartitions", parts)
                .with("ReplicationFactor", 1)
                .with("Assignments", Vec::<Struct>::new())
                .with(
                    "Configs",
                    cfg.into_iter()
                        .map(|(k, v)| Struct::new().with("Name", k.to_string()).with("Value", v.to_string()))
                        .collect::<Vec<_>>(),
                )],
        )
    };
    let code = |r: &Struct| r.structs("Topics").next().unwrap().i16("ErrorCode");
    let r = k.call(api::CREATE_TOPICS, 5, create("orders", 3, vec![("retention.ms", "7200000")])).await;
    assert_eq!(code(&r), 0);
    assert_eq!(r.structs("Topics").next().unwrap().i32("NumPartitions"), 3);
    {
        let st = env.shared.state.lock();
        let h = st.hub("default", "orders").unwrap();
        assert_eq!((h.partitions.len(), h.retention_ms), (3, 7_200_000));
    }
    assert_eq!(code(&k.call(api::CREATE_TOPICS, 5, create("orders", 3, vec![])).await), err::TOPIC_ALREADY_EXISTS);
    assert_eq!(code(&k.call(api::CREATE_TOPICS, 5, create("compact", 1, vec![("cleanup.policy", "compact")])).await), err::INVALID_CONFIG);
    assert!(env.shared.state.lock().hub("default", "compact").is_none());
    assert_eq!(code(&k.call(api::CREATE_TOPICS, 5, create("zero", 0, vec![])).await), err::INVALID_PARTITIONS);
    assert_eq!(code(&k.call(api::CREATE_TOPICS, 5, create("bad/name", 1, vec![])).await), err::INVALID_TOPIC_EXCEPTION);
    assert_eq!(code(&k.call(api::CREATE_TOPICS, 5, create("dflt", -1, vec![])).await), 0);
    assert_eq!(env.shared.state.lock().hub("default", "dflt").unwrap().partitions.len(), 4);
    assert_eq!(code(&k.call(api::CREATE_TOPICS, 2, create("older", 1, vec![])).await), 0);

    let d = k
        .call(
            api::DESCRIBE_CONFIGS,
            4,
            Struct::new().with(
                "Resources",
                vec![
                    Struct::new().with("ResourceType", 2).with("ResourceName", "orders").with("ConfigurationKeys", Value::Null),
                    Struct::new().with("ResourceType", 2).with("ResourceName", "ghost").with("ConfigurationKeys", Value::Null),
                ],
            ),
        )
        .await;
    let rs: Vec<&Struct> = d.structs("Results").collect();
    assert_eq!(rs[0].i16("ErrorCode"), 0);
    let ret = rs[0].structs("Configs").find(|c| c.str("Name") == Some("retention.ms")).unwrap();
    assert_eq!(ret.str("Value"), Some("7200000"));
    assert_eq!(rs[1].i16("ErrorCode"), err::UNKNOWN_TOPIC_OR_PARTITION);

    let del =
        |names: &[&str]| Struct::new().with("TopicNames", names.iter().map(|s| s.to_string()).collect::<Vec<_>>()).with("TimeoutMs", 1000);
    let r = k.call(api::DELETE_TOPICS, 4, del(&["orders", "ghost"])).await;
    let codes: Vec<i16> = r.structs("Responses").map(|t| t.i16("ErrorCode")).collect();
    assert_eq!(codes, vec![0, err::UNKNOWN_TOPIC_OR_PARTITION]);
    assert!(env.shared.state.lock().hub("default", "orders").is_none());
}

#[tokio::test]
async fn create_partitions_is_refused_because_an_event_hub_keeps_its_partition_count() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 2, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let r = k
        .call(
            api::CREATE_PARTITIONS,
            3,
            Struct::new()
                .with("Topics", vec![Struct::new().with("Name", "t").with("Count", 8).with("Assignments", Value::Null)])
                .with("TimeoutMs", 1000)
                .with("ValidateOnly", false),
        )
        .await;
    assert_eq!(r.structs("Results").next().unwrap().i16("ErrorCode"), err::INVALID_PARTITIONS);
    assert_eq!(env.shared.state.lock().hub("default", "t").unwrap().partitions.len(), 2);
}

// ---- SASL and namespaces ------------------------------------------------------------------------------------------

#[tokio::test]
async fn sasl_handshake_and_authenticate_accept_any_credentials() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    let r = k.call(api::SASL_HANDSHAKE, 1, Struct::new().with("Mechanism", "SCRAM-SHA-256")).await;
    assert_eq!(r.i16("ErrorCode"), err::UNSUPPORTED_SASL_MECHANISM);
    let mechs: Vec<String> = r.items("Mechanisms").iter().filter_map(|m| m.as_str().map(str::to_string)).collect();
    assert_eq!(mechs, vec!["PLAIN", "OAUTHBEARER"]);
    assert_eq!(
        k.call(api::SASL_AUTHENTICATE, 2, Struct::new().with("AuthBytes", b"\0u\0p".to_vec())).await.i16("ErrorCode"),
        err::ILLEGAL_SASL_STATE
    );
    assert_eq!(k.call(api::SASL_HANDSHAKE, 1, Struct::new().with("Mechanism", "PLAIN")).await.i16("ErrorCode"), 0);
    assert_eq!(
        k.call(api::SASL_AUTHENTICATE, 2, Struct::new().with("AuthBytes", b"no separators".to_vec())).await.i16("ErrorCode"),
        err::SASL_AUTHENTICATION_FAILED
    );
    let r = k.call(api::SASL_AUTHENTICATE, 2, Struct::new().with("AuthBytes", b"\0$ConnectionString\0anything at all".to_vec())).await;
    assert_eq!(r.i16("ErrorCode"), 0);

    let mut k = K::connect(&env).await;
    assert_eq!(k.call(api::SASL_HANDSHAKE, 1, Struct::new().with("Mechanism", "OAUTHBEARER")).await.i16("ErrorCode"), 0);
    let r = k.call(api::SASL_AUTHENTICATE, 2, Struct::new().with("AuthBytes", b"n,,\x01auth=Bearer eyJhbGciOi\x01\x01".to_vec())).await;
    assert_eq!(r.i16("ErrorCode"), 0);
}

#[tokio::test]
async fn a_sasl_password_with_multi_byte_characters_is_read_without_a_panic() {
    // Byte 9 of the password was inside a character, and a fixed slice panicked the connection's task
    let env = env().await;
    for password in ["éééééx", "日本語日本語", "😀😀😀😀😀", "Endpoint=sb://é.example/;k=日本", "ééééééééé=Endpoint=sb://h/"]
    {
        let mut k = K::connect(&env).await;
        assert_eq!(k.call(api::SASL_HANDSHAKE, 1, Struct::new().with("Mechanism", "PLAIN")).await.i16("ErrorCode"), 0);
        let mut bytes = b"\0$ConnectionString\0".to_vec();
        bytes.extend_from_slice(password.as_bytes());
        let r = k.call(api::SASL_AUTHENTICATE, 2, Struct::new().with("AuthBytes", bytes)).await;
        assert_eq!(r.i16("ErrorCode"), 0, "{password}");
        // the same connection goes on working
        assert_eq!(
            k.call(api::API_VERSIONS, 3, Struct::new().with("ClientSoftwareName", "t").with("ClientSoftwareVersion", "1"))
                .await
                .i16("ErrorCode"),
            0,
            "{password}"
        );
    }
}

#[tokio::test]
async fn the_namespace_comes_from_the_endpoint_in_the_sasl_password() {
    let env = env().await;
    env.shared.state.lock().create_hub("alpha", "only-in-alpha", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    assert_eq!(
        metadata(&mut k, Some(&["only-in-alpha"]), false).await.structs("Topics").next().unwrap().i16("ErrorCode"),
        err::UNKNOWN_TOPIC_OR_PARTITION
    );
    let mut k = K::connect(&env).await;
    k.call(api::SASL_HANDSHAKE, 1, Struct::new().with("Mechanism", "PLAIN")).await;
    k.call(
        api::SASL_AUTHENTICATE,
        2,
        Struct::new().with(
            "AuthBytes",
            b"\0$ConnectionString\0Endpoint=sb://alpha.eventhub.localhost:9093/;SharedAccessKeyName=k;SharedAccessKey=s".to_vec(),
        ),
    )
    .await;
    let r = metadata(&mut k, Some(&["only-in-alpha"]), false).await;
    assert_eq!(r.structs("Topics").next().unwrap().i16("ErrorCode"), 0);
    // and the broker address is that host (it names a local host), with the port the connection came in on
    let b = r.structs("Brokers").next().unwrap();
    assert_eq!((b.str("Host"), b.i32("Port")), (Some("alpha.eventhub.localhost"), env.srv.port() as i32));
}

// ---- reset, sniffing, bounds ---------------------------------------------------------------------------------------

#[tokio::test]
async fn reset_drops_topics_groups_and_waiting_fetches_end() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let (mut k, j) = join_with_id(&env, "gr", &["range"]).await;
    assert_eq!(j.i16("ErrorCode"), 0);
    let mut waiting = K::connect(&env).await;
    let w = tokio::spawn(async move { fetch(&mut waiting, "t", 0, 0, 20_000, 1).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    env.srv.reset();
    let (e, ..) = tokio::time::timeout(Duration::from_secs(5), w).await.expect("a reset ends a waiting fetch").unwrap();
    assert_eq!(e, err::UNKNOWN_TOPIC_OR_PARTITION);
    let d = k.call(api::DESCRIBE_GROUPS, 4, Struct::new().with("Groups", vec!["gr".to_string()])).await;
    assert_eq!(d.structs("Groups").next().unwrap().str("GroupState"), Some("Dead"));
}

#[test]
fn sniffing_claims_kafka_requests_and_nothing_else() {
    use azemu_eventhubs::kafka::looks_like_kafka;
    let h = RequestHeader { api_key: api::API_VERSIONS, version: 3, correlation_id: 1, client_id: Some("c".into()) };
    let frame = write_request(&h, &Struct::new().with("ClientSoftwareName", "x").with("ClientSoftwareVersion", "1"));
    assert!(looks_like_kafka(&frame));
    assert!(looks_like_kafka(&frame[..4]), "four bytes are enough when the length is plausible");
    assert!(looks_like_kafka(&frame[..8]));
    for not in [
        &b"GET / HTTP/1.1\r\n"[..],
        b"POST /x HTTP/1.1",
        b"PRI * HTTP/2.0\r\n\r\n",
        b"AMQP\x00\x01\x00\x00",
        b"AMQP\x03\x01\x00\x00",
        &[0x16, 3, 1, 0, 200, 1, 0, 0],
        b"",
        b"\0\0",
        b"\x00\x00\x00\x00\x00\x12\x00\x03",
        b"\x00\x00\x00\x40\x00\xff\x00\x03",
        b"\x00\x00\x00\x40\x00\x12\x7f\x03",
        b"\x01\x00\x00\x40\x00\x12\x00\x03",
    ] {
        assert!(!looks_like_kafka(not), "{:?}", String::from_utf8_lossy(not));
    }
}

#[tokio::test]
async fn http_and_amqp_still_work_on_the_same_port() {
    let env = env().await;
    let r = env.srv.send("GET", &format!("{}/_azemu/health", env.srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200);
    let r = env.srv.send("GET", &format!("{}/_azemu/kafka/default/groups", env.srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn garbage_and_oversized_requests_close_the_connection() {
    let env = env().await;
    // an unknown API key
    let mut k = K::connect(&env).await;
    let f = vec![0, 0, 0, 10, 0x00, 70, 0, 0, 0, 0, 0, 1, 0xFF, 0xFF]; // api key 70 is no API the broker serves
    assert!(k.closed_after(&f).await);
    // a version the server does not serve
    let mut k = K::connect(&env).await;
    let h = RequestHeader { api_key: api::METADATA, version: 0, correlation_id: 1, client_id: None };
    let mut w = azemu_kafka::wire::Writer::new();
    w.i32(10);
    w.i16(h.api_key);
    w.i16(h.version);
    w.i32(1);
    w.string(None);
    w.i32(0);
    assert!(k.closed_after(&w.buf).await);
    // a length over the limit (after a first good request: a first length of 16 MiB or more is not taken for Kafka at all):
    // closed at once, before any of the bytes are read
    let mut k = K::connect(&env).await;
    assert_eq!(metadata(&mut k, Some(&[]), false).await.structs("Brokers").count(), 1);
    let mut w = azemu_kafka::wire::Writer::new();
    w.i32(100 * 1024 * 1024);
    w.i16(api::METADATA);
    w.i16(9);
    assert!(k.closed_after(&w.buf).await);
    // a frame that ends inside a field
    let mut k = K::connect(&env).await;
    let mut w = azemu_kafka::wire::Writer::new();
    w.i32(14);
    w.i16(api::FETCH);
    w.i16(11);
    w.i32(1);
    w.string(None);
    w.i32(5);
    assert!(k.closed_after(&w.buf).await);
    // the server is fine
    let mut k = K::connect(&env).await;
    assert_eq!(metadata(&mut k, Some(&[]), false).await.structs("Brokers").count(), 1);
}

#[tokio::test]
async fn a_client_that_announces_a_large_request_and_sends_little_does_not_make_the_server_allocate_it() {
    // 15 MiB announced, 100 bytes sent: the server holds what arrived, not what was announced (it would otherwise
    // allocate 15 MiB per connection); here we check that many such connections are accepted and the server answers others.
    let env = env().await;
    let mut hanging = Vec::new();
    for _ in 0..50 {
        let mut k = K::connect(&env).await;
        let mut w = azemu_kafka::wire::Writer::new();
        w.i32(15 * 1024 * 1024);
        w.i16(api::PRODUCE);
        w.i16(7);
        w.raw(&[0u8; 100]);
        k.s.write_all(&w.buf).await.unwrap();
        hanging.push(k);
    }
    let mut k = K::connect(&env).await;
    assert_eq!(metadata(&mut k, Some(&[]), false).await.structs("Brokers").count(), 1);
}

#[tokio::test]
async fn pipelined_requests_are_answered_in_order() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    let mut out = Vec::new();
    for corr in 1..=20 {
        let h = RequestHeader { api_key: api::API_VERSIONS, version: 3, correlation_id: corr, client_id: None };
        out.extend_from_slice(&write_request(&h, &Struct::new().with("ClientSoftwareName", "x").with("ClientSoftwareVersion", "1")));
    }
    k.s.write_all(&out).await.unwrap();
    for corr in 1..=20 {
        let mut len = [0u8; 4];
        k.s.read_exact(&mut len).await.unwrap();
        let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
        k.s.read_exact(&mut buf).await.unwrap();
        assert_eq!(i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]), corr);
    }
}
