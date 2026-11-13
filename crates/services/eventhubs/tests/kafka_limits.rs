//! Bounds of the Kafka Produce path: what one request may make the emulator decode and store.
//! Every test here uses a few MB at most: the point is that the work stops at the limit, not after it.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use azemu_core::testing::TestServer;
use azemu_eventhubs::kafka::{endpoint_with, KafkaConfig};
use azemu_eventhubs::model::{EVENT_OVERHEAD, MAX_EVENTS_PER_BATCH};
use azemu_eventhubs::state::{now_ms, Shared};
use azemu_kafka::api::{self, err};
use azemu_kafka::header::{read_response, write_request};
use azemu_kafka::records::{encode_batch, encode_batch_raw, encode_record, Batch, Record};
use azemu_kafka::{RequestHeader, Struct, Value};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Env {
    srv: TestServer,
    shared: Arc<Shared>,
}

async fn env() -> Env {
    let shared = Shared::new(true, None);
    let (svc, amqp) = azemu_eventhubs::pair_with(shared.clone());
    let kafka = endpoint_with(shared.clone(), KafkaConfig::default());
    let srv = TestServer::start_with(vec![svc, kafka.clone()], vec![amqp, kafka.clone()]).await;
    Env { srv, shared }
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
}

fn header() -> Batch {
    Batch {
        partition_leader_epoch: -1,
        first_timestamp: now_ms(),
        max_timestamp: now_ms(),
        producer_id: -1,
        producer_epoch: -1,
        base_sequence: -1,
        ..Default::default()
    }
}

/// A batch of `n` records with null key and null value: 7 bytes each, the smallest legal record.
fn empty_records(n: usize, gzip: bool) -> Vec<u8> {
    let one = encode_record(&Record::default());
    assert_eq!(one.len(), 7);
    encode_batch_raw(&header(), n as i32, one.repeat(n), gzip)
}

/// `partitions` is (topic, partition, records bytes) per entry; answers (error, base offset) in the same order.
async fn produce_many(k: &mut K, entries: &[(&str, i32, Vec<u8>)]) -> Vec<(i16, i64)> {
    let mut topics: Vec<Struct> = Vec::new();
    for (topic, partition, records) in entries {
        let p = Struct::new().with("Index", *partition).with("Records", records.clone());
        topics.push(Struct::new().with("Name", topic.to_string()).with("PartitionData", vec![p]));
    }
    let body = Struct::new().with("TransactionalId", Value::Null).with("Acks", -1).with("TimeoutMs", 5000).with("TopicData", topics);
    let r = k.call(api::PRODUCE, 7, body).await;
    r.structs("Responses")
        .flat_map(|t| t.structs("PartitionResponses").map(|p| (p.i16("ErrorCode"), p.i64("BaseOffset"))).collect::<Vec<_>>())
        .collect()
}

fn stored(env: &Env, topic: &str, partition: usize) -> (usize, usize) {
    let st = env.shared.state.lock();
    let p = &st.hub("default", topic).unwrap().partitions[partition];
    (p.events.len(), p.bytes())
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

#[tokio::test]
async fn a_small_gzip_batch_that_inflates_to_16_mib_and_2_million_records_is_refused_and_stores_nothing() {
    // the review's request: 23 KB on the wire, 16.1 MB inflated, 2.3 million records of 7 bytes
    let env = env().await;
    env.shared.state.lock().create_hub("default", "bomb", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let wire = empty_records(2_300_000, true);
    assert!(wire.len() < 40_000, "the batch is small on the wire: {}", wire.len());
    let answer = produce_many(&mut k, &[("bomb", 0, wire)]).await;
    assert_eq!(answer, vec![(err::MESSAGE_TOO_LARGE, -1)]);
    assert_eq!(stored(&env, "bomb", 0).0, 0);
    // the connection still works, and a normal batch goes in
    let ok = produce_many(
        &mut k,
        &[(
            "bomb",
            0,
            encode_batch(
                &Batch { records: vec![Record { value: Some(Bytes::from_static(b"x")), ..Default::default() }], ..header() },
                true,
            ),
        )],
    )
    .await;
    assert_eq!(ok, vec![(err::NONE, 0)]);
}

#[tokio::test]
async fn a_batch_of_more_records_than_the_limit_is_refused_even_when_it_is_plain_and_small() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    // 50,000 records are allowed (350 KB), one more is not
    assert_eq!(produce_many(&mut k, &[("t", 0, empty_records(MAX_EVENTS_PER_BATCH, false))]).await, vec![(err::NONE, 0)]);
    assert_eq!(produce_many(&mut k, &[("t", 0, empty_records(MAX_EVENTS_PER_BATCH + 1, false))]).await, vec![(err::MESSAGE_TOO_LARGE, -1)]);
    assert_eq!(stored(&env, "t", 0).0, MAX_EVENTS_PER_BATCH);
}

#[tokio::test]
async fn every_stored_event_is_charged_its_overhead_so_empty_events_fill_a_partition() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    env.shared.state.lock().hub_mut("default", "t").unwrap().partitions[0].max_bytes = 1000 * EVENT_OVERHEAD;
    let mut k = K::connect(&env).await;
    for _ in 0..5 {
        assert_eq!(produce_many(&mut k, &[("t", 0, empty_records(10_000, false))]).await[0].0, err::NONE);
    }
    let (events, bytes) = stored(&env, "t", 0);
    assert!(bytes <= 1000 * EVENT_OVERHEAD, "{bytes} bytes charged");
    assert!(events <= 1000, "{events} events kept: they cost the same memory as full ones");
    // the offsets go on counting
    assert_eq!(produce_many(&mut k, &[("t", 0, empty_records(1, false))]).await, vec![(err::NONE, 50_000)]);
}

#[tokio::test]
async fn the_batches_of_one_request_share_one_budget() {
    // 20 partitions, each with 40,000 empty records (a batch within its own limit): together they would be 800,000
    // events. Each costs 7 + EVENT_OVERHEAD, so the 32 MiB budget of the request runs out after about 120,000.
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 20, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let one = empty_records(40_000, true);
    let entries: Vec<(&str, i32, Vec<u8>)> = (0..20).map(|i| ("t", i, one.clone())).collect();
    let answer = produce_many(&mut k, &entries).await;
    let ok = answer.iter().filter(|a| a.0 == err::NONE).count();
    assert!((1..=2).contains(&ok), "{answer:?}");
    assert!(answer.iter().all(|a| a.0 == err::NONE || a.0 == err::RECORD_LIST_TOO_LARGE), "{answer:?}");
    let total: usize = (0..20).map(|p| stored(&env, "t", p).0).sum();
    assert_eq!(total, ok * 40_000);
}

#[tokio::test]
async fn inflation_stops_at_the_limit_without_building_the_whole_output() {
    // 64 MiB of zeros in one gzip stream (65 KB on the wire): refused, and the test itself holds only the 64 MiB it made
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    let zeros = vec![0u8; 64 << 20];
    let wire = encode_batch_raw(&header(), 1, gzip(&zeros), false);
    // the codec bits say "none" here: the bytes are read as records and fail the CRC-less checks; use the real thing:
    let mut batch = wire.clone();
    batch[22] = 1; // attributes low byte: gzip
    let crc = azemu_kafka::crc32c::crc32c(&batch[21..]);
    batch[17..21].copy_from_slice(&crc.to_be_bytes());
    drop(zeros);
    let answer = produce_many(&mut k, &[("t", 0, batch)]).await;
    assert_eq!(answer, vec![(err::MESSAGE_TOO_LARGE, -1)]);
}

#[tokio::test]
async fn a_batch_over_the_size_limit_in_bytes_is_message_too_large_whatever_it_holds() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    // 5 records of 1 MB each: every record is within the limit, the batch (5 MB) is over four times the message size
    let rec = Record { value: Some(Bytes::from(vec![7u8; 1_000_000])), ..Default::default() };
    let b = Batch { records: vec![rec; 5], ..header() };
    assert_eq!(produce_many(&mut k, &[("t", 0, encode_batch(&b, true))]).await, vec![(err::MESSAGE_TOO_LARGE, -1)]);
    assert_eq!(stored(&env, "t", 0).0, 0);
}

// ---- What one request may name and one answer may hold --------------------------------------------------------------

impl K {
    /// Send a request and say whether the broker closed the connection instead of answering.
    async fn closed_by_broker(&mut self, key: i16, version: i16, body: Struct) -> bool {
        self.corr += 1;
        let h = RequestHeader { api_key: key, version, correlation_id: self.corr, client_id: Some("rust-test".into()) };
        self.s.write_all(&write_request(&h, &body)).await.unwrap();
        let mut b = [0u8; 1];
        matches!(tokio::time::timeout(Duration::from_secs(10), self.s.read(&mut b)).await, Ok(Ok(0)) | Ok(Err(_)))
    }
}

fn topic_list(name: &str, times: usize) -> Value {
    Value::Array((0..times).map(|_| Value::Struct(Struct::new().with("Name", name.to_string()))).collect())
}

fn metadata_body(topics: Value) -> Struct {
    Struct::new()
        .with("Topics", topics)
        .with("AllowAutoTopicCreation", false)
        .with("IncludeClusterAuthorizedOperations", false)
        .with("IncludeTopicAuthorizedOperations", false)
}

#[tokio::test]
async fn a_request_that_names_a_topic_hundreds_of_thousands_of_times_is_refused_without_building_an_answer() {
    // the review's request: 200,000 mentions of one topic, 600 KB, 22.8 MB answered
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 4, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&env).await;
    assert!(k.closed_by_broker(api::METADATA, 9, metadata_body(topic_list("t", 200_000))).await);
    // a client that names 1,000 topics is within the limit; a topic named many times is answered once
    let mut k = K::connect(&env).await;
    let r = k.call(api::METADATA, 9, metadata_body(topic_list("t", 1000))).await;
    let topics: Vec<_> = r.structs("Topics").collect();
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0].structs("Partitions").count(), 4);
    let r = k.call(api::METADATA, 9, metadata_body(topic_list("nothing-here", 1000))).await;
    assert_eq!(r.structs("Topics").count(), 1, "an unknown topic costs one small entry");
    assert!(k.closed_by_broker(api::METADATA, 9, metadata_body(topic_list("t", 1001))).await);
}

#[tokio::test]
async fn fetch_list_offsets_and_offset_fetch_refuse_requests_that_name_too_many_partitions() {
    let env = env().await;
    env.shared.state.lock().create_hub("default", "t", 1, 3_600_000, now_ms()).unwrap();
    let parts = |n: i32, key: &'static str| {
        Value::Array(
            (0..n)
                .map(|i| {
                    Value::Struct(
                        Struct::new()
                            .with(key, i)
                            .with("FetchOffset", 0i64)
                            .with("LogStartOffset", -1i64)
                            .with("PartitionMaxBytes", 1000)
                            .with("Timestamp", -1i64),
                    )
                })
                .collect(),
        )
    };
    let topic = |parts: Value| Value::Array(vec![Value::Struct(Struct::new().with("Name", "t").with("Partitions", parts))]);
    // within the limit: answered (an entry per partition, most of them unknown partitions)
    let mut k = K::connect(&env).await;
    let ok = Struct::new().with("ReplicaId", -1).with("IsolationLevel", 0).with("Topics", topic(parts(20_000, "PartitionIndex")));
    let r = k.call(api::LIST_OFFSETS, 5, ok).await;
    assert_eq!(r.structs("Topics").next().unwrap().structs("Partitions").count(), 20_000);
    // over it: closed
    let big = Struct::new().with("ReplicaId", -1).with("IsolationLevel", 0).with("Topics", topic(parts(20_001, "PartitionIndex")));
    assert!(K::connect(&env).await.closed_by_broker(api::LIST_OFFSETS, 5, big).await);
    let big = Struct::new()
        .with("MaxWaitMs", 0)
        .with("MinBytes", 0)
        .with("MaxBytes", 1000)
        .with("IsolationLevel", 0)
        .with("SessionId", 0)
        .with("SessionEpoch", -1)
        .with(
            "Topics",
            Value::Array(vec![Value::Struct(Struct::new().with("Topic", "t").with("Partitions", parts(100_000, "Partition")))]),
        );
    assert!(K::connect(&env).await.closed_by_broker(api::FETCH, 11, big).await);
    let big = Struct::new().with("GroupId", "g").with(
        "Topics",
        Value::Array(vec![Value::Struct(Struct::new().with("Name", "t").with("PartitionIndexes", (0..100_000).collect::<Vec<i32>>()))]),
    );
    assert!(K::connect(&env).await.closed_by_broker(api::OFFSET_FETCH, 6, big).await);
    // the broker is fine afterwards
    let mut k = K::connect(&env).await;
    assert_eq!(k.call(api::METADATA, 9, metadata_body(topic_list("t", 1))).await.structs("Topics").count(), 1);
}

#[tokio::test]
async fn a_metadata_answer_lists_a_bounded_number_of_partitions() {
    // 40 event hubs of 1,024 partitions: 40,960 partitions. One answer lists 20,000 of them; the topics that do not fit are retryable errors.
    let env = env().await;
    {
        let mut st = env.shared.state.lock();
        for i in 0..40 {
            st.create_hub("default", &format!("big{i:02}"), 1024, 3_600_000, now_ms()).unwrap();
        }
    }
    let mut k = K::connect(&env).await;
    let r = k.call(api::METADATA, 9, metadata_body(Value::Null)).await;
    let (mut listed, mut deferred) = (0usize, 0usize);
    for t in r.structs("Topics") {
        match t.i16("ErrorCode") {
            0 => listed += t.structs("Partitions").count(),
            err::LEADER_NOT_AVAILABLE => deferred += 1,
            other => panic!("unexpected error {other}"),
        }
    }
    assert!((19_000..=20_000).contains(&listed), "{listed} partitions listed");
    assert_eq!(listed / 1024 + deferred, 40);
    assert!(deferred >= 20);
}
