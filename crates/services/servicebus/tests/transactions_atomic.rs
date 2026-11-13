//! A transaction commit applies everything or nothing: operations that only
//! fail when they are applied together (the room in a queue, a subscription that is full) take the
//! whole commit back, including what was applied before the one that failed.

mod common;
use std::time::Duration;

use azemu_amqp::client::TestClient;
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::types::Value;
use azemu_servicebus::store::MsgState;
use common::*;

fn t(txn: &[u8], outcome: DeliveryState) -> DeliveryState {
    DeliveryState::Transactional { txn_id: txn.to_vec(), outcome: Some(Box::new(outcome)) }
}

async fn controller(c: &mut TestClient) -> (u32, Vec<u8>) {
    let coord = c.attach_coordinator().await.expect("coordinator attach");
    let txn = c.declare(coord).await.expect("declare");
    (coord, txn)
}

fn big(text: &str, id: &str, bytes: usize) -> azemu_amqp::message::Message {
    let mut m = msg_id(text, id);
    m.body = azemu_amqp::message::Body::Data(vec![vec![b'x'; bytes]]);
    m
}

fn stored(e: &Env, q: &str) -> usize {
    e.core(q, |c| c.main.len())
}

async fn rolled_back(c: &mut TestClient, coord: u32, txn: &[u8]) {
    let err = c.discharge(coord, txn, false).await.unwrap_err().expect("an error");
    assert_eq!(err.condition, "amqp:transaction:rollback", "{err:?}");
}

#[tokio::test]
async fn sends_that_do_not_fit_together_fail_the_whole_commit() {
    // the review's case: queue limit 1 MB, five sends of 250,000 bytes. Each fits when it is made.
    let e = env().await;
    e.queue("q", |s| s.max_size_mb = 1);
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    for i in 0..5 {
        let out = c.send_message_in_txn(s, &big("b", &format!("m{i}"), 250_000), &txn).await;
        assert_eq!(out, Some(t(&txn, DeliveryState::Accepted)), "send {i}");
    }
    rolled_back(&mut c, coord, &txn).await;
    assert_eq!(stored(&e, "q"), 0, "none of the five may be stored");
    assert_eq!(e.core("q", |c| c.size_bytes()), 0);
    // the entity still works and its size count is right: four fit, the fifth does not
    let txn = c.declare(coord).await.unwrap();
    for i in 0..4 {
        assert_eq!(c.send_message_in_txn(s, &big("b", &format!("n{i}"), 250_000), &txn).await, Some(t(&txn, DeliveryState::Accepted)));
    }
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(stored(&e, "q"), 4);
}

#[tokio::test]
async fn a_control_with_room_for_all_commits_all() {
    let e = env().await;
    e.queue("q", |s| s.max_size_mb = 100);
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    for i in 0..5 {
        c.send_message_in_txn(s, &big("b", &format!("m{i}"), 250_000), &txn).await;
    }
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(stored(&e, "q"), 5);
}

#[tokio::test]
async fn a_failed_commit_takes_back_the_earlier_operations_of_every_kind() {
    let e = env().await;
    e.queue("in", |s| s.lock_duration_ms = 60_000);
    e.queue("out", |_| {});
    e.queue("full", |s| s.max_size_mb = 1);
    e.queue("dup", |s| s.requires_duplicate_detection = true);
    e.inject("in", msg_id("work", "w1"));
    e.inject("in", msg_id("work2", "w2"));
    e.inject("in", msg_id("work3", "w3"));
    // `full` already holds 3 x 300,000 bytes
    for i in 0..3 {
        e.inject("full", big("f", &format!("f{i}"), 250_000));
    }
    let mut c = e.client().await;
    let r = receiver(&mut c, "in", 5).await;
    let d1 = c.recv(r).await;
    let d2 = c.recv(r).await;
    let d3 = c.recv(r).await;
    let (coord, txn) = controller(&mut c).await;
    let out = sender(&mut c, "out").await;
    let dup = sender(&mut c, "dup").await;
    let full = sender(&mut c, "full").await;
    // complete, abandon, dead-letter, a plain send, a send that registers a duplicate id ...
    assert!(settle(&mut c, &d1, t(&txn, DeliveryState::Accepted)).await.is_some());
    assert!(settle(&mut c, &d2, t(&txn, DeliveryState::Released)).await.is_some());
    assert!(settle(&mut c, &d3, t(&txn, DeliveryState::rejected(azemu_amqp::Error::new("com.microsoft:dead-letter", "no"))))
        .await
        .is_some());
    c.send_message_in_txn(out, &msg("to out"), &txn).await;
    c.send_message_in_txn(dup, &msg_id("d", "same-id"), &txn).await;
    // ... and at the end the send that does not fit once the first one is stored
    assert_eq!(c.send_message_in_txn(full, &big("f", "fa", 250_000), &txn).await, Some(t(&txn, DeliveryState::Accepted)));
    assert_eq!(c.send_message_in_txn(full, &big("f", "fb", 250_000), &txn).await, Some(t(&txn, DeliveryState::Accepted)));
    rolled_back(&mut c, coord, &txn).await;
    // nothing moved
    assert_eq!(stored(&e, "in"), 3, "the three messages are still there");
    assert_eq!(
        e.core("in", |c| c.main.msgs.values().filter(|m| m.state == MsgState::Locked).count()),
        3,
        "and still locked by their receiver"
    );
    assert_eq!(e.core("in", |c| c.dlq.len()), 0);
    assert_eq!(stored(&e, "out"), 0);
    assert_eq!(stored(&e, "dup"), 0);
    assert_eq!(stored(&e, "full"), 3);
    assert_eq!(
        e.core("full", |c| c.size_bytes()),
        e.core("full", |c| c.main.msgs.values().map(|m| azemu_servicebus::store::charged(m.size)).sum::<u64>())
    );
    // the duplicate id was not remembered: the same id is accepted by a plain send now
    let s = sender(&mut c, "dup").await;
    send(&mut c, s, &msg_id("d2", "same-id")).await;
    assert_eq!(stored(&e, "dup"), 1);
}

#[tokio::test]
async fn operations_are_applied_in_the_order_they_were_made() {
    // the queue (1 MB) holds three messages of 250,000 bytes: room for one more. Two sends and a complete
    // (which frees 250,000 bytes) fit together only when the complete comes first.
    for complete_first in [true, false] {
        let e = env().await;
        e.queue("q", |s| s.max_size_mb = 1);
        for i in 0..3 {
            e.inject("q", big("f", &format!("f{i}"), 250_000));
        }
        let mut c = e.client().await;
        let r = receiver(&mut c, "q", 1).await;
        let d = c.recv(r).await;
        let (coord, txn) = controller(&mut c).await;
        let s = sender(&mut c, "q").await;
        if complete_first {
            assert!(settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await.is_some());
        }
        for i in 0..2 {
            let out = c.send_message_in_txn(s, &big("n", &format!("n{i}"), 250_000), &txn).await;
            assert_eq!(out, Some(t(&txn, DeliveryState::Accepted)), "each send fits when it is made");
        }
        if !complete_first {
            assert!(settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await.is_some());
        }
        if complete_first {
            c.discharge(coord, &txn, false).await.unwrap();
            assert_eq!(stored(&e, "q"), 4, "3 - 1 + 2");
        } else {
            rolled_back(&mut c, coord, &txn).await;
            assert_eq!(stored(&e, "q"), 3, "nothing changed");
            assert_eq!(e.core("q", |c| c.main.msgs.values().filter(|m| m.state == MsgState::Locked).count()), 1);
        }
    }
}

#[tokio::test]
async fn a_topic_send_with_a_subscription_that_is_full_fails_the_commit_and_leaves_no_copies() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "roomy", vec![azemu_servicebus::rules::Rule::default_rule(0)], |_| {});
    e.sub("t", "tight", vec![azemu_servicebus::rules::Rule::default_rule(0)], |s| s.max_size_mb = 1);
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "t").await;
    for i in 0..5 {
        assert_eq!(c.send_message_in_txn(s, &big("b", &format!("m{i}"), 250_000), &txn).await, Some(t(&txn, DeliveryState::Accepted)));
    }
    rolled_back(&mut c, coord, &txn).await;
    let counts = e.state(|st| {
        let topic = st.ns("default").topic("t").unwrap();
        (topic.subs["roomy"].core.main.len(), topic.subs["tight"].core.main.len())
    });
    assert_eq!(counts, (0, 0), "the copies that fitted must go too");
}

#[tokio::test]
async fn a_scheduled_send_and_a_session_state_are_taken_back_too() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    e.queue("full", |s| s.max_size_mb = 1);
    e.inject("sq", msg_session("x", "S1"));
    for i in 0..3 {
        e.inject("full", big("f", &format!("f{i}"), 250_000));
    }
    let mut c = e.client().await;
    let mut src = azemu_amqp::terminus::Source::with_address("sq");
    src.filter = Some(Value::Map(vec![(Value::sym("com.microsoft:session-filter"), Value::str("S1"))]));
    let _r = c
        .attach_receiver_source(
            src,
            azemu_amqp::performatives::SndSettleMode::Unsettled,
            azemu_amqp::performatives::RcvSettleMode::Second,
            1,
        )
        .await
        .unwrap();
    let (coord, txn) = controller(&mut c).await;
    let mut mg = Mgmt::open(&mut c, "sq/$management").await;
    let reply = mg
        .call_in(
            &mut c,
            "com.microsoft:set-session-state",
            vec![("session-id", Value::str("S1")), ("session-state", Value::Binary(b"state".to_vec()))],
            Some(&txn),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.description);
    let s = sender(&mut c, "full").await;
    c.send_message_in_txn(s, &big("a", "a", 250_000), &txn).await;
    c.send_message_in_txn(s, &big("b", "b", 250_000), &txn).await;
    rolled_back(&mut c, coord, &txn).await;
    assert_eq!(e.core("sq", |c| c.session_state("S1")), None, "the session state is as it was");
    assert_eq!(stored(&e, "full"), 3);
}

#[tokio::test]
async fn a_commit_that_succeeds_after_a_failed_one_on_the_same_entities_is_not_affected() {
    let e = env().await;
    e.queue("q", |s| s.max_size_mb = 1);
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    for i in 0..5 {
        c.send_message_in_txn(s, &big("b", &format!("m{i}"), 250_000), &txn).await;
    }
    rolled_back(&mut c, coord, &txn).await;
    for round in 0..3 {
        let txn = c.declare(coord).await.unwrap();
        c.send_message_in_txn(s, &msg_id("ok", &format!("ok{round}")), &txn).await;
        c.discharge(coord, &txn, false).await.unwrap();
    }
    assert_eq!(stored(&e, "q"), 3);
    // a receiver gets them in order
    let r = receiver_rd(&mut c, "q", 5).await;
    for round in 0..3 {
        let (_, m) = recv_body(&mut c, r).await;
        assert_eq!(body(&m), "ok");
        let _ = round;
    }
    no_delivery(&mut c, r, 100).await;
    let _ = Duration::from_millis(1);
}
