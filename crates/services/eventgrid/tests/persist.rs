//! Saving to a data directory and starting again from it (`docs/services/persistence.md`).

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_eventgrid::{EventGridService, Options};
use common::*;
use serde_json::{json, Value};

const NS: &str = "ns1";

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-eg-persist-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

async fn start_on(data: &Path, opts: Options) -> (Eg, Arc<EventGridService>) {
    let svc = Arc::new(EventGridService::with_options(opts));
    let srv = TestServer::start_persistent(vec![svc.clone()], data).await;
    (Eg { srv }, svc)
}

fn files(data: &Path) -> Vec<(String, std::time::SystemTime, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut v: Vec<_> = std::fs::read_dir(data.join("eventgrid"))
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

fn tokens(r: &azemu_core::testing::TestResponse) -> Vec<String> {
    r.json()["value"].as_array().unwrap().iter().map(|v| v["brokerProperties"]["lockToken"].as_str().unwrap().to_string()).collect()
}

fn ids(r: &azemu_core::testing::TestResponse) -> Vec<String> {
    r.json()["value"].as_array().unwrap().iter().map(|v| v["event"]["id"].as_str().unwrap().to_string()).collect()
}

/// What a client can read about everything through the admin API.
async fn look(t: &Eg) -> Value {
    let mut out = serde_json::Map::new();
    for path in [
        "/topics".to_string(),
        "/topics/orders".to_string(),
        "/topics/orders/subscriptions".to_string(),
        "/topics/orders/subscriptions/to-hook".to_string(),
        "/topics/orders/subscriptions/to-hook/deliveries".to_string(),
        "/topics/dom".to_string(),
        "/topics/dt?domain=dom".to_string(),
        "/topics/dt/subscriptions/ds?domain=dom".to_string(),
        format!("/topics/jobs?namespace={NS}"),
        format!("/topics/jobs/subscriptions?namespace={NS}"),
        format!("/topics/jobs/subscriptions/w?namespace={NS}"),
        format!("/topics/jobs/subscriptions/w/deadletters?namespace={NS}"),
        "/sources".to_string(),
    ] {
        let r = t.get_admin(&path).await;
        out.insert(path, json!({"status": r.status, "body": r.json()}));
    }
    Value::Object(out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn topics_subscriptions_queues_locks_and_dead_letters_survive_a_restart() {
    let data = dir("all");
    let (t, _svc) = start_on(&data, fast_options()).await;
    let hook = Hook::ok().await;
    // a custom topic with a push subscription and a filter, a domain with a domain topic, a namespace topic
    assert_eq!(t.put_json("/topics/orders", &json!({"inputSchema": "EventGridSchema"})).await.status, 201);
    let sub = json!({"webhookUrl": hook.url, "filter": {"includedEventTypes": ["Order.Created"], "subjectBeginsWith": "/orders"}, "retryPolicy": {"maxDeliveryAttempts": 7, "eventTimeToLiveInMinutes": 60}});
    assert_eq!(t.put_json("/topics/orders/subscriptions/to-hook", &sub).await.status, 201);
    assert_eq!(t.put_json("/topics/dom", &json!({"kind": "domain"})).await.status, 201);
    assert_eq!(t.put_json("/topics/dt?domain=dom", &json!({})).await.status, 201);
    assert_eq!(t.put_json("/topics/dt/subscriptions/ds?domain=dom", &json!({"webhookUrl": hook.url})).await.status, 201);
    assert_eq!(t.put_json(&format!("/topics/jobs?namespace={NS}"), &json!({})).await.status, 201);
    let cfg = json!({"lockDurationSeconds": 120, "maxDeliveryCount": 3, "eventTimeToLiveSeconds": 7200});
    assert_eq!(t.put_json(&format!("/topics/jobs/subscriptions/w?namespace={NS}"), &cfg).await.status, 201);
    // a published event reaches the webhook (the delivery log has it)
    assert_eq!(t.publish_eg("orders", json!([eg("o1", "Order.Created", "/orders/1")])).await.status, 200);
    eventually(5.0, || (hook.notifications().len() == 1).then_some(())).await.expect("delivered");
    // pull queue: five events; two received, one acknowledged, one rejected, one stays locked
    let events: Vec<Value> = (1..=5).map(|i| ce(&format!("e{i}"), "job.made")).collect();
    assert_eq!(t.ns_publish(NS, "jobs", json!(events)).await.status, 200);
    let r = t.receive(NS, "jobs", "w", "maxEvents=3&maxWaitTime=1").await;
    let toks = tokens(&r);
    assert_eq!(ids(&r), vec!["e1", "e2", "e3"]);
    assert_eq!(t.ns_op(NS, "jobs", "w", "acknowledge", "", json!({"lockTokens": [toks[0]]})).await.status, 200);
    assert_eq!(t.ns_op(NS, "jobs", "w", "reject", "", json!({"lockTokens": [toks[1]]})).await.status, 200);
    let locked_token = toks[2].clone();

    let before = look(&t).await;
    t.srv.shutdown().await.unwrap();

    let (t2, _svc2) = start_on(&data, fast_options()).await;
    let after = look(&t2).await;
    assert_eq!(before, after, "every topic, subscription, count and dead letter reads back as it was");
    // the lock is good until it ends: the token still acknowledges e3
    let r = t2.ns_op(NS, "jobs", "w", "acknowledge", "", json!({"lockTokens": [locked_token]})).await;
    assert_eq!(r.json()["succeededLockTokens"], json!([locked_token]), "{}", r.text());
    // the others are still there, in order, with their delivery counts
    let r = t2.receive(NS, "jobs", "w", "maxEvents=10&maxWaitTime=1").await;
    assert_eq!(ids(&r), vec!["e4", "e5"]);
    assert_eq!(r.json()["value"][0]["brokerProperties"]["deliveryCount"], 1);
    // a token that was used before the restart is not good any more
    let r = t2.ns_op(NS, "jobs", "w", "acknowledge", "", json!({"lockTokens": [toks[0]]})).await;
    assert_eq!(r.json()["succeededLockTokens"], json!([]));
    // publishing still reaches the filtered webhook after the restart
    assert_eq!(t2.publish_eg("orders", json!([eg("o2", "Order.Created", "/orders/2")])).await.status, 200);
    assert_eq!(t2.publish_eg("orders", json!([eg("o3", "Other", "/orders/3")])).await.status, 200);
    eventually(5.0, || (hook.notifications().len() == 2).then_some(())).await.expect("delivered after the restart");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(hook.notifications().len(), 2, "the filter still holds");
    t2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_that_was_waiting_for_a_retry_is_made_after_the_restart() {
    let data = dir("pending");
    // a retry waits a minute, so the event is still pending when the emulator stops
    let slow = Options { retry_base_ms: 60_000, retry_max_ms: 60_000, ..fast_options() };
    let (t, _svc) = start_on(&data, slow.clone()).await;
    let up = Arc::new(AtomicBool::new(false));
    let flag = up.clone();
    let hook = Hook::start(move |rec, _| {
        if rec.header("aeg-event-type") == Some("Notification") && !flag.load(Ordering::SeqCst) {
            return Reply::status(500);
        }
        default_reply(rec)
    })
    .await;
    assert_eq!(t.put_json("/topics/rt", &json!({})).await.status, 201);
    assert_eq!(
        t.put_json("/topics/rt/subscriptions/s", &json!({"webhookUrl": hook.url, "retryPolicy": {"maxDeliveryAttempts": 5}})).await.status,
        201
    );
    assert_eq!(t.publish_eg("rt", json!([eg("p1", "T", "s")])).await.status, 200);
    eventually(5.0, || (hook.notifications().len() == 1).then_some(())).await.expect("the first attempt");
    // let the attempt be recorded
    tokio::time::sleep(Duration::from_millis(200)).await;
    t.srv.shutdown().await.unwrap();
    up.store(true, Ordering::SeqCst);

    let (t2, _svc2) = start_on(&data, slow).await;
    let got =
        eventually(8.0, || (hook.notifications().len() >= 2).then(|| hook.notifications())).await.expect("delivered after the restart");
    assert_eq!(got.len(), 2);
    assert_eq!(got[1].header("aeg-delivery-count"), Some("1"), "the attempts made before the restart are counted");
    assert_eq!(got[1].json()[0]["id"], "p1");
    // the log has the failure from before and the success after
    let mut outcomes: Vec<String> = vec![];
    for _ in 0..100 {
        let l = t2.get_admin("/topics/rt/subscriptions/s/deliveries").await.json();
        outcomes = l["value"].as_array().unwrap().iter().map(|a| a["outcome"].as_str().unwrap().to_string()).collect();
        if outcomes.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(outcomes, vec!["Failure".to_string(), "Success".to_string()]);
    // nothing is delivered twice
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(hook.notifications().len(), 2);
    t2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_sources_and_their_subscriptions_survive() {
    let data = dir("sources");
    let (t, svc) = start_on(&data, fast_options()).await;
    let hook = Hook::ok().await;
    let source = azemu_eventgrid::SourceRef::storage_account("acct1");
    svc.put_source_subscription(
        &source,
        "s1",
        azemu_eventgrid::SubscriptionConfig { webhook_url: Some(hook.url.clone()), ..Default::default() },
        None,
    )
    .await
    .unwrap();
    t.srv.shutdown().await.unwrap();
    let (t2, svc2) = start_on(&data, fast_options()).await;
    assert!(svc2.get_source_subscription(&source, "s1").is_some());
    assert_eq!(
        svc2.raise_source_event(
            &source,
            azemu_eventgrid::SourceEvent {
                subject: "/blobServices/default/containers/c/blobs/b".into(),
                event_type: "Microsoft.Storage.BlobCreated".into(),
                data: json!({"api": "PutBlob"}),
                data_version: String::new(),
                id: None,
                time: None
            }
        ),
        1
    );
    eventually(5.0, || (hook.notifications().len() == 1).then_some(())).await.expect("delivered");
    t2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_with_nothing_changed_writes_nothing_and_a_change_writes_only_its_subscription() {
    let data = dir("quiet");
    let (t, _svc) = start_on(&data, fast_options()).await;
    for topic in ["one", "two"] {
        t.put_json(&format!("/topics/{topic}?namespace={NS}"), &json!({})).await;
        t.put_json(&format!("/topics/{topic}/subscriptions/w?namespace={NS}"), &json!({})).await;
        t.ns_publish(NS, topic, json!([ce("a", "x"), ce("b", "x")])).await;
    }
    t.srv.save_now().await;
    let a = files(&data);
    assert_eq!(a.len(), 3, "{a:?}: definitions and two subscriptions");
    // an empty receive and reads are not changes
    t.receive(NS, "one", "w", "maxWaitTime=0&maxEvents=0").await;
    t.get_admin("/topics").await;
    t.srv.save_now().await;
    t.srv.save_now().await;
    assert_eq!(files(&data), a, "nothing was written");
    // one event received: only that subscription's file (and not the definitions, which did not change) is written
    let r = t.receive(NS, "one", "w", "maxEvents=1&maxWaitTime=1").await;
    assert_eq!(ids(&r), vec!["a"]);
    t.srv.save_now().await;
    let b = files(&data);
    let changed: Vec<_> = a.iter().zip(&b).filter(|(x, y)| x != y).map(|(x, _)| x.0.clone()).collect();
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert!(changed[0].starts_with("sub-"));
    // a deleted topic loses its file
    assert_eq!(t.srv.request("DELETE", &t.admin(&format!("/topics/two?namespace={NS}"))).send().await.status, 204);
    t.srv.save_now().await;
    assert_eq!(files(&data).len(), 2);
    use std::os::unix::fs::PermissionsExt;
    for (name, _, _) in files(&data) {
        let mode = std::fs::metadata(data.join("eventgrid").join(&name)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    t.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reset_is_saved_too() {
    let data = dir("reset");
    let (t, svc) = start_on(&data, fast_options()).await;
    t.put_json("/topics/c", &json!({})).await;
    t.put_json(&format!("/topics/n?namespace={NS}"), &json!({})).await;
    t.put_json(&format!("/topics/n/subscriptions/s?namespace={NS}"), &json!({})).await;
    t.srv.save_now().await;
    let before = svc.persist_generation();
    t.srv.reset();
    assert_ne!(svc.persist_generation(), before);
    t.srv.save_now().await;
    t.srv.shutdown().await.unwrap();
    let (t2, _svc2) = start_on(&data, fast_options()).await;
    assert_eq!(t2.get_admin("/topics/c").await.status, 404);
    assert_eq!(files(&data).iter().filter(|f| f.0.starts_with("sub-")).count(), 0);
    t2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_does_not_hold_the_lock_while_it_encodes_and_writes() {
    let data = dir("lock");
    let (t, _svc) = start_on(&data, fast_options()).await;
    let t = Arc::new(t);
    t.put_json(&format!("/topics/big?namespace={NS}"), &json!({})).await;
    t.put_json(&format!("/topics/big/subscriptions/w?namespace={NS}"), &json!({"eventTimeToLiveSeconds": 86400})).await;
    t.put_json(&format!("/topics/small?namespace={NS}"), &json!({})).await;
    t.put_json(&format!("/topics/small/subscriptions/w?namespace={NS}"), &json!({})).await;
    let filler = "x".repeat(1500);
    for chunk in 0..40 {
        let events: Vec<Value> = (0..250)
            .map(|i| json!({"specversion":"1.0","id":format!("e{}", chunk * 250 + i),"source":"/t","type":"t","data":{"f":filler}}))
            .collect();
        assert_eq!(t.ns_publish(NS, "big", json!(events)).await.status, 200);
    }
    let saver = t.clone();
    let save = tokio::spawn(async move {
        let t0 = Instant::now();
        saver.srv.save_now().await;
        t0.elapsed()
    });
    let mut worst = Duration::ZERO;
    let mut n = 0;
    while !save.is_finished() {
        let t0 = Instant::now();
        let r = t.get_admin("/topics").await;
        assert_eq!(r.status, 200);
        worst = worst.max(t0.elapsed());
        n += 1;
    }
    let took = save.await.unwrap();
    assert!(n >= 3, "only {n} requests ran during a save of {took:?}");
    assert!(worst < took / 2 || worst < Duration::from_millis(80), "a request waited {worst:?} while the save took {took:?}");
    let t = Arc::try_unwrap(t).ok().unwrap();
    t.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

// ---- files that are not to be trusted ----

async fn make_saved(name: &str) -> PathBuf {
    let data = dir(name);
    let (t, _svc) = start_on(&data, fast_options()).await;
    t.put_json(&format!("/topics/jobs?namespace={NS}"), &json!({})).await;
    t.put_json(&format!("/topics/jobs/subscriptions/w?namespace={NS}"), &json!({})).await;
    t.ns_publish(NS, "jobs", json!([ce("a", "x")])).await;
    t.srv.shutdown().await.unwrap();
    data
}

fn sub_file(data: &Path) -> PathBuf {
    std::fs::read_dir(data.join("eventgrid"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("sub-"))
        .unwrap()
}

async fn start_error(data: &Path) -> String {
    let svc = Arc::new(EventGridService::with_options(fast_options()));
    match TestServer::try_start_persistent(vec![svc], data, |b| b).await {
        Ok(_) => panic!("the emulator started on a damaged directory"),
        Err(e) => format!("{e:#}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_files_stop_the_start_with_a_message_that_names_the_file() {
    let data = make_saved("trunc").await;
    let f = sub_file(&data);
    let bytes = std::fs::read(&f).unwrap();
    std::fs::write(&f, &bytes[..bytes.len() - 5]).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains(&f.file_name().unwrap().to_string_lossy().to_string()), "{e}");
    let _ = std::fs::remove_dir_all(&data);

    for which in ["eventgrid.state", "sub"] {
        let data = make_saved("flip").await;
        let target = if which == "sub" { sub_file(&data) } else { data.join("eventgrid").join(which) };
        let original = std::fs::read(&target).unwrap();
        let mut errors = 0;
        for i in 0..original.len() {
            let mut b = original.clone();
            b[i] ^= 0xff;
            std::fs::write(&target, &b).unwrap();
            let svc = Arc::new(EventGridService::with_options(fast_options()));
            match TestServer::try_start_persistent(vec![svc], &data, |b| b).await {
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
    let f = sub_file(&data);
    let mut b = std::fs::read(&f).unwrap();
    std::fs::copy(data.join("eventgrid").join("eventgrid.state"), &f).unwrap();
    let e = start_error(&data).await;
    assert!(
        e.contains("holds eventgrid state, expected eventgrid-sub") && e.contains(&f.file_name().unwrap().to_string_lossy().to_string()),
        "{e}"
    );
    let i = b.iter().position(|c| *c == b'\n').unwrap();
    let header = String::from_utf8(b[..i].to_vec()).unwrap().replace(" 1", " 99");
    b.splice(..i, header.into_bytes());
    std::fs::write(&f, &b).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains("version 99") && e.contains("newer azemu"), "{e}");
    std::fs::write(data.join("eventgrid").join("eventgrid.state"), b"hello").unwrap();
    assert!(start_error(&data).await.contains("eventgrid.state"));
    let _ = std::fs::remove_dir_all(&data);

    let data = make_saved("hostile").await;
    let defs = data.join("eventgrid").join("eventgrid.state");
    let mut b = b"AZEMU-STATE eventgrid 1\n".to_vec();
    b.extend_from_slice(&[0x81, 0xa5]);
    b.extend_from_slice(b"hosts");
    b.extend(std::iter::repeat_n(0x91, 100_000));
    b.push(0xc0);
    std::fs::write(&defs, b).unwrap();
    assert!(start_error(&data).await.contains("eventgrid.state"));
    let mut b = b"AZEMU-STATE eventgrid 1\n".to_vec();
    b.extend_from_slice(&[0x81, 0xa5]);
    b.extend_from_slice(b"hosts");
    b.extend_from_slice(&[0xdd, 0xff, 0xff, 0xff, 0xff]);
    std::fs::write(&defs, b).unwrap();
    assert!(start_error(&data).await.contains("eventgrid.state"));
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_and_extra_files_and_another_subscriptions_events() {
    let data = make_saved("missing").await;
    std::fs::remove_file(sub_file(&data)).unwrap();
    let (t, _svc) = start_on(&data, fast_options()).await;
    assert_eq!(t.get_admin(&format!("/topics/jobs/subscriptions/w?namespace={NS}")).await.status, 200);
    let r = t.receive(NS, "jobs", "w", "maxWaitTime=0").await;
    assert_eq!(r.json(), json!({"value": []}));
    t.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);

    let data = make_saved("extra").await;
    let orphan = data.join("eventgrid").join("sub-0000000000000000.state");
    std::fs::copy(sub_file(&data), &orphan).unwrap();
    std::fs::write(data.join("eventgrid").join("notes.txt"), "mine").unwrap();
    let (t, _svc) = start_on(&data, fast_options()).await;
    t.ns_publish(NS, "jobs", json!([ce("b", "x")])).await;
    t.srv.save_now().await;
    assert!(!orphan.exists());
    assert!(data.join("eventgrid").join("notes.txt").exists());
    t.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);

    let data = make_saved("swap").await;
    {
        let (t, _svc) = start_on(&data, fast_options()).await;
        t.put_json(&format!("/topics/jobs2?namespace={NS}"), &json!({})).await;
        t.put_json(&format!("/topics/jobs2/subscriptions/w?namespace={NS}"), &json!({})).await;
        t.srv.shutdown().await.unwrap();
    }
    let all: Vec<PathBuf> = std::fs::read_dir(data.join("eventgrid"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("sub-"))
        .collect();
    assert_eq!(all.len(), 2);
    let (a, b) = (std::fs::read(&all[0]).unwrap(), std::fs::read(&all[1]).unwrap());
    std::fs::write(&all[0], b).unwrap();
    std::fs::write(&all[1], a).unwrap();
    assert!(start_error(&data).await.contains("expected"));
    let _ = std::fs::remove_dir_all(&data);
}

/// Cost of a save after one small change next to about 100 MB of other state, and of a restart. Not part of the
/// normal run (it writes 100 MB): `AZEMU_PERSIST_BENCH_DIR=<dir not on /tmp> cargo test -p azemu-eventgrid --test persist -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cost_of_a_small_save_next_to_100_mb() {
    let data = PathBuf::from(std::env::var("AZEMU_PERSIST_BENCH_DIR").expect("AZEMU_PERSIST_BENCH_DIR")).join("eg-bench");
    let _ = std::fs::remove_dir_all(&data);
    let (t, _svc) = start_on(&data, fast_options()).await;
    for topic in ["big", "small"] {
        t.put_json(&format!("/topics/{topic}?namespace={NS}"), &json!({})).await;
        t.put_json(&format!("/topics/{topic}/subscriptions/w?namespace={NS}"), &json!({"eventTimeToLiveSeconds": 86400})).await;
    }
    let filler = "x".repeat(1500);
    for chunk in 0..240 {
        let events: Vec<Value> = (0..250)
            .map(|i| json!({"specversion":"1.0","id":format!("e{}", chunk * 250 + i),"source":"/t","type":"t","data":{"f":filler}}))
            .collect();
        assert_eq!(t.ns_publish(NS, "big", json!(events)).await.status, 200);
    }
    let t0 = Instant::now();
    t.srv.save_now().await;
    eprintln!("BENCH first save of 60,000 events of 1.5 KB: {:?}", t0.elapsed());
    let size: u64 = std::fs::read_dir(data.join("eventgrid")).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
    eprintln!("BENCH bytes on disk: {size}");
    t.ns_publish(NS, "small", json!([ce("one", "x")])).await;
    let t0 = Instant::now();
    t.srv.save_now().await;
    eprintln!("BENCH save after one small change: {:?}", t0.elapsed());
    t.srv.shutdown().await.unwrap();
    let t0 = Instant::now();
    let (t2, _svc2) = start_on(&data, fast_options()).await;
    eprintln!("BENCH restart (load) of 60,000 events: {:?}", t0.elapsed());
    t2.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}
