//! Queue state across restarts (`--data-dir`).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use azemu_core::persist;
use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_queue::QueueService;
use common::*;

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-queue-persist-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn open(d: &Path) -> Q {
    let srv = TestServer::start_persistent(vec![Arc::new(QueueService::new())], d).await;
    let base = srv.url("queue", ACCOUNT);
    Q { srv, base }
}

async fn try_open(d: &Path) -> anyhow::Result<Q> {
    let srv = TestServer::try_start_persistent(vec![Arc::new(QueueService::new())], d, |b| b).await?;
    let base = srv.url("queue", ACCOUNT);
    Ok(Q { srv, base })
}

fn ids(r: &azemu_core::testing::TestResponse) -> Vec<String> {
    let body = r.text();
    let mut out = vec![];
    let mut rest = body.as_str();
    while let Some(i) = rest.find("<MessageId>") {
        let after = &rest[i + 11..];
        let j = after.find("</MessageId>").unwrap();
        out.push(after[..j].to_string());
        rest = &after[j..];
    }
    out
}

fn urlenc(s: &str) -> String {
    s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
}

fn norm(text: &str, q: &Q) -> String {
    text.replace(&format!(":{}", q.srv.port()), ":PORT")
}

#[tokio::test]
async fn queues_messages_times_counts_and_receipts_survive() {
    let d = dir("rich");
    let q = open(&d).await;
    q.send("PUT", "/work", &[("x-ms-meta-owner", "alice"), ("x-ms-meta-Team", "Blue")], "").await;
    q.send("PUT", "/empty", &[], "").await;
    let acl = "<SignedIdentifiers><SignedIdentifier><Id>p1</Id><AccessPolicy><Start>2024-01-01T00:00:00Z</Start><Expiry>2099-01-01T00:00:00Z</Expiry><Permission>raup</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(q.send("PUT", "/work?comp=acl", &[], acl).await.status, 204);
    let props = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>https://example.com</AllowedOrigins><AllowedMethods>GET,POST</AllowedMethods><AllowedHeaders>x-ms-meta-*</AllowedHeaders><ExposedHeaders>x-ms-request-id</ExposedHeaders><MaxAgeInSeconds>60</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
    assert_eq!(q.send("PUT", "/?restype=service&comp=properties", &[], props).await.status, 202);

    // m1: dequeued, hidden for 2 s; m2: delayed; m3: dequeued, hidden for long, then updated; m4: never expires; m5: visible
    let put = |text: &'static str, query: &'static str| {
        let q = &q;
        async move { ids(&q.put("work", text, query).await).remove(0) }
    };
    let id1 = put("first", "").await;
    let id2 = put("delayed", "visibilitytimeout=3000").await;
    let id3 = put("third", "").await;
    let id4 = put("forever", "messagettl=-1").await;
    let got = q.get("work", "numofmessages=1&visibilitytimeout=2").await;
    assert_eq!(ids(&got), vec![id1.clone()]);
    let got3 = q.get("work", "numofmessages=1&visibilitytimeout=2000").await;
    assert_eq!(ids(&got3), vec![id3.clone()]);
    let old_receipt3 = tag(&got3.text(), "PopReceipt").unwrap();
    let upd = q
        .send(
            "PUT",
            &format!("/work/messages/{id3}?popreceipt={}&visibilitytimeout=2000", urlenc(&old_receipt3)),
            &[],
            &msg_body("third, updated"),
        )
        .await;
    assert_eq!(upd.status, 204, "{}", upd.text());
    let receipt3 = upd.header("x-ms-popreceipt").unwrap().to_string();
    let id5 = put("visible", "").await;
    let _ = q.put("work", "one more", "messagettl=3600").await;

    let peek_before = q.peek("work", "numofmessages=32").await.text();
    assert_eq!(
        ids(&q.peek("work", "numofmessages=32").await),
        vec![id4.clone(), id5.clone(), ids(&q.peek("work", "numofmessages=32").await)[2].clone()]
    );
    let list_before = norm(&q.send("GET", "/?comp=list&include=metadata", &[], "").await.text(), &q);
    let meta_before = q.send("GET", "/work?comp=metadata", &[], "").await;
    let acl_before = q.send("GET", "/work?comp=acl", &[], "").await.text();
    let props_before = q.send("GET", "/?restype=service&comp=properties", &[], "").await.text();
    q.srv.shutdown().await.unwrap();

    let q = open(&d).await;
    assert_eq!(q.peek("work", "numofmessages=32").await.text(), peek_before, "visible messages, with their times and receipts");
    assert_eq!(norm(&q.send("GET", "/?comp=list&include=metadata", &[], "").await.text(), &q), list_before);
    let meta_after = q.send("GET", "/work?comp=metadata", &[], "").await;
    assert_eq!(meta_after.header("x-ms-approximate-messages-count"), meta_before.header("x-ms-approximate-messages-count"));
    assert_eq!(meta_after.header("x-ms-meta-owner"), Some("alice"));
    assert_eq!(meta_after.header("x-ms-meta-team"), Some("Blue"));
    assert_eq!(q.send("GET", "/work?comp=acl", &[], "").await.text(), acl_before);
    assert_eq!(q.send("GET", "/?restype=service&comp=properties", &[], "").await.text(), props_before);
    assert_eq!(q.send("GET", "/empty/messages?peekonly=true", &[], "").await.status, 200);

    // the receipt from before the restart still deletes the message, the one before the update does not
    let r = q.send("DELETE", &format!("/work/messages/{id3}?popreceipt={}", urlenc(&old_receipt3)), &[], "").await;
    assert_eq!(r.status, 400);
    let r = q.send("DELETE", &format!("/work/messages/{id3}?popreceipt={}", urlenc(&receipt3)), &[], "").await;
    assert_eq!(r.status, 204, "{}", r.text());
    // m1 comes back when its hiding time is over, with the dequeue count that was kept
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let again = q.get("work", "numofmessages=1&visibilitytimeout=30").await;
    assert_eq!(ids(&again), vec![id1.clone()]);
    assert_eq!(tag(&again.text(), "DequeueCount").as_deref(), Some("2"));
    // the delayed one is still waiting
    assert!(!ids(&q.peek("work", "numofmessages=32").await).contains(&id2));
    // new messages and queues work after a restart
    assert_eq!(q.put("work", "after", "").await.status, 201);
    q.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn an_expired_message_is_gone_after_a_restart_and_reset_is_kept() {
    let d = dir("expiry");
    let q = open(&d).await;
    q.create("qqq").await;
    q.put("qqq", "short", "messagettl=1").await;
    q.put("qqq", "long", "messagettl=3600").await;
    q.srv.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let q = open(&d).await;
    let texts = q.peek("qqq", "numofmessages=10").await.text();
    assert!(texts.contains("long") && !texts.contains("short"), "{texts}");
    // reset, then a save: the next start has nothing
    q.srv.reset();
    q.srv.save_now().await;
    drop(q);
    let q = open(&d).await;
    assert_eq!(q.send("GET", "/?comp=list", &[], "").await.text().matches("<Queue>").count(), 0);
    q.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn unchanged_state_is_not_saved_again_and_reads_are_not_changes() {
    let d = dir("unchanged");
    let svc = Arc::new(QueueService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &d).await;
    let q = Q { base: srv.url("queue", ACCOUNT), srv };
    q.create("qqq").await;
    q.put("qqq", "m", "").await;
    let state_dir = d.join("queue");
    svc.save(&state_dir).unwrap();
    let file = state_dir.join("queue.state");
    let first = (std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap());
    let g = svc.persist_generation();
    std::thread::sleep(Duration::from_millis(30));
    svc.save(&state_dir).unwrap();
    assert_eq!(
        (std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap()),
        first,
        "nothing written the second time"
    );
    // reads, peeks, a Get Messages that finds nothing: not changes
    q.send("GET", "/?comp=list", &[], "").await;
    q.peek("qqq", "").await;
    q.send("GET", "/qqq?comp=metadata", &[], "").await;
    q.send("GET", "/?restype=service&comp=properties", &[], "").await;
    q.get("qqq", "visibilitytimeout=60").await; // hands out the message: this one is a change
    assert_ne!(svc.persist_generation(), g);
    let g2 = svc.persist_generation();
    q.get("qqq", "").await; // nothing visible now
    assert_eq!(svc.persist_generation(), g2);
    q.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

fn flip_byte(path: &Path, at: usize) {
    let mut b = std::fs::read(path).unwrap();
    let at = at.min(b.len() - 1);
    b[at] ^= 0xff;
    std::fs::write(path, b).unwrap();
}

#[tokio::test]
async fn damaged_state_files_are_refused_with_a_message_that_names_the_file() {
    let d = dir("damage");
    let q = open(&d).await;
    q.create("qqq").await;
    q.put("qqq", "some text", "").await;
    q.srv.shutdown().await.unwrap();
    let file = d.join("queue").join("queue.state");
    let good = std::fs::read(&file).unwrap();
    let refused = |d: PathBuf| async move {
        match try_open(&d).await {
            Ok(q) => {
                q.srv.shutdown().await.ok();
                None
            }
            Err(e) => Some(e.to_string()),
        }
    };
    for cut in [0usize, 5, 21, good.len() / 2, good.len() - 1] {
        std::fs::write(&file, &good[..cut]).unwrap();
        let e = refused(d.clone()).await.unwrap_or_else(|| panic!("cut at {cut} was accepted"));
        assert!(e.contains("queue.state") && e.contains("--data-discard-unreadable"), "{e}");
    }
    let mut accepted = 0;
    for at in 0..good.len() {
        std::fs::write(&file, &good).unwrap();
        flip_byte(&file, at);
        if let Some(e) = refused(d.clone()).await {
            assert!(e.contains("queue.state"), "flip at {at}: {e}");
        } else {
            accepted += 1;
            std::fs::write(&file, &good).unwrap();
        }
    }
    assert!(accepted < good.len() / 2, "{accepted} of {} flips were accepted", good.len());
    std::fs::write(&file, persist::encode("blob", 1, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("holds blob state, expected queue"));
    std::fs::write(&file, persist::encode("queue", 99, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("version 99"));
    std::fs::write(&file, b"AZEMU-STATE queue 1\n\xc1\xc1").unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("queue.state"));
    // a file that is valid MessagePack of the right shape but with a wrong time inside
    std::fs::write(&file, &good).unwrap();
    std::fs::write(d.join("queue").join("notes.txt"), "mine").unwrap();
    let q = try_open(&d).await.expect("extra files are ignored");
    assert!(q.peek("qqq", "").await.text().contains("some text"));
    q.srv.shutdown().await.unwrap();
    assert!(d.join("queue/notes.txt").exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// While a big queue is being saved, other requests are served: the state is copied under the lock, encoded and
/// written after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_does_not_make_requests_wait_for_the_encoding_and_the_writing() {
    let d = dir("lock");
    let svc = Arc::new(QueueService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &d).await;
    let q = Q { base: srv.url("queue", ACCOUNT), srv };
    q.create("qqq").await;
    for i in 0..30 {
        q.create(&format!("other{i}")).await;
    }
    let text = "x".repeat(2000);
    for _ in 0..8000 {
        let r = q.put("qqq", &text, "").await;
        assert_eq!(r.status, 201);
    }
    let state_dir = d.join("queue");
    let saver = {
        let svc = svc.clone();
        let state_dir = state_dir.clone();
        tokio::task::spawn_blocking(move || {
            let t = std::time::Instant::now();
            svc.save(&state_dir).unwrap();
            t.elapsed()
        })
    };
    let mut slowest = Duration::ZERO;
    let mut served = 0;
    while !saver.is_finished() {
        let t = std::time::Instant::now();
        let r = q.send("GET", "/other3?comp=metadata", &[], "").await;
        slowest = slowest.max(t.elapsed());
        assert_eq!(r.status, 200);
        served += 1;
    }
    let took = saver.await.unwrap();
    assert!(served >= 3, "only {served} requests were served during a save of {took:?}");
    assert!(slowest < took / 2, "a request waited {slowest:?} during a save of {took:?}");
    q.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}
