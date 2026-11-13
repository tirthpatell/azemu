mod common;
use std::time::Duration;

use azemu_amqp::message::Message;
use azemu_amqp::performatives::{RcvSettleMode, SndSettleMode};
use azemu_amqp::terminus::{DeliveryState, Source};
use azemu_amqp::types::Value;
use common::*;

fn session_source(address: &str, sid: Option<&str>) -> Source {
    let mut s = Source::with_address(address);
    // a bare value, as JavaScript (rhea) sends it
    s.filter = Some(Value::Map(vec![(Value::sym("com.microsoft:session-filter"), sid.map(Value::str).unwrap_or(Value::Null))]));
    s
}

/// The same request as the Go SDK (go-amqp) writes it: the value is a described type, also for "next available" (null).
fn session_source_described(address: &str, sid: Option<&str>) -> Source {
    let mut s = Source::with_address(address);
    let value = Value::described_code(0x0000_0013_7000_000C, sid.map(Value::str).unwrap_or(Value::Null));
    s.filter = Some(Value::Map(vec![(Value::sym("com.microsoft:session-filter"), value)]));
    s
}

async fn session_receiver(
    c: &mut azemu_amqp::client::TestClient,
    address: &str,
    sid: Option<&str>,
) -> Result<u32, Option<azemu_amqp::error::Error>> {
    c.attach_receiver_source(session_source(address, sid), SndSettleMode::Unsettled, RcvSettleMode::Second, 10).await
}

async fn session_receiver_credit(c: &mut azemu_amqp::client::TestClient, address: &str, sid: &str, credit: u32) -> u32 {
    c.attach_receiver_source(session_source(address, Some(sid)), SndSettleMode::Unsettled, RcvSettleMode::Second, credit).await.unwrap()
}

fn filter_value(c: &azemu_amqp::client::TestClient, h: u32) -> Option<String> {
    let a = c.links[&h].attach.as_ref().unwrap();
    let v = a.source.as_ref().unwrap().filter_get("com.microsoft:session-filter")?;
    v.undescribed().as_str().map(str::to_string)
}

#[tokio::test]
async fn the_attach_answer_repeats_the_form_of_the_session_filter() {
    // go-amqp refuses a filter value that is not a described type; rhea compares the value with `===`
    // and so needs a bare string. The answer has the form of the request.
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    e.inject("sq", msg_session("a", "A"));
    e.inject("sq", msg_session("n", "NEXT"));
    let mut c = e.client().await;
    let h = c
        .attach_receiver_source(session_source_described("sq", Some("A")), SndSettleMode::Unsettled, RcvSettleMode::Second, 5)
        .await
        .unwrap();
    let v = c.links[&h].attach.as_ref().unwrap().source.as_ref().unwrap().filter_get("com.microsoft:session-filter").unwrap().clone();
    let d = v.as_described().expect("a described request gets a described answer");
    assert_eq!(d.descriptor.as_u64(), Some(0x0000_0013_7000_000C));
    assert_eq!(d.value, Value::str("A"));
    // "next available session", described null: the session id comes back described
    let h =
        c.attach_receiver_source(session_source_described("sq", None), SndSettleMode::Unsettled, RcvSettleMode::Second, 5).await.unwrap();
    let v = c.links[&h].attach.as_ref().unwrap().source.as_ref().unwrap().filter_get("com.microsoft:session-filter").unwrap().clone();
    assert_eq!(v.as_described().expect("described").value, Value::str("NEXT"));
    // a bare request gets a bare answer
    let mut c2 = e.client().await;
    e.inject("sq", msg_session("b", "B"));
    let h = session_receiver(&mut c2, "sq", Some("B")).await.unwrap();
    let v = c2.links[&h].attach.as_ref().unwrap().source.as_ref().unwrap().filter_get("com.microsoft:session-filter").unwrap().clone();
    assert_eq!(v, Value::str("B"));
}

#[tokio::test]
async fn named_session_receives_only_its_messages_in_order() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    for i in 0..3 {
        e.inject("sq", msg_session(&format!("a{i}"), "A"));
        e.inject("sq", msg_session(&format!("b{i}"), "B"));
    }
    let mut c = e.client().await;
    let h = session_receiver(&mut c, "sq", Some("B")).await.unwrap();
    assert_eq!(filter_value(&c, h).as_deref(), Some("B"), "the attach reply echoes the session id");
    let props = c.links[&h].attach.as_ref().unwrap().properties.clone().unwrap();
    assert!(matches!(props.map_get("com.microsoft:locked-until-utc"), Some(Value::Long(t)) if *t > 637_000_000_000_000_000));
    let mut got = vec![];
    for _ in 0..3 {
        let (d, m) = recv_body(&mut c, h).await;
        got.push(body(&m));
        assert_eq!(m.properties.as_ref().unwrap().group_id.as_deref(), Some("B"));
        assert_eq!(settle(&mut c, &d, DeliveryState::Accepted).await, Some(DeliveryState::Accepted));
    }
    assert_eq!(got, ["b0", "b1", "b2"]);
    no_delivery(&mut c, h, 150).await;
}

#[tokio::test]
async fn abandoned_message_keeps_its_place_in_the_session() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    for i in 0..3 {
        e.inject("sq", msg_session(&format!("m{i}"), "S"));
    }
    let mut c = e.client().await;
    let h = session_receiver_credit(&mut c, "sq", "S", 1).await;
    let (d, _) = recv_body(&mut c, h).await;
    settle(&mut c, &d, DeliveryState::modified(true, false)).await;
    c.flow(h, 10, false).await;
    let first = c.recv(h).await.message();
    assert_eq!(body(&first), "m0");
}

#[tokio::test]
async fn a_second_receiver_cannot_take_a_locked_session() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c1 = e.client().await;
    session_receiver(&mut c1, "sq", Some("S")).await.unwrap();
    let mut c2 = e.client().await;
    let err = session_receiver(&mut c2, "sq", Some("S")).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "com.microsoft:session-cannot-be-locked");
    // once the first receiver detaches the session is free
    let h1 = c1.links.keys().copied().next().unwrap();
    c1.detach(h1).await;
    assert!(session_receiver(&mut c2, "sq", Some("S")).await.is_ok());
}

#[tokio::test]
async fn next_available_session_picks_one_with_messages_and_skips_locked_ones() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    e.inject("sq", msg_session("a", "A"));
    e.inject("sq", msg_session("b", "B"));
    let mut c1 = e.client().await;
    let h1 = session_receiver(&mut c1, "sq", None).await.unwrap();
    assert_eq!(filter_value(&c1, h1).as_deref(), Some("A"));
    let mut c2 = e.client().await;
    let h2 = session_receiver(&mut c2, "sq", None).await.unwrap();
    assert_eq!(filter_value(&c2, h2).as_deref(), Some("B"));
}

#[tokio::test]
async fn next_available_session_waits_then_times_out_with_the_timeout_condition() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    let mut a = azemu_amqp::performatives::Attach::new("w", 0, azemu_amqp::performatives::Role::Receiver);
    a.source = Some(session_source("sq", None));
    a.target = Some(azemu_amqp::terminus::TargetKind::Target(Default::default()));
    a.properties = Some(Value::Map(vec![(Value::sym("com.microsoft:timeout"), Value::Uint(400))]));
    let t = std::time::Instant::now();
    let err = c.attach(a).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "com.microsoft:timeout");
    assert!(t.elapsed() >= Duration::from_millis(350));
}

#[tokio::test]
async fn next_available_session_attach_completes_when_a_session_appears() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    let shared = e.shared.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let m = msg_session("late", "NEW");
        let size = m.encode().len();
        shared
            .state
            .lock()
            .ns("default")
            .queue_mut("sq")
            .unwrap()
            .core
            .enqueue(azemu_servicebus::core::Incoming::new(m, size), azemu_servicebus::state::now_ms())
            .unwrap();
    });
    let h = session_receiver(&mut c, "sq", None).await.unwrap();
    assert_eq!(filter_value(&c, h).as_deref(), Some("NEW"));
    assert_eq!(body(&c.recv(h).await.message()), "late");
}

#[tokio::test]
async fn session_receiver_on_a_plain_entity_and_plain_receiver_on_a_session_entity_are_refused() {
    let e = env().await;
    e.queue("plain", |_| {});
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    assert!(session_receiver(&mut c, "plain", Some("x")).await.is_err());
    assert!(c.attach_receiver("sq", 1).await.is_err());
}

#[tokio::test]
async fn session_state_and_lock_renewal_over_management() {
    let e = env().await;
    e.queue("sq", |s| {
        s.requires_session = true;
        s.lock_duration_ms = 1000;
    });
    e.inject("sq", msg_session("x", "S"));
    let mut c = e.client().await;
    let h = session_receiver(&mut c, "sq", Some("S")).await.unwrap();
    let mut mg = Mgmt::open(&mut c, "sq/$management").await;
    let r = mg.call(&mut c, "com.microsoft:get-session-state", vec![("session-id", Value::str("S"))]).await;
    assert_eq!(r.body.map_get("session-state"), Some(&Value::Null));
    let r = mg
        .call(
            &mut c,
            "com.microsoft:set-session-state",
            vec![("session-id", Value::str("S")), ("session-state", Value::Binary(b"hello".to_vec()))],
        )
        .await;
    assert_eq!(r.status, 200);
    let r = mg.call(&mut c, "com.microsoft:get-session-state", vec![("session-id", Value::str("S"))]).await;
    assert_eq!(r.body.map_get("session-state"), Some(&Value::Binary(b"hello".to_vec())));
    let r = mg.call(&mut c, "com.microsoft:set-session-state", vec![("session-id", Value::str("S")), ("session-state", Value::Null)]).await;
    assert_eq!(r.status, 200);
    let r = mg.call(&mut c, "com.microsoft:get-session-state", vec![("session-id", Value::str("S"))]).await;
    assert_eq!(r.body.map_get("session-state"), Some(&Value::Null));

    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let r = mg.call(&mut c, "com.microsoft:renew-session-lock", vec![("session-id", Value::str("S"))]).await;
        assert_eq!(r.status, 200, "{}", r.description);
        assert!(matches!(r.body.map_get("expiration"), Some(Value::Timestamp(_))));
    }
    let d = c.recv(h).await;
    assert_eq!(settle(&mut c, &d, DeliveryState::Accepted).await, Some(DeliveryState::Accepted));
}

#[tokio::test]
async fn lost_session_lock_fails_settlement_and_state_updates() {
    let e = env().await;
    e.queue("sq", |s| {
        s.requires_session = true;
        s.lock_duration_ms = 300;
    });
    e.inject("sq", msg_session("x", "S"));
    let mut c = e.client().await;
    let h = session_receiver(&mut c, "sq", Some("S")).await.unwrap();
    let d = c.recv(h).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    match settle(&mut c, &d, DeliveryState::Accepted).await {
        Some(DeliveryState::Rejected { error: Some(err) }) => assert!(
            err.condition == "com.microsoft:session-lock-lost" || err.condition == "com.microsoft:message-lock-lost",
            "{}",
            err.condition
        ),
        other => panic!("{other:?}"),
    }
    // the link is detached by the broker once the lock is gone
    let mut mg = Mgmt::open(&mut c, "sq/$management").await;
    let r = mg.call(&mut c, "com.microsoft:renew-session-lock", vec![("session-id", Value::str("S"))]).await;
    assert_eq!((r.status, r.condition.as_deref()), (410, Some("com.microsoft:session-lock-lost")));
    // the message is back for somebody else
    let mut c2 = e.client().await;
    let h2 = session_receiver(&mut c2, "sq", Some("S")).await.unwrap();
    let m = c2.recv(h2).await.message();
    assert_eq!(m.header.unwrap().delivery_count, Some(1));
}

#[tokio::test]
async fn closing_a_session_receiver_releases_the_session_and_its_locks() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    e.inject("sq", msg_session("x", "S"));
    let mut c = e.client().await;
    let h = session_receiver(&mut c, "sq", Some("S")).await.unwrap();
    c.recv(h).await;
    c.detach(h).await;
    let mut c2 = e.client().await;
    let h2 = session_receiver(&mut c2, "sq", Some("S")).await.unwrap();
    let m = c2.recv(h2).await.message();
    assert_eq!(m.header.unwrap().delivery_count, Some(0), "a clean close is not a failed attempt");
}

#[tokio::test]
async fn list_sessions_over_management() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    for s in ["c", "a", "b"] {
        e.inject("sq", msg_session("x", s));
    }
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "sq/$management").await;
    let r = mg
        .call(
            &mut c,
            "com.microsoft:get-message-sessions",
            vec![("last-updated-time", Value::Timestamp(0)), ("skip", Value::Int(0)), ("top", Value::Int(100))],
        )
        .await;
    assert_eq!(r.status, 200);
    let ids: Vec<&str> = r.body.map_get("sessions-ids").unwrap().as_list().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(ids, ["a", "b", "c"]);
    let r = mg.call(&mut c, "com.microsoft:get-message-sessions", vec![("skip", Value::Int(5)), ("top", Value::Int(10))]).await;
    assert_eq!(r.status, 204);
}

#[tokio::test]
async fn peek_of_one_session() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    e.inject("sq", msg_session("a", "A"));
    e.inject("sq", msg_session("b", "B"));
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "sq/$management").await;
    let r = mg
        .call(
            &mut c,
            "com.microsoft:peek-message",
            vec![("from-sequence-number", Value::Long(1)), ("message-count", Value::Int(10)), ("session-id", Value::str("B"))],
        )
        .await;
    let list = r.body.map_get("messages").unwrap().as_list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(body(&Message::decode(list[0].map_get("message").unwrap().as_binary().unwrap()).unwrap()), "b");
}

#[tokio::test]
async fn a_session_receiver_on_an_autocreated_queue_makes_it_session_enabled() {
    let e = env().await;
    let mut c = e.client().await;
    session_receiver(&mut c, "auto-sq", Some("S")).await.unwrap();
    assert!(e.core("auto-sq", |c| c.settings.requires_session));
}

#[tokio::test]
async fn the_empty_string_is_a_valid_session_id() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    e.inject("sq", msg_session("x", ""));
    let mut c = e.client().await;
    let h = session_receiver(&mut c, "sq", Some("")).await.unwrap();
    let m = c.recv(h).await.message();
    assert_eq!(m.properties.unwrap().group_id.as_deref(), Some(""));
}

#[tokio::test]
async fn the_dead_letter_queue_of_a_session_queue_needs_no_session() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    e.inject("sq", msg_session("x", "S"));
    let mut c = e.client().await;
    let h = session_receiver(&mut c, "sq", Some("S")).await.unwrap();
    let d = c.recv(h).await;
    settle(&mut c, &d, DeliveryState::rejected(azemu_amqp::error::Error::new("com.microsoft:dead-letter", "d"))).await;
    let dl = c.attach_receiver("sq/$DeadLetterQueue", 5).await.unwrap();
    let m = c.recv(dl).await.message();
    assert_eq!(m.properties.unwrap().group_id.as_deref(), Some("S"), "the session id is kept");
}

// ---- a lost session lock (finding SB1) -------------------------------------------------------------

fn lock_error(out: Option<DeliveryState>) -> String {
    match out {
        Some(DeliveryState::Rejected { error: Some(err) }) => err.condition,
        other => panic!("expected a rejection with an error, got {other:?}"),
    }
}

#[tokio::test]
async fn settling_after_the_session_lock_ran_out_is_session_lock_lost() {
    // The SDKs map com.microsoft:session-lock-lost to SessionLockLost ("Request a new session receiver") and
    // com.microsoft:message-lock-lost to MessageLockLost; a session receiver's messages are held by the session lock.
    let e = env().await;
    e.queue("sq", |s| {
        s.requires_session = true;
        s.lock_duration_ms = 300;
    });
    for i in 0..4 {
        e.inject("sq", msg_session(&format!("m{i}"), "A"));
    }
    let mut c = e.client().await;
    let h = c.attach_receiver_source(session_source("sq", Some("A")), SndSettleMode::Unsettled, RcvSettleMode::Second, 4).await.unwrap();
    let mut ds = Vec::new();
    for _ in 0..4 {
        ds.push(c.recv(h).await);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    let states = [
        DeliveryState::Accepted,
        DeliveryState::Modified { delivery_failed: true, undeliverable_here: false, message_annotations: None },
        DeliveryState::Modified { delivery_failed: false, undeliverable_here: true, message_annotations: None },
        DeliveryState::rejected(azemu_amqp::Error::new("com.microsoft:dead-letter", "x")),
    ];
    for (d, state) in ds.iter().zip(states) {
        let out = settle(&mut c, d, state.clone()).await;
        assert_eq!(lock_error(out), "com.microsoft:session-lock-lost", "{state:?}");
    }
}

#[tokio::test]
async fn settling_a_message_of_a_queue_without_sessions_stays_message_lock_lost() {
    let e = env().await;
    e.queue("q", |s| s.lock_duration_ms = 300);
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 1).await;
    let d = c.recv(r).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let out = settle(&mut c, &d, DeliveryState::Accepted).await;
    assert_eq!(lock_error(out), "com.microsoft:message-lock-lost");
}

#[tokio::test]
async fn management_calls_on_a_lost_session_lock_say_session_lock_lost() {
    let e = env().await;
    e.queue("sq", |s| {
        s.requires_session = true;
        s.lock_duration_ms = 300;
    });
    e.inject("sq", msg_session("x", "A"));
    let mut c = e.client().await;
    let h = c.attach_receiver_source(session_source("sq", Some("A")), SndSettleMode::Unsettled, RcvSettleMode::Second, 1).await.unwrap();
    let d = c.recv(h).await;
    let token = Value::Uuid(azemu_servicebus::store::tag_to_token(&d.tag).unwrap());
    let mut mg = Mgmt::open(&mut c, "sq/$management").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let r = mg
        .call(
            &mut c,
            "com.microsoft:update-disposition",
            vec![
                ("disposition-status", Value::str("completed")),
                ("lock-tokens", Value::Array(vec![token])),
                ("session-id", Value::str("A")),
            ],
        )
        .await;
    assert_eq!((r.status, r.condition.as_deref()), (410, Some("com.microsoft:session-lock-lost")), "{}", r.description);
    for op in ["com.microsoft:renew-session-lock", "com.microsoft:get-session-state"] {
        let r = mg.call(&mut c, op, vec![("session-id", Value::str("A"))]).await;
        assert_eq!((r.status, r.condition.as_deref()), (410, Some("com.microsoft:session-lock-lost")), "{op}: {}", r.description);
    }
    let r = mg
        .call(
            &mut c,
            "com.microsoft:set-session-state",
            vec![("session-id", Value::str("A")), ("session-state", Value::Binary(b"s".to_vec()))],
        )
        .await;
    assert_eq!((r.status, r.condition.as_deref()), (410, Some("com.microsoft:session-lock-lost")), "{}", r.description);
}

// ---- growth (review of the parts the reviewer did not reach) -----------------------------------------

#[tokio::test]
async fn session_state_is_limited_to_256_kb_and_idle_session_records_are_dropped() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let now = azemu_servicebus::state::now_ms();
    e.core("sq", |c| {
        let owner = azemu_servicebus::store::Owner { conn: 1, link: 1 };
        c.accept_session("A", owner, now).unwrap();
        assert!(c.set_session_state("A", Some(vec![0; 256 * 1024]), now).is_ok());
        let err = c.set_session_state("A", Some(vec![0; 256 * 1024 + 1]), now).unwrap_err();
        assert_eq!(err.condition, "com.microsoft:argument-error");
        // a state that was set keeps the record after the lock goes; one without state does not
        c.accept_session("B", owner, now).unwrap();
        c.release_session("A", owner);
        c.release_session("B", owner);
        assert!(c.sessions.contains_key("A") && !c.sessions.contains_key("B"));
        // made-up session ids: set-state without a lock leaves no record behind
        for i in 0..100 {
            assert!(c.set_session_state(&format!("ghost{i}"), Some(vec![1]), now).is_err());
        }
        assert_eq!(c.sessions.len(), 1);
    });
}

#[tokio::test]
async fn scheduled_messages_of_a_topic_are_limited_by_the_size_of_the_topic() {
    let e = env().await;
    e.state(|st| {
        let mut s = azemu_servicebus::model::Settings::topic_default(azemu_servicebus::state::now_ms());
        s.max_size_mb = 1;
        st.ns("default").create_topic("t", s).unwrap();
    });
    let now = azemu_servicebus::state::now_ms();
    let big = |i: usize| {
        let mut m = msg_id("b", &format!("s{i}"));
        m.body = azemu_amqp::message::Body::Data(vec![vec![b'x'; 200_000]]);
        m.message_annotations = Some(vec![(Value::sym("x-opt-scheduled-enqueue-time"), Value::Timestamp(now + 3_600_000))]);
        let size = m.encode().len();
        azemu_servicebus::core::Incoming::new(m, size)
    };
    let results: Vec<bool> = e.state(|st| {
        (0..8).map(|i| st.send("default", &azemu_servicebus::model::Address::Topic { name: "t".into() }, big(i), now).is_ok()).collect()
    });
    // 1 MiB holds five of 200,000 bytes
    assert_eq!(results.iter().filter(|r| **r).count(), 5, "{results:?}");
}
