//! Saving to a data directory and starting again from it (`docs/services/persistence.md`).

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use azemu_amqp::message::{Body, Message};
use azemu_amqp::types::Value;
use azemu_core::Service;
use azemu_eventhubs::state::now_ms;
use azemu_kafka::api::{self, err};
use azemu_kafka::header::{read_response, write_request};
use azemu_kafka::records::{encode_batch, Batch, Record};
use azemu_kafka::{RequestHeader, Struct};
use bytes::Bytes;
use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-eh-persist-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn files(data: &Path) -> Vec<(String, std::time::SystemTime, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut v: Vec<_> = std::fs::read_dir(data.join("eventhubs"))
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            (e.file_name().to_string_lossy().into_owned(), m.modified().unwrap(), m.ino())
        })
        .collect();
    v.sort();
    v
}

/// What a client could tell about every partition, one line per event.
fn describe(e: &Env) -> Vec<String> {
    let st = e.shared.state.lock();
    let mut out = Vec::new();
    for (ns, n) in &st.namespaces {
        for h in n.hubs.values() {
            out.push(format!(
                "{ns}/{} id {} created {} retention {} groups {:?} auto {} rr {}",
                h.name,
                h.id,
                h.created_ms,
                h.retention_ms,
                h.consumer_groups,
                h.autocreated,
                h.round_robin()
            ));
            for (i, p) in h.partitions.iter().enumerate() {
                out.push(format!(
                    "{ns}/{}/{i} next {} {} last {:?} {:?} total {} held {} max {} producers {:?}",
                    h.name,
                    p.next_seq,
                    p.next_offset,
                    p.last_offset,
                    p.last_ms,
                    p.total_events,
                    p.bytes_held,
                    p.max_bytes,
                    {
                        let mut v: Vec<_> =
                            p.producers.iter().map(|(k, s)| format!("{k}:{}:{}:{:?}", s.epoch, s.next_seq, s.recent)).collect();
                        v.sort();
                        v
                    }
                ));
                for ev in &p.events {
                    out.push(format!(
                        "{ns}/{}/{i} #{} off {} at {} key {:?} size {} {:?}",
                        h.name, ev.seq, ev.offset, ev.enqueued_ms, ev.partition_key, ev.size, ev.message
                    ));
                }
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_hubs_and_consumer_groups_survive_a_restart() {
    let data = dir("all");
    let e = env_persistent(&data).await;
    e.hub("h1", 3);
    e.shared.state.lock().hub_mut("myns", "h1").unwrap().add_consumer_group("cg1");
    e.shared.state.lock().create_hub("myns", "short", 1, 3_600_000, now_ms()).unwrap();
    let mut c = e.client().await;
    let s0 = attach_sender(&mut c, "h1/Partitions/0").await.unwrap();
    let s2 = attach_sender(&mut c, "h1/Partitions/2").await.unwrap();
    let mut rich = keyed("with everything", "pk1");
    rich.application_properties = Some(vec![
        (Value::str("color"), Value::str("blue")),
        (Value::str("n"), Value::Long(7)),
        (Value::str("bin"), Value::Binary(vec![0, 1, 255])),
    ]);
    rich.properties = Some(azemu_amqp::message::Properties {
        message_id: Some(Value::str("m-1")),
        content_type: Some("text/plain".into()),
        subject: Some("subj".into()),
        ..Default::default()
    });
    send_one(&mut c, s0, &rich).await;
    send_one(&mut c, s0, &event("second")).await;
    let value_body = Message { body: Body::Value(Value::Map(vec![(Value::str("k"), Value::Long(1))])), ..Default::default() };
    send_one(&mut c, s2, &value_body).await;
    send_batch(&mut c, s2, &[event("b1"), event("b2"), event("b3")]).await;
    // an old event in a hub with short retention: its time is absolute, so it is gone after the restart too
    {
        let mut st = e.shared.state.lock();
        let p = &mut st.hub_mut("myns", "short").unwrap().partitions[0];
        p.append(event("old"), now_ms() - 7_200_000);
        p.append(event("new"), now_ms());
    }
    // send to the hub itself: round robin moves
    let any = attach_sender(&mut c, "h1").await.unwrap();
    send_one(&mut c, any, &event("anywhere")).await;
    drop(c);
    let before = describe(&e);
    e.srv.save_now().await;
    drop(e); // as a kill: only the save above exists

    let e2 = env_persistent(&data).await;
    let after = describe(&e2);
    assert_eq!(before, after, "every hub, partition and event reads back as it was");
    // a receiver reads the events, in order, with the annotations the broker sets
    let mut c = e2.client().await;
    let r = consume(&mut c, "h1", "cg1", 0, &from_start()).await.unwrap();
    let first = c.recv(r).await;
    let m = first.message();
    assert_eq!(body_of(&first), "with everything");
    assert_eq!(ann_i64(&m, "x-opt-sequence-number"), 0);
    assert_eq!(app(&m, "color"), Some(Value::str("blue")));
    assert_eq!(app(&m, "bin"), Some(Value::Binary(vec![0, 1, 255])));
    assert_eq!(m.properties.as_ref().unwrap().subject.as_deref(), Some("subj"));
    let second = c.recv(r).await;
    assert_eq!(body_of(&second), "second");
    let r2 = consume(&mut c, "h1", "$Default", 2, &from_start()).await.unwrap();
    let v = c.recv(r2).await;
    assert!(matches!(v.message().body, Body::Value(Value::Map(_))));
    // new events go on from the old sequence numbers and offsets
    let s0 = attach_sender(&mut c, "h1/Partitions/0").await.unwrap();
    send_one(&mut c, s0, &event("third")).await;
    let third = c.recv(r).await;
    assert_eq!(ann_i64(&third.message(), "x-opt-sequence-number"), 2);
    // retention applied by absolute time: the old event goes at the first read
    let r3 = consume(&mut c, "short", "$Default", 0, &from_start()).await.unwrap();
    assert_eq!(body_of(&c.recv(r3).await), "new");
    drop(c);
    e2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

fn app(m: &Message, k: &str) -> Option<Value> {
    m.app_property(k).cloned()
}

// ---- Kafka ----

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

fn batch(producer: i64, epoch: i16, base_seq: i32, values: &[&str]) -> Batch {
    Batch {
        base_offset: 0,
        partition_leader_epoch: -1,
        attributes: 0,
        last_offset_delta: values.len() as i32 - 1,
        first_timestamp: now_ms(),
        max_timestamp: now_ms(),
        producer_id: producer,
        producer_epoch: epoch,
        base_sequence: base_seq,
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

async fn produce(k: &mut K, topic: &str, partition: i32, b: &Batch) -> (i16, i64) {
    let body = Struct::new().with("TransactionalId", azemu_kafka::Value::Null).with("Acks", -1).with("TimeoutMs", 5000).with(
        "TopicData",
        vec![Struct::new()
            .with("Name", topic.to_string())
            .with("PartitionData", vec![Struct::new().with("Index", partition).with("Records", encode_batch(b, false))])],
    );
    let r = k.call(api::PRODUCE, 7, body).await;
    let p = r.structs("Responses").next().unwrap().structs("PartitionResponses").next().unwrap().clone();
    (p.i16("ErrorCode"), p.i64("BaseOffset"))
}

async fn high_watermark(k: &mut K, topic: &str, partition: i32) -> (i16, i64) {
    let body = Struct::new()
        .with("MaxWaitMs", 0)
        .with("MinBytes", 1)
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
                    .with("FetchOffset", 0i64)
                    .with("LogStartOffset", -1i64)
                    .with("PartitionMaxBytes", 1_000_000)],
            )],
        );
    let r = k.call(api::FETCH, 11, body).await;
    let p = r.structs("Responses").next().unwrap().structs("Partitions").next().unwrap().clone();
    (p.i16("ErrorCode"), p.i64("HighWatermark"))
}

fn commit_body(group: &str, generation: i32, member: &str, topic: &str, part: i32, off: i64) -> Struct {
    Struct::new()
        .with("GroupId", group.to_string())
        .with("GenerationIdOrMemberEpoch", generation)
        .with("MemberId", member.to_string())
        .with("GroupInstanceId", azemu_kafka::Value::Null)
        .with(
            "Topics",
            vec![Struct::new().with("Name", topic.to_string()).with(
                "Partitions",
                vec![Struct::new()
                    .with("PartitionIndex", part)
                    .with("CommittedOffset", off)
                    .with("CommittedLeaderEpoch", -1)
                    .with("CommittedMetadata", Some("meta".to_string()))],
            )],
        )
}

fn code_of(r: &Struct) -> i16 {
    r.structs("Topics").next().unwrap().structs("Partitions").next().unwrap().i16("ErrorCode")
}

async fn init_producer(k: &mut K) -> i64 {
    k.call(api::INIT_PRODUCER_ID, 3, Struct::new().with("TransactionalId", azemu_kafka::Value::Null).with("TransactionTimeoutMs", 60000))
        .await
        .i64("ProducerId")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kafka_offsets_producers_and_groups_after_a_restart() {
    let data = dir("kafka");
    let e = env_persistent(&data).await;
    e.shared.state.lock().create_hub("default", "t", 2, 3_600_000, now_ms()).unwrap();
    let mut k = K::connect(&e).await;
    let pid = init_producer(&mut k).await;
    assert_eq!(produce(&mut k, "t", 0, &batch(pid, 0, 0, &["a", "b"])).await, (0, 0));
    assert_eq!(produce(&mut k, "t", 0, &batch(pid, 0, 2, &["c"])).await, (0, 2));
    // committed offsets: from a consumer that assigns by itself, and from a member of a group
    assert_eq!(code_of(&k.call(api::OFFSET_COMMIT, 7, commit_body("standalone", -1, "", "t", 0, 2)).await), 0);
    let join_with = |member: &str| {
        Struct::new()
            .with("GroupId", "members")
            .with("SessionTimeoutMs", 30000)
            .with("RebalanceTimeoutMs", 30000)
            .with("MemberId", member.to_string())
            .with("GroupInstanceId", azemu_kafka::Value::Null)
            .with("ProtocolType", "consumer")
            .with("Protocols", vec![Struct::new().with("Name", "range").with("Metadata", Bytes::from_static(b"\0\0\0\0\0\0\0\0"))])
    };
    let first = k.call(api::JOIN_GROUP, 5, join_with("")).await;
    assert_eq!(first.i16("ErrorCode"), 79, "the broker asks the client to come back with the member id it made");
    let join = k.call(api::JOIN_GROUP, 5, join_with(first.str("MemberId").unwrap())).await;
    let member = join.str("MemberId").unwrap().to_string();
    assert_eq!(join.i16("ErrorCode"), 0, "{join:?}");
    let generation = join.i32("GenerationId");
    k.call(
        api::SYNC_GROUP,
        3,
        Struct::new()
            .with("GroupId", "members")
            .with("GenerationId", generation)
            .with("MemberId", member.clone())
            .with("GroupInstanceId", azemu_kafka::Value::Null)
            .with(
                "Assignments",
                vec![Struct::new().with("MemberId", member.clone()).with("Assignment", Bytes::from_static(b"\0\0\0\0\0\0\0\0"))],
            ),
    )
    .await;
    assert_eq!(code_of(&k.call(api::OFFSET_COMMIT, 7, commit_body("members", generation, &member, "t", 1, 0)).await), 0);
    assert_eq!(code_of(&k.call(api::OFFSET_COMMIT, 7, commit_body("members", generation, &member, "t", 0, 3)).await), 0);
    drop(k);
    e.srv.shutdown().await.unwrap();

    let e2 = env_persistent(&data).await;
    let mut k = K::connect(&e2).await;
    // the events and the high watermark
    assert_eq!(high_watermark(&mut k, "t", 0).await, (0, 3));
    // committed offsets of both groups, with their metadata
    for (group, topic_part, want) in [("standalone", 0, 2), ("members", 0, 3)] {
        let r = k
            .call(
                api::OFFSET_FETCH,
                6,
                Struct::new().with("GroupId", group.to_string()).with(
                    "Topics",
                    azemu_kafka::Value::Array(vec![azemu_kafka::Value::Struct(
                        Struct::new().with("Name", "t").with("PartitionIndexes", vec![topic_part]),
                    )]),
                ),
            )
            .await;
        let p = r.structs("Topics").next().unwrap().structs("Partitions").next().unwrap().clone();
        assert_eq!((p.i64("CommittedOffset"), p.str("Metadata")), (want, Some("meta")), "{group}");
    }
    // the group has no members: a commit with the old member id is refused, as after any restart of a broker
    assert_eq!(
        code_of(&k.call(api::OFFSET_COMMIT, 7, commit_body("members", generation, &member, "t", 0, 3)).await),
        err::UNKNOWN_MEMBER_ID
    );
    let listed = e2.shared.kafka.list("default");
    assert!(listed.iter().all(|g| g.members.is_empty()), "{listed:?}");
    // the idempotent producer that comes back: a retry of its last batch gets the original offset, the next batch goes on,
    // an older epoch is fenced
    assert_eq!(produce(&mut k, "t", 0, &batch(pid, 0, 2, &["c"])).await, (0, 2));
    assert_eq!(produce(&mut k, "t", 0, &batch(pid, 0, 3, &["d"])).await, (0, 3));
    assert_eq!(produce(&mut k, "t", 0, &batch(pid, 0, 9, &["gap"])).await.0, err::OUT_OF_ORDER_SEQUENCE_NUMBER);
    // a producer id is never handed out twice
    let fresh = init_producer(&mut k).await;
    assert!(fresh > pid, "{fresh} <= {pid}");
    assert_eq!(produce(&mut k, "t", 0, &batch(fresh, 0, 0, &["e"])).await, (0, 4));
    drop(k);
    e2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_with_nothing_changed_writes_nothing_and_a_change_writes_only_its_partition() {
    let data = dir("quiet");
    let e = env_persistent(&data).await;
    e.hub("h", 3);
    {
        let mut st = e.shared.state.lock();
        for p in 0..3 {
            st.hub_mut("myns", "h").unwrap().partitions[p].append(event("x"), now_ms());
        }
    }
    e.srv.save_now().await;
    let a = files(&data);
    assert_eq!(a.len(), 4, "{a:?}: definitions and three partitions");
    // reads are not changes
    let mut c = e.client().await;
    let r = consume(&mut c, "h", "$Default", 1, &from_start()).await.unwrap();
    assert_eq!(body_of(&c.recv(r).await), "x");
    drop(c);
    e.srv.save_now().await;
    e.srv.save_now().await;
    assert_eq!(files(&data), a, "nothing was written");
    let mut c = e.client().await;
    let s = attach_sender(&mut c, "h/Partitions/1").await.unwrap();
    send_one(&mut c, s, &event("y")).await;
    drop(c);
    e.srv.save_now().await;
    let b = files(&data);
    let changed: Vec<_> = a.iter().zip(&b).filter(|(x, y)| x != y).map(|(x, _)| x.0.clone()).collect();
    assert_eq!(changed.len(), 1, "only the partition that changed is written again: {changed:?}");
    // a deleted hub loses its files
    e.shared.delete_hub("myns", "h");
    e.srv.save_now().await;
    assert_eq!(files(&data).len(), 1);
    use std::os::unix::fs::PermissionsExt;
    for (name, _, _) in files(&data) {
        let mode = std::fs::metadata(data.join("eventhubs").join(&name)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reset_is_saved_too() {
    let data = dir("reset");
    let e = env_persistent(&data).await;
    e.hub("h", 1);
    e.srv.save_now().await;
    e.srv.reset();
    e.srv.save_now().await;
    e.srv.shutdown().await.unwrap();
    let e2 = env_persistent(&data).await;
    assert!(e2.shared.state.lock().hub("myns", "h").is_none());
    assert_eq!(files(&data).iter().filter(|f| f.0.starts_with("part-")).count(), 0);
    e2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_does_not_hold_the_lock_while_it_encodes_and_writes() {
    let data = dir("lock");
    let e = env_persistent(&data).await;
    e.hub("big", 1);
    {
        let mut st = e.shared.state.lock();
        let p = &mut st.hub_mut("myns", "big").unwrap().partitions[0];
        p.max_bytes = 0;
        let filler = "x".repeat(3000);
        for i in 0..20_000 {
            p.append(event(&format!("{i}{filler}")), now_ms());
        }
    }
    let shared = e.shared.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut worst = Duration::ZERO;
            let mut n = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let t = Instant::now();
                drop(shared.state.lock());
                worst = worst.max(t.elapsed());
                n += 1;
                std::thread::sleep(Duration::from_micros(200));
            }
            (worst, n)
        })
    };
    let t = Instant::now();
    e.srv.save_now().await;
    let took = t.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (worst, n) = probe.join().unwrap();
    assert!(n > 20, "the probe ran only {n} times");
    assert!(worst < took / 3, "the lock was held up to {worst:?} during a save that took {took:?}");
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

// ---- files that are not to be trusted ----

async fn make_saved(name: &str) -> PathBuf {
    let data = dir(name);
    let e = env_persistent(&data).await;
    e.hub("h", 1);
    e.shared.state.lock().hub_mut("myns", "h").unwrap().partitions[0].append(event("hello"), now_ms());
    e.srv.shutdown().await.unwrap();
    data
}

fn part_file(data: &Path) -> PathBuf {
    std::fs::read_dir(data.join("eventhubs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("part-"))
        .unwrap()
}

async fn start_error(data: &Path) -> String {
    let shared = azemu_eventhubs::state::Shared::new(true, None);
    let (svc, amqp) = azemu_eventhubs::pair_with(shared);
    match azemu_core::testing::TestServer::try_start_persistent(vec![svc], data, |b| b.raw_protocol(amqp)).await {
        Ok(_) => panic!("the emulator started on a damaged directory"),
        Err(e) => format!("{e:#}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_files_stop_the_start_with_a_message_that_names_the_file() {
    let data = make_saved("trunc").await;
    let f = part_file(&data);
    let bytes = std::fs::read(&f).unwrap();
    std::fs::write(&f, &bytes[..bytes.len() - 5]).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains(&f.file_name().unwrap().to_string_lossy().to_string()), "{e}");
    let _ = std::fs::remove_dir_all(&data);

    for which in ["eventhubs.state", "part"] {
        let data = make_saved("flip").await;
        let target = if which == "part" { part_file(&data) } else { data.join("eventhubs").join(which) };
        let original = std::fs::read(&target).unwrap();
        let mut errors = 0;
        for i in 0..original.len() {
            let mut b = original.clone();
            b[i] ^= 0xff;
            std::fs::write(&target, &b).unwrap();
            let shared = azemu_eventhubs::state::Shared::new(true, None);
            let (svc, amqp) = azemu_eventhubs::pair_with(shared);
            match azemu_core::testing::TestServer::try_start_persistent(vec![svc], &data, |b| b.raw_protocol(amqp)).await {
                Ok(s) => drop(s),
                Err(e) => {
                    errors += 1;
                    assert!(format!("{e:#}").contains(".state"), "{e:#}");
                }
            }
            std::fs::write(&target, &original).unwrap();
        }
        assert!(errors > 5, "{which}: {errors} of {} flipped bytes were noticed", original.len());
        let _ = std::fs::remove_dir_all(&data);
    }

    let data = make_saved("kind").await;
    let f = part_file(&data);
    let mut b = std::fs::read(&f).unwrap();
    std::fs::copy(data.join("eventhubs").join("eventhubs.state"), &f).unwrap();
    let e = start_error(&data).await;
    assert!(
        e.contains("holds eventhubs state, expected eventhubs-partition")
            && e.contains(&f.file_name().unwrap().to_string_lossy().to_string()),
        "{e}"
    );
    let i = b.iter().position(|c| *c == b'\n').unwrap();
    let header = String::from_utf8(b[..i].to_vec()).unwrap().replace(" 1", " 99");
    b.splice(..i, header.into_bytes());
    std::fs::write(&f, &b).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains("version 99") && e.contains("newer azemu"), "{e}");
    std::fs::write(data.join("eventhubs").join("eventhubs.state"), b"hello").unwrap();
    assert!(start_error(&data).await.contains("eventhubs.state"));
    let _ = std::fs::remove_dir_all(&data);

    let data = make_saved("hostile").await;
    let defs = data.join("eventhubs").join("eventhubs.state");
    let mut b = b"AZEMU-STATE eventhubs 1\n".to_vec();
    b.extend_from_slice(&[0x81, 0xaa]);
    b.extend_from_slice(b"namespaces");
    b.extend(std::iter::repeat_n(0x91, 100_000));
    b.push(0xc0);
    std::fs::write(&defs, b).unwrap();
    assert!(start_error(&data).await.contains("eventhubs.state"));
    let mut b = b"AZEMU-STATE eventhubs 1\n".to_vec();
    b.extend_from_slice(&[0x81, 0xaa]);
    b.extend_from_slice(b"namespaces");
    b.extend_from_slice(&[0xdd, 0xff, 0xff, 0xff, 0xff]);
    std::fs::write(&defs, b).unwrap();
    assert!(start_error(&data).await.contains("eventhubs.state"));
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_and_extra_files_and_another_partitions_events() {
    let data = make_saved("missing").await;
    std::fs::remove_file(part_file(&data)).unwrap();
    let e = env_persistent(&data).await;
    assert!(e.shared.state.lock().hub("myns", "h").unwrap().partitions[0].is_empty());
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);

    let data = make_saved("extra").await;
    let orphan = data.join("eventhubs").join("part-0000000000000000.state");
    std::fs::copy(part_file(&data), &orphan).unwrap();
    std::fs::write(data.join("eventhubs").join("notes.txt"), "mine").unwrap();
    let e = env_persistent(&data).await;
    assert_eq!(e.shared.state.lock().hub("myns", "h").unwrap().partitions[0].events.len(), 1);
    e.shared.state.lock().hub_mut("myns", "h").unwrap().partitions[0].append(event("more"), now_ms());
    e.srv.save_now().await;
    assert!(!orphan.exists());
    assert!(data.join("eventhubs").join("notes.txt").exists());
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);

    let data = make_saved("swap").await;
    {
        let e = env_persistent(&data).await;
        e.hub("h2", 1);
        e.shared.state.lock().hub_mut("myns", "h2").unwrap().partitions[0].append(event("two"), now_ms());
        e.srv.shutdown().await.unwrap();
    }
    let all: Vec<PathBuf> = std::fs::read_dir(data.join("eventhubs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("part-"))
        .collect();
    assert_eq!(all.len(), 2);
    let (a, b) = (std::fs::read(&all[0]).unwrap(), std::fs::read(&all[1]).unwrap());
    std::fs::write(&all[0], b).unwrap();
    std::fs::write(&all[1], a).unwrap();
    assert!(start_error(&data).await.contains("expected partition 0 of"));
    let _ = std::fs::remove_dir_all(&data);
}

/// Cost of a save after one small change next to about 100 MB of other state, and of a restart. Not part of the
/// normal run (it writes 100 MB): `AZEMU_PERSIST_BENCH_DIR=<dir not on /tmp> cargo test -p azemu-eventhubs --test persist -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cost_of_a_small_save_next_to_100_mb() {
    let data = PathBuf::from(std::env::var("AZEMU_PERSIST_BENCH_DIR").expect("AZEMU_PERSIST_BENCH_DIR")).join("eh-bench");
    let _ = std::fs::remove_dir_all(&data);
    let e = env_persistent(&data).await;
    e.hub("big", 2);
    e.hub("small", 1);
    {
        let mut st = e.shared.state.lock();
        let filler = "x".repeat(3000);
        for p in 0..2 {
            let part = &mut st.hub_mut("myns", "big").unwrap().partitions[p];
            part.max_bytes = 0;
            for i in 0..17_500 {
                part.append(event(&format!("{i}{filler}")), now_ms());
            }
        }
    }
    let t = Instant::now();
    e.srv.save_now().await;
    eprintln!("BENCH first save of 35,000 events of 3 KB: {:?}", t.elapsed());
    let size: u64 = std::fs::read_dir(data.join("eventhubs")).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
    eprintln!("BENCH bytes on disk: {size}");
    e.shared.state.lock().hub_mut("myns", "small").unwrap().partitions[0].append(event("one"), now_ms());
    let t = Instant::now();
    e.srv.save_now().await;
    eprintln!("BENCH save after one small change: {:?}", t.elapsed());
    e.srv.shutdown().await.unwrap();
    let t = Instant::now();
    let e2 = env_persistent(&data).await;
    eprintln!("BENCH restart (load) of 35,000 events: {:?}", t.elapsed());
    assert_eq!(e2.shared.state.lock().hub("myns", "big").unwrap().partitions[1].events.len(), 17_500);
    e2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn the_kafka_service_keeps_nothing_of_its_own() {
    let shared = azemu_eventhubs::state::Shared::new(true, None);
    let kafka = azemu_eventhubs::kafka::endpoint_with(shared, Default::default());
    assert_eq!(kafka.persist_generation(), None);
}
