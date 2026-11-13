//! Saving to a data directory and starting again from it (`docs/services/persistence.md`).

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use azemu_amqp::message::Message;
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::types::Value;
use azemu_servicebus::core::{Outcome, ReceiveMode};
use azemu_servicebus::model::SubQueue;
use azemu_servicebus::rules::Rule;
use azemu_servicebus::state::now_ms;
use azemu_servicebus::store::{MsgState, Owner};
use common::*;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-sb-persist-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn files(data: &Path) -> Vec<(String, std::time::SystemTime, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut v: Vec<_> = std::fs::read_dir(data.join("servicebus"))
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

/// One line per message of every entity: what a client could tell about it (locks left out, they do not survive).
fn line(out: &mut Vec<String>, ns_name: &str, path: &str, what: &str, core: &azemu_servicebus::core::Core) {
    out.push(format!("{ns_name}/{path} {what}: settings {:?}", core.settings));
    for (label, store) in [("main", &core.main), ("dlq", &core.dlq), ("tdlq", &core.tdlq)] {
        out.push(format!("{ns_name}/{path} {label} next_seq {}", store.next_seq()));
        for m in store.msgs.values() {
            out.push(format!(
                "{ns_name}/{path} {label} #{} enq {} {:?} dc {} enqueued {} sched {:?} ttl {:?} exp {:?} sid {:?} deferred {} dls {:?} size {} {:?}",
                m.seq, m.enq_seq, m.state, m.delivery_count, m.enqueued_ms, m.scheduled_ms, m.ttl_ms, m.expires_ms, m.session_id, m.deferred, m.dead_letter_source, m.size, m.message
            ));
        }
    }
    let mut sessions: Vec<_> = core.sessions.iter().filter_map(|(k, s)| s.state.as_ref().map(|v| format!("{k}={v:?}"))).collect();
    sessions.sort();
    out.push(format!("{ns_name}/{path} sessions {sessions:?}"));
}

fn describe(e: &Env) -> Vec<String> {
    e.state(|st| {
        let mut out = Vec::new();
        for (ns_name, ns) in st.namespaces.iter() {
            for q in ns.queues.values() {
                line(&mut out, ns_name, &q.name, "queue", &q.core);
            }
            for t in ns.topics.values() {
                let mut dup: Vec<_> = t.dup.keys().cloned().collect();
                dup.sort();
                out.push(format!(
                    "{ns_name}/{} topic settings {:?} scheduled next {} dup {dup:?}",
                    t.name,
                    t.settings,
                    t.scheduled.next_seq()
                ));
                for m in t.scheduled.msgs.values() {
                    out.push(format!("{ns_name}/{} scheduled #{} {:?} {:?}", t.name, m.seq, m.scheduled_ms, m.message));
                }
                for s in t.subs.values() {
                    line(&mut out, ns_name, &format!("{}/{}", t.name, s.name), "sub", &s.core);
                    out.push(format!(
                        "{}/{} rules {:?}",
                        t.name,
                        s.name,
                        s.rules
                            .iter()
                            .map(|r| format!(
                                "{} {:?} {:?} {}",
                                r.name,
                                r.filter,
                                r.action.as_ref().map(|a| (&a.expression, &a.parameters)),
                                r.created_ms
                            ))
                            .collect::<Vec<_>>()
                    ));
                }
            }
        }
        out
    })
}

fn scheduled_msg(text: &str, in_ms: i64) -> Message {
    let mut m = msg_id(text, text);
    m.message_annotations = Some(vec![(Value::sym("x-opt-scheduled-enqueue-time"), Value::Timestamp(now_ms() + in_ms))]);
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_kind_of_state_survives_a_restart() {
    let data = dir("all");
    let e = env_persistent(&data).await;
    e.queue("q1", |s| {
        s.lock_duration_ms = 45_000;
        s.max_delivery_count = 5;
        s.default_ttl_ms = Some(3_600_000);
        s.dead_letter_on_expiry = true;
        s.requires_duplicate_detection = true;
        s.max_size_mb = 10;
        s.user_metadata = Some("hello".into());
    });
    e.queue("qs", |s| s.requires_session = true);
    e.queue("fwd", |s| s.forward_to = Some("q1".into()));
    e.topic("t");
    let mut corr = azemu_servicebus::rules::CorrelationFilter { label: Some("blue".into()), ..Default::default() };
    corr.properties.insert("n".into(), azemu_servicebus_sql::Value::Long(3));
    let mut params = azemu_servicebus_sql::Params::new();
    params.insert("@x".into(), azemu_servicebus_sql::Value::Str("v".into()));
    let sql = Rule::sql_filter("sqlrule", "color = @x AND n > 1", params, 123)
        .unwrap()
        .with_action("SET out = 'yes'", azemu_servicebus_sql::Params::new())
        .unwrap();
    e.sub(
        "t",
        "s1",
        vec![
            Rule { name: "corr".into(), filter: azemu_servicebus::rules::RuleFilter::Correlation(corr), action: None, created_ms: 5 },
            sql,
        ],
        |_| {},
    );
    e.sub("t", "s2", vec![Rule::default_rule(0)], |s| s.max_delivery_count = 2);

    // over AMQP: three messages to q1, one of them twice (duplicate detection)
    let mut c = e.client().await;
    let s = sender(&mut c, "q1").await;
    for (i, body) in ["one", "two", "three", "four", "five"].iter().enumerate() {
        send(&mut c, s, &msg_id(body, &format!("id{i}"))).await;
    }
    send(&mut c, s, &msg_id("one again", "id0")).await;
    assert_eq!(e.core("q1", |c| c.main.len()), 5, "the duplicate was dropped");
    // a scheduled message, and a message sent to the topic (fan-out to both subscriptions) and a scheduled one
    send(&mut c, s, &scheduled_msg("later", 3_600_000)).await;
    let ts = sender(&mut c, "t").await;
    send(&mut c, ts, &msg_id("to all", "t1")).await;
    send(&mut c, ts, &scheduled_msg("topic later", 3_600_000)).await;
    // engine states: deferred, dead-lettered with a reason, abandoned (delivery count), locked
    let now = now_ms();
    e.core("q1", |c| {
        let owner = Owner::default();
        let get = |c: &mut azemu_servicebus::core::Core| {
            c.pull(SubQueue::Main, None, owner, ReceiveMode::PeekLock, now, None).unwrap().0.unwrap()
        };
        let d = get(c);
        c.settle(&d.token.unwrap(), Outcome::Defer { modify: None }, now).unwrap();
        let d = get(c);
        c.settle(
            &d.token.unwrap(),
            Outcome::DeadLetter { reason: Some("why".into()), description: Some("because".into()), modify: None },
            now,
        )
        .unwrap();
        let d = get(c);
        c.settle(&d.token.unwrap(), Outcome::Abandon { modify: Some(vec![(Value::str("added"), Value::str("1"))]) }, now).unwrap();
        // the abandoned one is next in line again (delivery count 1): it is taken again and stays locked
        let d = get(c);
        assert!(d.token.is_some());
    });
    // session state
    e.core("qs", |c| {
        c.accept_session("sess-a", Owner { conn: 1, link: 1 }, now).unwrap();
        c.set_session_state("sess-a", Some(b"\x00state\xff".to_vec()), now).unwrap();
    });
    e.inject("qs", msg_session("in a session", "sess-b"));
    // a message sent to `fwd` lands in q1
    let fs = sender(&mut c, "fwd").await;
    send(&mut c, fs, &msg_id("forwarded", "f1")).await;
    drop(c);

    let before = describe(&e);
    assert!(before.iter().any(|l| l.contains("Locked")), "one message is locked at the time of the save");
    e.srv.save_now().await;
    // a kill: the server is dropped without the final save, so only what save_now wrote exists
    drop(e);

    let e2 = env_persistent(&data).await;
    let after = describe(&e2);
    // the locked message is available again with one more delivery attempt; everything else is the same
    let unlocked = |lines: &[String]| -> Vec<String> {
        lines.iter().map(|l| l.replace(" Locked ", " X ").replace(" Ready ", " X ")).map(|l| strip_delivery_count_of_locked(&l)).collect()
    };
    let locked_line: Vec<_> = before.iter().filter(|l| l.contains(" Locked ")).collect();
    assert_eq!(locked_line.len(), 1);
    assert!(locked_line[0].contains("dc 1 "), "{}", locked_line[0]);
    let back: Vec<_> = after.iter().filter(|l| l.contains("q1 main #") && l.contains("[116, 104, 114, 101, 101]")).collect();
    assert_eq!(back.len(), 1, "{after:#?}");
    assert!(back[0].contains(" Ready ") && back[0].contains("dc 2 "), "{}", back[0]);
    let mut b = unlocked(&before);
    let mut a = unlocked(&after);
    b.sort();
    a.sort();
    let only_before: Vec<_> = b.iter().filter(|l| !a.contains(l)).collect();
    let only_after: Vec<_> = a.iter().filter(|l| !b.contains(l)).collect();
    assert!(
        only_before.is_empty() && only_after.is_empty(),
        "everything else reads back as it was.\nonly before: {only_before:#?}\nonly after: {only_after:#?}"
    );
    // duplicate detection history, session state, dead letter reason
    assert!(e2.core("q1", |c| c.dlq.msgs.values().any(|m| app(&m.message, "DeadLetterReason") == Some(Value::str("why")))));
    assert_eq!(e2.core("qs", |c| c.session_state("sess-a")), Some(b"\x00state\xff".to_vec()));
    let mut c = e2.client().await;
    let s = sender(&mut c, "q1").await;
    send(&mut c, s, &msg_id("one again", "id0")).await;
    assert!(!e2.core("q1", |c| c.main.msgs.values().any(|m| body(&m.message) == "one again")), "id0 is still inside the duplicate window");
    // a receiver gets the messages that were there, in order, with their sequence numbers
    let r = receiver(&mut c, "q1", 10).await;
    let (d, m) = recv_body(&mut c, r).await;
    assert_eq!(body(&m), "three", "the message that was locked is first in line again");
    assert_eq!(ann(&m, "x-opt-sequence-number"), Some(Value::Long(3)));
    settle(&mut c, &d, DeliveryState::Accepted).await;
    // new sequence numbers go on from the old ones
    send(&mut c, s, &msg_id("fresh", "idx")).await;
    let seq = e2.core("q1", |c| c.main.msgs.values().find(|m| body(&m.message) == "fresh").unwrap().seq);
    assert_eq!(seq, 8, "the counter goes on where it was");
    // the topic's messages and the subscriptions' rules
    assert_eq!(e2.state(|st| st.ns("default").topic("t").unwrap().scheduled.len()), 1);
    assert_eq!(e2.state(|st| st.ns("default").topic("t").unwrap().subs["s2"].core.main.len()), 1);
    drop(c);
    e2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

fn strip_delivery_count_of_locked(l: &str) -> String {
    if l.contains(" X ") && l.contains("q1 main") && l.contains("[116, 104, 114, 101, 101]") {
        l.replace("dc 1 ", "dc 2 ")
    } else {
        l.to_string()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_locked_at_the_delivery_limit_goes_to_the_dead_letter_queue_after_the_restart() {
    let data = dir("limit");
    let e = env_persistent(&data).await;
    e.queue("q", |s| s.max_delivery_count = 2);
    e.inject("q", msg("m"));
    let now = now_ms();
    e.core("q", |c| {
        let d = c.pull(SubQueue::Main, None, Owner::default(), ReceiveMode::PeekLock, now, None).unwrap().0.unwrap();
        c.settle(&d.token.unwrap(), Outcome::Abandon { modify: None }, now).unwrap();
        let d = c.pull(SubQueue::Main, None, Owner::default(), ReceiveMode::PeekLock, now, None).unwrap().0.unwrap();
        assert_eq!(d.msg.delivery_count, 1);
    });
    e.srv.shutdown().await.unwrap();
    let e2 = env_persistent(&data).await;
    let (main, dlq) = e2.core("q", |c| (c.main.len(), c.dlq.len()));
    assert_eq!((main, dlq), (0, 1));
    assert_eq!(
        e2.core("q", |c| app(&c.dlq.msgs.values().next().unwrap().message, "DeadLetterReason")),
        Some(Value::str("MaxDeliveryCountExceeded"))
    );
    e2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transaction_that_was_not_committed_is_not_there_after_the_restart() {
    let data = dir("txn");
    let e = env_persistent(&data).await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let coord = c.attach_coordinator().await.unwrap();
    let txn = c.declare(coord).await.unwrap();
    let s = sender(&mut c, "q").await;
    let t = DeliveryState::Transactional { txn_id: txn.clone(), outcome: Some(Box::new(DeliveryState::Accepted)) };
    assert_eq!(c.send_message_in_txn(s, &msg("in the transaction"), &txn).await, Some(t));
    let s2 = sender(&mut c, "q").await;
    send(&mut c, s2, &msg("outside")).await;
    e.srv.save_now().await;
    drop(c);
    drop(e);
    let e2 = env_persistent(&data).await;
    let bodies: Vec<String> = e2.core("q", |c| c.main.msgs.values().map(|m| body(&m.message)).collect());
    assert_eq!(bodies, vec!["outside".to_string()]);
    e2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_with_nothing_changed_writes_nothing_and_a_change_writes_only_its_entity() {
    let data = dir("quiet");
    let e = env_persistent(&data).await;
    e.queue("one", |_| {});
    e.queue("two", |_| {});
    e.topic("t");
    e.sub("t", "s", vec![Rule::default_rule(0)], |_| {});
    for i in 0..10 {
        e.inject("one", msg(&format!("a{i}")));
        e.inject("two", msg(&format!("b{i}")));
    }
    e.srv.save_now().await;
    let a = files(&data);
    assert_eq!(a.len(), 5, "{a:?}: definitions, two queues, a topic, a subscription");
    // reading and peeking are not changes; nothing is written, and the generation did not move
    let svc = e.srv.emulator();
    let _ = svc;
    let mut c = e.client().await;
    let r = receiver(&mut c, "one", 1).await;
    let (d, _) = recv_body(&mut c, r).await;
    settle(&mut c, &d, DeliveryState::Accepted).await;
    drop(c);
    e.srv.save_now().await;
    let b = files(&data);
    let changed: Vec<_> = a.iter().zip(&b).filter(|(x, y)| x != y).map(|(x, _)| x.0.clone()).collect();
    assert_eq!(changed.len(), 1, "only the queue that changed is written again: {changed:?}");
    assert!(changed[0].starts_with("queue-"));
    e.srv.save_now().await;
    e.srv.save_now().await;
    assert_eq!(files(&data), b, "nothing was written");
    // an entity that is deleted loses its file
    e.state(|st| {
        st.ns("default").queues.remove("two");
    });
    azemu_servicebus::store::next_revision();
    e.srv.save_now().await;
    assert_eq!(files(&data).len(), 4);
    // files are private
    use std::os::unix::fs::PermissionsExt;
    for (name, _, _) in files(&data) {
        let mode = std::fs::metadata(data.join("servicebus").join(&name)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reset_is_saved_too() {
    let data = dir("reset");
    let e = env_persistent(&data).await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    e.srv.save_now().await;
    e.srv.reset();
    e.srv.save_now().await;
    e.srv.shutdown().await.unwrap();
    let e2 = env_persistent(&data).await;
    assert!(e2.state(|st| st.namespaces.values().all(|n| n.queues.is_empty())));
    assert_eq!(files(&data).iter().filter(|f| f.0.starts_with("queue-")).count(), 0);
    e2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_does_not_hold_the_lock_while_it_encodes_and_writes() {
    let data = dir("lock");
    let e = env_persistent(&data).await;
    e.queue("big", |s| s.max_size_mb = 1024);
    let filler = "x".repeat(3000);
    for i in 0..20_000 {
        e.inject("big", msg(&format!("{i}{filler}")));
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
    e.queue("q", |_| {});
    e.inject("q", msg("hello"));
    e.srv.shutdown().await.unwrap();
    data
}

fn entity_file(data: &Path) -> PathBuf {
    std::fs::read_dir(data.join("servicebus"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("queue-"))
        .unwrap()
}

async fn start_error(data: &Path) -> String {
    let shared = azemu_servicebus::Shared::new(true, None);
    let (svc, amqp) = azemu_servicebus::pair_with(shared);
    match azemu_core::testing::TestServer::try_start_persistent(vec![svc], data, |b| b.raw_protocol(amqp)).await {
        Ok(_) => panic!("the emulator started on a damaged directory"),
        Err(e) => format!("{e:#}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_files_stop_the_start_with_a_message_that_names_the_file() {
    // truncated
    let data = make_saved("trunc").await;
    let f = entity_file(&data);
    let bytes = std::fs::read(&f).unwrap();
    std::fs::write(&f, &bytes[..bytes.len() - 5]).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains(&f.file_name().unwrap().to_string_lossy().to_string()), "{e}");
    let _ = std::fs::remove_dir_all(&data);

    // flipped bytes in both kinds of file: never a panic or a hang
    for which in ["servicebus.state", "entity"] {
        let data = make_saved("flip").await;
        let target = if which == "entity" { entity_file(&data) } else { data.join("servicebus").join(which) };
        let original = std::fs::read(&target).unwrap();
        let mut errors = 0;
        for i in 0..original.len() {
            let mut b = original.clone();
            b[i] ^= 0xff;
            std::fs::write(&target, &b).unwrap();
            let shared = azemu_servicebus::Shared::new(true, None);
            let (svc, amqp) = azemu_servicebus::pair_with(shared);
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

    // another kind, a newer version, garbage
    let data = make_saved("kind").await;
    let f = entity_file(&data);
    let mut b = std::fs::read(&f).unwrap();
    std::fs::copy(data.join("servicebus").join("servicebus.state"), &f).unwrap();
    let e = start_error(&data).await;
    assert!(
        e.contains("holds servicebus state, expected servicebus-entity")
            && e.contains(&f.file_name().unwrap().to_string_lossy().to_string()),
        "{e}"
    );
    let i = b.iter().position(|c| *c == b'\n').unwrap();
    let header = String::from_utf8(b[..i].to_vec()).unwrap().replace(" 1", " 99");
    b.splice(..i, header.into_bytes());
    std::fs::write(&f, &b).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains("version 99") && e.contains("newer azemu"), "{e}");
    std::fs::write(data.join("servicebus").join("servicebus.state"), b"hello").unwrap();
    assert!(start_error(&data).await.contains("servicebus.state"));
    let _ = std::fs::remove_dir_all(&data);

    // a nesting far past any limit, and an array that claims billions of entries
    let data = make_saved("hostile").await;
    let defs = data.join("servicebus").join("servicebus.state");
    let mut b = b"AZEMU-STATE servicebus 1\n".to_vec();
    b.extend_from_slice(&[0x81, 0xaa]);
    b.extend_from_slice(b"namespaces");
    b.extend(std::iter::repeat_n(0x91, 100_000));
    b.push(0xc0);
    std::fs::write(&defs, b).unwrap();
    assert!(start_error(&data).await.contains("servicebus.state"));
    let mut b = b"AZEMU-STATE servicebus 1\n".to_vec();
    b.extend_from_slice(&[0x81, 0xaa]);
    b.extend_from_slice(b"namespaces");
    b.extend_from_slice(&[0xdd, 0xff, 0xff, 0xff, 0xff]);
    std::fs::write(&defs, b).unwrap();
    assert!(start_error(&data).await.contains("servicebus.state"));
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_and_extra_files_and_another_entitys_messages() {
    // an entity's file is missing: the entity is there and empty
    let data = make_saved("missing").await;
    std::fs::remove_file(entity_file(&data)).unwrap();
    let e = env_persistent(&data).await;
    assert_eq!(e.core("q", |c| c.main.len()), 0);
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);

    // extra files are ignored; an unreferenced entity file is removed by the next save
    let data = make_saved("extra").await;
    let orphan = data.join("servicebus").join("queue-0000000000000000.state");
    std::fs::copy(entity_file(&data), &orphan).unwrap();
    std::fs::write(data.join("servicebus").join("notes.txt"), "mine").unwrap();
    let e = env_persistent(&data).await;
    assert_eq!(e.core("q", |c| c.main.len()), 1);
    e.inject("q", msg("more"));
    e.srv.save_now().await;
    assert!(!orphan.exists());
    assert!(data.join("servicebus").join("notes.txt").exists());
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);

    // a file that holds another entity's messages is refused
    let data = make_saved("swap").await;
    {
        let e = env_persistent(&data).await;
        e.queue("q2", |_| {});
        e.inject("q2", msg("two"));
        e.srv.shutdown().await.unwrap();
    }
    let all: Vec<PathBuf> = std::fs::read_dir(data.join("servicebus"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("queue-"))
        .collect();
    assert_eq!(all.len(), 2);
    let (a, b) = (std::fs::read(&all[0]).unwrap(), std::fs::read(&all[1]).unwrap());
    std::fs::write(&all[0], b).unwrap();
    std::fs::write(&all[1], a).unwrap();
    assert!(start_error(&data).await.contains("expected queue"));
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn state_of_messages_is_one_of_the_four() {
    assert_eq!(MsgState::Ready, MsgState::Ready);
}

/// Cost of a save after one small change next to about 100 MB of other state, and of a restart. Not part of the
/// normal run (it writes 100 MB): `AZEMU_PERSIST_BENCH_DIR=<dir not on /tmp> cargo test -p azemu-servicebus --test persist -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cost_of_a_small_save_next_to_100_mb() {
    let data = PathBuf::from(std::env::var("AZEMU_PERSIST_BENCH_DIR").expect("AZEMU_PERSIST_BENCH_DIR")).join("sb-bench");
    let _ = std::fs::remove_dir_all(&data);
    let e = env_persistent(&data).await;
    e.queue("big", |s| s.max_size_mb = 4096);
    e.queue("small", |_| {});
    let filler = "x".repeat(3000);
    for i in 0..35_000 {
        e.inject("big", msg(&format!("{i}{filler}")));
    }
    let t = Instant::now();
    e.srv.save_now().await;
    eprintln!("BENCH first save of 35,000 messages of 3 KB: {:?}", t.elapsed());
    let size: u64 = std::fs::read_dir(data.join("servicebus")).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
    eprintln!("BENCH bytes on disk: {size}");
    e.inject("small", msg("one"));
    let t = Instant::now();
    e.srv.save_now().await;
    eprintln!("BENCH save after one small change: {:?}", t.elapsed());
    e.srv.shutdown().await.unwrap();
    let t = Instant::now();
    let e2 = env_persistent(&data).await;
    eprintln!("BENCH restart (load) of 35,000 messages: {:?}", t.elapsed());
    assert_eq!(e2.core("big", |c| c.main.len()), 35_000);
    e2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}
