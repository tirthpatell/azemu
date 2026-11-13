//! AMQP 1.0 transactions: the coordinator link, `declare` / `discharge`, transactional
//! sends, dispositions and `$management` requests, timeouts, limits and what happens when the controller goes away.

mod common;
use std::time::Duration;

use azemu_amqp::client::TestClient;
use azemu_amqp::message::{Message, BATCH_MESSAGE_FORMAT};
use azemu_amqp::performatives::{Attach, RcvSettleMode, Role, SndSettleMode};
use azemu_amqp::terminus::{DeliveryState, Source, Target, TargetKind};
use azemu_amqp::types::Value;
use azemu_servicebus::rules::{CorrelationFilter, Rule, RuleFilter};
use common::*;

const WAIT: Duration = Duration::from_secs(3);

fn t(txn: &[u8], outcome: DeliveryState) -> DeliveryState {
    DeliveryState::Transactional { txn_id: txn.to_vec(), outcome: Some(Box::new(outcome)) }
}

/// The answer a transactional send gets.
fn accepted_in(txn: &[u8]) -> Option<DeliveryState> {
    Some(t(txn, DeliveryState::Accepted))
}

async fn controller(c: &mut TestClient) -> (u32, Vec<u8>) {
    let coord = c.attach_coordinator().await.expect("coordinator attach");
    let txn = c.declare(coord).await.expect("declare");
    (coord, txn)
}

fn active(e: &Env, q: &str) -> usize {
    e.core(q, |c| c.counts().active)
}

/// Messages of a queue that a receiver could be given now (not locked, not deferred, not scheduled).
fn ready(e: &Env, q: &str) -> usize {
    e.core(q, |c| c.main.msgs.values().filter(|m| m.state == azemu_servicebus::store::MsgState::Ready).count())
}

fn scheduled_msg(text: &str, in_ms: i64) -> Message {
    let mut m = msg_id(text, text);
    m.message_annotations =
        Some(vec![(Value::sym("x-opt-scheduled-enqueue-time"), Value::Timestamp(azemu_servicebus::state::now_ms() + in_ms))]);
    m
}

fn sched_request(m: &Message, id: &str) -> Value {
    Value::Map(vec![(Value::str("message-id"), Value::str(id)), (Value::str("message"), Value::Binary(m.encode()))])
}

// ---- the coordinator link -----------------------------------------------------------------------

#[tokio::test]
async fn declare_gives_distinct_ids_and_discharge_answers_accepted() {
    let e = env().await;
    let mut c = e.client().await;
    let coord = c.attach_coordinator().await.expect("attach");
    let a = c.declare(coord).await.unwrap();
    let b = c.declare(coord).await.unwrap();
    assert!(!a.is_empty() && a != b);
    c.discharge(coord, &a, false).await.unwrap();
    c.discharge(coord, &b, true).await.unwrap();
}

#[tokio::test]
async fn the_coordinator_attach_needs_no_token_and_echoes_the_coordinator_target() {
    let e = env().await;
    let mut c = e.client().await;
    let coord = c.attach_coordinator().await.unwrap();
    let resp = c.attach_of(coord).expect("attach answer");
    assert!(matches!(resp.target, Some(TargetKind::Coordinator(_))), "{:?}", resp.target);
}

#[tokio::test]
async fn a_receiver_attached_to_the_coordinator_is_refused() {
    let e = env().await;
    let mut c = e.client().await;
    let mut a = Attach::new("txn", 0, Role::Receiver);
    a.source = Some(Source::default());
    a.target = Some(TargetKind::Coordinator(Default::default()));
    let err = c.attach(a).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:not-allowed");
}

#[tokio::test]
async fn discharging_an_unknown_id_or_the_same_id_twice_is_unknown_id() {
    let e = env().await;
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let err = c.discharge(coord, b"no such transaction", false).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:unknown-id");
    c.discharge(coord, &txn, false).await.unwrap();
    let err = c.discharge(coord, &txn, false).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:unknown-id");
    let err = c.discharge(coord, &txn, true).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:unknown-id");
}

#[tokio::test]
async fn work_in_an_unknown_or_finished_transaction_is_rejected() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    c.discharge(coord, &txn, true).await.unwrap();
    let s = sender(&mut c, "q").await;
    let out = c.send_message_in_txn(s, &msg("late"), &txn).await;
    match out {
        Some(DeliveryState::Transactional { outcome: Some(o), .. }) => match *o {
            DeliveryState::Rejected { error: Some(err) } => assert_eq!(err.condition, "amqp:transaction:unknown-id"),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    assert_eq!(active(&e, "q"), 0);
}

#[tokio::test]
async fn a_transaction_id_of_another_connection_is_unknown() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut a = e.client().await;
    let (_, txn) = controller(&mut a).await;
    let mut b = e.client().await;
    let coord_b = b.attach_coordinator().await.unwrap();
    let s = sender(&mut b, "q").await;
    let out = b.send_message_in_txn(s, &msg("x"), &txn).await;
    assert!(
        matches!(out, Some(DeliveryState::Transactional { outcome: Some(ref o), .. }) if matches!(**o, DeliveryState::Rejected { .. })),
        "{out:?}"
    );
    let err = b.discharge(coord_b, &txn, false).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:unknown-id");
    assert_eq!(active(&e, "q"), 0);
}

#[tokio::test]
async fn a_message_that_is_not_declare_or_discharge_is_rejected() {
    let e = env().await;
    let mut c = e.client().await;
    let coord = c.attach_coordinator().await.unwrap();
    let out = c.send_message_wait(coord, &msg("hello")).await;
    match out {
        Some(DeliveryState::Rejected { error: Some(err) }) => assert_eq!(err.condition, "amqp:not-allowed"),
        other => panic!("{other:?}"),
    }
}

// ---- sends ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_send_in_a_transaction_is_invisible_until_commit() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    assert_eq!(c.send_message_in_txn(s, &msg("one"), &txn).await, accepted_in(&txn));
    assert_eq!(c.send_message_in_txn(s, &msg("two"), &txn).await, accepted_in(&txn));
    let r = receiver(&mut c, "q", 5).await;
    no_delivery(&mut c, r, 200).await;
    assert_eq!(active(&e, "q"), 0);
    c.discharge(coord, &txn, false).await.unwrap();
    let (d1, m1) = recv_body(&mut c, r).await;
    let (d2, m2) = recv_body(&mut c, r).await;
    assert_eq!((body(&m1).as_str(), body(&m2).as_str()), ("one", "two"));
    settle(&mut c, &d1, DeliveryState::Accepted).await;
    settle(&mut c, &d2, DeliveryState::Accepted).await;
}

#[tokio::test]
async fn a_rollback_discards_the_sends() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    c.send_message_in_txn(s, &msg("gone"), &txn).await;
    c.discharge(coord, &txn, true).await.unwrap();
    let r = receiver(&mut c, "q", 5).await;
    no_delivery(&mut c, r, 300).await;
    assert_eq!(active(&e, "q"), 0);
    // the rolled back transaction is finished
    let out = c.send_message_in_txn(s, &msg("again"), &txn).await;
    assert!(
        matches!(out, Some(DeliveryState::Transactional { outcome: Some(ref o), .. }) if matches!(**o, DeliveryState::Rejected { .. }))
    );
}

#[tokio::test]
async fn a_batch_sent_in_a_transaction_commits_as_a_whole() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    let payload = Message::encode_batch(&[msg("a").encode(), msg("b").encode(), msg("c").encode()]);
    let id = c
        .send_payload_with_state(
            s,
            &payload,
            BATCH_MESSAGE_FORMAT,
            false,
            Some(DeliveryState::Transactional { txn_id: txn.clone(), outcome: None }),
        )
        .await;
    assert_eq!(c.wait_disposition(id).await, accepted_in(&txn));
    assert_eq!(active(&e, "q"), 0);
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(active(&e, "q"), 3);
}

#[tokio::test]
async fn transactional_and_plain_work_mix_on_one_link() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    c.send_message_in_txn(s, &msg("in the transaction"), &txn).await;
    send(&mut c, s, &msg("plain")).await;
    // the plain one is there at once, the other after the commit
    assert_eq!(active(&e, "q"), 1);
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(active(&e, "q"), 2);
    let r = receiver_rd(&mut c, "q", 5).await;
    assert_eq!(body(&c.recv(r).await.message()), "plain");
    assert_eq!(body(&c.recv(r).await.message()), "in the transaction");
}

#[tokio::test]
async fn two_transactions_at_once_are_independent() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let coord = c.attach_coordinator().await.unwrap();
    let a = c.declare(coord).await.unwrap();
    let b = c.declare(coord).await.unwrap();
    let s = sender(&mut c, "q").await;
    c.send_message_in_txn(s, &msg("from a"), &a).await;
    c.send_message_in_txn(s, &msg("from b"), &b).await;
    c.discharge(coord, &b, false).await.unwrap();
    c.discharge(coord, &a, true).await.unwrap();
    assert_eq!(active(&e, "q"), 1);
    let r = receiver_rd(&mut c, "q", 5).await;
    assert_eq!(body(&c.recv(r).await.message()), "from b");
}

#[tokio::test]
async fn a_send_that_would_be_refused_is_refused_when_it_is_made() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    e.queue("off", |s| s.status = "Disabled".into());
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    // no session id on a session queue
    let s = sender(&mut c, "sq").await;
    match c.send_message_in_txn(s, &msg("no session"), &txn).await {
        Some(DeliveryState::Transactional { outcome: Some(o), .. }) => match *o {
            DeliveryState::Rejected { error: Some(err) } => assert_eq!(err.condition, "com.microsoft:argument-error"),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    // the transaction is still usable and commits what was good
    assert_eq!(c.send_message_in_txn(s, &msg_session("fine", "S1"), &txn).await, accepted_in(&txn));
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(active(&e, "sq"), 1);
    // an entity that does not exist: the attach is refused (auto-creation is on, so use a deleted one)
    let s2 = sender(&mut c, "gone").await;
    e.state(|st| {
        st.ns("default").queues.remove("gone");
    });
    let txn2 = c.declare(coord).await.unwrap();
    match c.send_message_in_txn(s2, &msg("x"), &txn2).await {
        Some(DeliveryState::Transactional { outcome: Some(o), .. }) => assert!(matches!(*o, DeliveryState::Rejected { .. }), "{o:?}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn duplicate_detection_works_for_committed_transactional_sends() {
    let e = env().await;
    e.queue("q", |s| s.requires_duplicate_detection = true);
    let mut c = e.client().await;
    let coord = c.attach_coordinator().await.unwrap();
    let s = sender(&mut c, "q").await;
    let t1 = c.declare(coord).await.unwrap();
    c.send_message_in_txn(s, &msg_id("first", "same"), &t1).await;
    c.send_message_in_txn(s, &msg_id("second", "same"), &t1).await;
    c.discharge(coord, &t1, false).await.unwrap();
    assert_eq!(active(&e, "q"), 1, "the second copy inside the transaction is a duplicate");
    // a rolled back send does not use up the id
    let t2 = c.declare(coord).await.unwrap();
    c.send_message_in_txn(s, &msg_id("rolled back", "other"), &t2).await;
    c.discharge(coord, &t2, true).await.unwrap();
    send(&mut c, s, &msg_id("plain", "other")).await;
    assert_eq!(active(&e, "q"), 2);
    // a plain send with the id of a committed one is a duplicate too
    send(&mut c, s, &msg_id("dup", "same")).await;
    assert_eq!(active(&e, "q"), 2);
    // and a transactional send with the id of a plain one is dropped at commit
    let t3 = c.declare(coord).await.unwrap();
    c.send_message_in_txn(s, &msg_id("dup again", "other"), &t3).await;
    c.discharge(coord, &t3, false).await.unwrap();
    assert_eq!(active(&e, "q"), 2);
}

#[tokio::test]
async fn sessions_work_for_committed_transactional_sends() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "sq").await;
    c.send_message_in_txn(s, &msg_session("a1", "A"), &txn).await;
    c.send_message_in_txn(s, &msg_session("b1", "B"), &txn).await;
    c.send_message_in_txn(s, &msg_session("a2", "A"), &txn).await;
    let mut src = Source::with_address("sq");
    src.filter = Some(Value::Map(vec![(Value::sym("com.microsoft:session-filter"), Value::str("A"))]));
    let r = c.attach_receiver_source(src, SndSettleMode::Unsettled, RcvSettleMode::Second, 5).await.unwrap();
    no_delivery(&mut c, r, 200).await;
    c.discharge(coord, &txn, false).await.unwrap();
    // the receiver that was already waiting for session A gets its two messages, in order
    let first = c.recv_for(r, WAIT).await.expect("woken by the commit");
    let second = c.recv_for(r, WAIT).await.expect("second");
    assert_eq!((body(&first.message()).as_str(), body(&second.message()).as_str()), ("a1", "a2"));
    assert_eq!(active(&e, "sq"), 3);
}

#[tokio::test]
async fn a_scheduled_send_in_a_transaction_is_scheduled_when_it_commits() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    c.send_message_in_txn(s, &scheduled_msg("later", 400), &txn).await;
    assert_eq!(e.core("q", |c| (c.counts().active, c.counts().scheduled)), (0, 0));
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("q", |c| (c.counts().active, c.counts().scheduled)), (0, 1));
    let r = receiver(&mut c, "q", 2).await;
    let got = c.recv_for(r, WAIT).await.expect("delivered when due");
    assert_eq!(body(&got.message()), "later");
}

#[tokio::test]
async fn a_send_to_a_topic_in_a_transaction_fans_out_at_commit() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "all", vec![Rule::default_rule(0)], |_| {});
    e.sub(
        "t",
        "red",
        vec![Rule {
            name: "r".into(),
            filter: RuleFilter::Correlation(CorrelationFilter { label: Some("red".into()), ..Default::default() }),
            action: None,
            created_ms: 0,
        }],
        |_| {},
    );
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "t").await;
    let mut red = msg_id("red one", "m1");
    red.properties.as_mut().unwrap().subject = Some("red".into());
    let mut blue = msg_id("blue one", "m2");
    blue.properties.as_mut().unwrap().subject = Some("blue".into());
    assert_eq!(c.send_message_in_txn(s, &red, &txn).await, accepted_in(&txn));
    assert_eq!(c.send_message_in_txn(s, &blue, &txn).await, accepted_in(&txn));
    let count = |sub: &str| e.state(|st| st.ns("default").topic("t").unwrap().subs[sub].core.counts().active);
    assert_eq!((count("all"), count("red")), (0, 0));
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!((count("all"), count("red")), (2, 1));
    // a rolled back one reaches nobody
    let txn2 = c.declare(coord).await.unwrap();
    c.send_message_in_txn(s, &red, &txn2).await;
    c.discharge(coord, &txn2, true).await.unwrap();
    assert_eq!((count("all"), count("red")), (2, 1));
}

#[tokio::test]
async fn a_send_via_link_sends_to_its_destination() {
    let e = env().await;
    e.queue("via", |_| {});
    e.queue("dest", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let mut a = Attach::new("via-link", 0, Role::Sender);
    a.source = Some(Source::with_address("sender"));
    a.target = Some(TargetKind::Target(Target::with_address("via")));
    a.properties = Some(Value::Map(vec![(Value::sym("com.microsoft:transfer-destination-address"), Value::str("dest"))]));
    let h = c.attach(a).await.unwrap();
    c.wait_until("credit", |c| c.credit(h) > 0).await;
    let resp = c.attach_of(h).unwrap();
    assert!(
        resp.properties.as_ref().and_then(|p| p.map_get("com.microsoft:transfer-destination-address")).is_some(),
        "the property is echoed"
    );
    assert_eq!(c.send_message_in_txn(h, &msg("via"), &txn).await, accepted_in(&txn));
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!((active(&e, "via"), active(&e, "dest")), (0, 1));
}

// ---- dispositions --------------------------------------------------------------------------------

async fn one_locked(e: &Env, c: &mut TestClient, q: &str) -> (u32, azemu_amqp::client::Received) {
    e.inject(q, msg_id("work", "w1"));
    let r = receiver(c, q, 5).await;
    let d = c.recv(r).await;
    (r, d)
}

#[tokio::test]
async fn complete_in_a_transaction_is_applied_at_commit() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (_r, d) = one_locked(&e, &mut c, "q").await;
    let (coord, txn) = controller(&mut c).await;
    let echo = settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await;
    assert_eq!(echo, accepted_in(&txn));
    // not removed yet
    assert_eq!(e.core("q", |c| c.main.len()), 1);
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("q", |c| c.main.len()), 0);
}

#[tokio::test]
async fn complete_in_a_transaction_that_rolls_back_leaves_the_message_locked_by_its_receiver() {
    let e = env().await;
    e.queue("q", |s| s.lock_duration_ms = 700);
    let mut c = e.client().await;
    let (r, d) = one_locked(&e, &mut c, "q").await;
    let (coord, txn) = controller(&mut c).await;
    settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await;
    c.discharge(coord, &txn, true).await.unwrap();
    // still there and still locked: not delivered to anybody else
    assert_eq!((e.core("q", |c| c.main.len()), ready(&e, "q")), (1, 0));
    let mut other = e.client().await;
    let r2 = receiver(&mut other, "q", 5).await;
    no_delivery(&mut other, r2, 200).await;
    // the delivery was settled on the wire by the transactional disposition, but the lock is still the receiver's:
    // it can complete the message by its lock token over $management
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let reply = mg
        .call(
            &mut c,
            "com.microsoft:update-disposition",
            vec![("disposition-status", Value::str("completed")), ("lock-tokens", Value::Array(vec![uuid_value(&d.tag)]))],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.description);
    assert_eq!(e.core("q", |c| c.main.len()), 0);
    let _ = r;
}

#[tokio::test]
async fn after_a_rollback_the_lock_runs_out_and_the_message_comes_back() {
    let e = env().await;
    e.queue("q", |s| s.lock_duration_ms = 500);
    let mut c = e.client().await;
    let (r, d) = one_locked(&e, &mut c, "q").await;
    let (coord, txn) = controller(&mut c).await;
    settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await;
    c.discharge(coord, &txn, true).await.unwrap();
    c.flow(r, 5, false).await;
    let again = c.recv_for(r, WAIT).await.expect("redelivered when the lock ran out");
    let m = again.message();
    assert_eq!(body(&m), "work");
    assert_eq!(m.header.unwrap().delivery_count, Some(1));
}

#[tokio::test]
async fn abandon_defer_and_dead_letter_in_a_transaction_are_applied_at_commit() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    for i in 0..3 {
        e.inject("q", msg_id(&format!("m{i}"), &format!("id{i}")));
    }
    let r = receiver(&mut c, "q", 5).await;
    let d0 = c.recv(r).await;
    let d1 = c.recv(r).await;
    let d2 = c.recv(r).await;
    let (coord, txn) = controller(&mut c).await;
    settle(&mut c, &d0, t(&txn, DeliveryState::modified(true, false))).await; // abandon
    settle(&mut c, &d1, t(&txn, DeliveryState::modified(false, true))).await; // defer
    let dl = azemu_amqp::Error::new("com.microsoft:dead-letter", "")
        .with_info("DeadLetterReason", Value::str("because"))
        .with_info("DeadLetterErrorDescription", Value::str("it is bad"));
    settle(&mut c, &d2, t(&txn, DeliveryState::rejected(dl))).await;
    assert_eq!((e.core("q", |c| c.main.len()), ready(&e, "q"), e.core("q", |c| c.counts().dead_letter)), (3, 0, 0), "nothing yet");
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!((e.core("q", |c| c.main.len()), e.core("q", |c| c.counts().dead_letter)), (2, 1));
    assert_eq!(e.core("q", |c| c.main.get(2).map(|m| (m.deferred, m.delivery_count))), Some((true, 0)));
    // the dead-letter message carries the reason
    let dlr = receiver_rd(&mut c, "q/$DeadLetterQueue", 2).await;
    let m = c.recv(dlr).await.message();
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("because")));
    // the abandoned one counts the delivery
    c.flow(r, 5, false).await;
    let again = c.recv_for(r, WAIT).await.expect("the abandoned message is back");
    assert_eq!(again.message().header.unwrap().delivery_count, Some(1));
}

#[tokio::test]
async fn receive_process_forward_completes_and_sends_in_one_transaction() {
    let e = env().await;
    e.queue("in", |s| s.lock_duration_ms = 5000);
    e.queue("out", |_| {});
    let mut c = e.client().await;
    let (_r, d) = one_locked(&e, &mut c, "in").await;
    let (coord, txn) = controller(&mut c).await;
    let out_sender = sender(&mut c, "out").await;
    settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await;
    assert_eq!(c.send_message_in_txn(out_sender, &msg("result"), &txn).await, accepted_in(&txn));
    assert_eq!((e.core("in", |c| c.main.len()), active(&e, "out")), (1, 0));
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!((e.core("in", |c| c.main.len()), active(&e, "out")), (0, 1));

    // the same, rolled back: the input stays, nothing is forwarded
    e.inject("in", msg_id("work 2", "w2"));
    c.flow(_r, 5, false).await;
    let d2 = c.recv(_r).await;
    let txn2 = c.declare(coord).await.unwrap();
    settle(&mut c, &d2, t(&txn2, DeliveryState::Accepted)).await;
    c.send_message_in_txn(out_sender, &msg("result 2"), &txn2).await;
    c.discharge(coord, &txn2, true).await.unwrap();
    assert_eq!((e.core("in", |c| c.main.len()), active(&e, "out")), (1, 1));
}

#[tokio::test]
async fn a_commit_that_cannot_complete_one_operation_applies_none() {
    // the lock of the completed message ran out before the commit: the send in the same transaction must not happen
    let e = env().await;
    e.queue("in", |s| s.lock_duration_ms = 400);
    e.queue("out", |_| {});
    let mut c = e.client().await;
    let (_r, d) = one_locked(&e, &mut c, "in").await;
    let (coord, txn) = controller(&mut c).await;
    let out_sender = sender(&mut c, "out").await;
    settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await;
    c.send_message_in_txn(out_sender, &msg("result"), &txn).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let err = c.discharge(coord, &txn, false).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:rollback");
    assert_eq!(active(&e, "out"), 0);
    // the message was unlocked by its lock running out and is delivered again
    assert_eq!(e.core("in", |c| c.main.len()), 1);
}

#[tokio::test]
async fn settling_one_message_twice_in_a_transaction_fails_the_commit() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (_r, d) = one_locked(&e, &mut c, "q").await;
    let (coord, txn) = controller(&mut c).await;
    settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await;
    // the same lock again, by its token over $management
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let reply = mg
        .call_in(
            &mut c,
            "com.microsoft:update-disposition",
            vec![("disposition-status", Value::str("abandoned")), ("lock-tokens", Value::Array(vec![uuid_value(&d.tag)]))],
            Some(&txn),
        )
        .await;
    assert_eq!(reply.status, 200);
    let err = c.discharge(coord, &txn, false).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:rollback");
    assert_eq!(e.core("q", |c| c.main.len()), 1, "nothing applied");
}

#[tokio::test]
async fn a_transactional_disposition_in_an_unknown_transaction_is_rejected() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (_r, d) = one_locked(&e, &mut c, "q").await;
    let echo = settle(&mut c, &d, t(b"nope", DeliveryState::Accepted)).await;
    match echo {
        Some(DeliveryState::Transactional { outcome: Some(o), .. }) => match *o {
            DeliveryState::Rejected { error: Some(err) } => assert_eq!(err.condition, "amqp:transaction:unknown-id"),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    assert_eq!(e.core("q", |c| c.main.len()), 1);
}

// ---- $management in a transaction ------------------------------------------------------------------

#[tokio::test]
async fn schedule_message_in_a_transaction_answers_sequence_numbers_and_commits_later() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let m1 = scheduled_msg("s1", 60_000);
    let m2 = scheduled_msg("s2", 60_000);
    let reply = mg
        .call_in(
            &mut c,
            "com.microsoft:schedule-message",
            vec![("messages", Value::List(vec![sched_request(&m1, "s1"), sched_request(&m2, "s2")]))],
            Some(&txn),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.description);
    let seqs: Vec<i64> = reply.body.map_get("sequence-numbers").unwrap().as_list().unwrap().iter().filter_map(|v| v.as_i64()).collect();
    assert_eq!(seqs.len(), 2);
    assert_eq!(e.core("q", |c| c.counts().scheduled), 0);
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("q", |c| c.counts().scheduled), 2);
    // the numbers that were answered are the ones the messages have
    let stored: Vec<u64> = e.core("q", |c| c.main.msgs.keys().copied().collect());
    assert_eq!(stored, seqs.iter().map(|s| *s as u64).collect::<Vec<_>>());
    // a rolled back schedule leaves nothing
    let txn2 = c.declare(coord).await.unwrap();
    let reply = mg
        .call_in(
            &mut c,
            "com.microsoft:schedule-message",
            vec![("messages", Value::List(vec![sched_request(&scheduled_msg("s3", 60_000), "s3")]))],
            Some(&txn2),
        )
        .await;
    assert_eq!(reply.status, 200);
    c.discharge(coord, &txn2, true).await.unwrap();
    assert_eq!(e.core("q", |c| c.counts().scheduled), 2);
}

#[tokio::test]
async fn cancel_scheduled_in_a_transaction_is_applied_at_commit() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let reply = mg
        .call(
            &mut c,
            "com.microsoft:schedule-message",
            vec![("messages", Value::List(vec![sched_request(&scheduled_msg("s1", 60_000), "s1")]))],
        )
        .await;
    let seq = reply.body.map_get("sequence-numbers").unwrap().as_list().unwrap()[0].clone();
    let (coord, txn) = controller(&mut c).await;
    let reply = mg
        .call_in(&mut c, "com.microsoft:cancel-scheduled-message", vec![("sequence-numbers", Value::Array(vec![seq.clone()]))], Some(&txn))
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(e.core("q", |c| c.counts().scheduled), 1);
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("q", |c| c.counts().scheduled), 0);
}

#[tokio::test]
async fn update_disposition_in_a_transaction_is_applied_at_commit() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (_r, d) = one_locked(&e, &mut c, "q").await;
    let (coord, txn) = controller(&mut c).await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let token = uuid_value(&d.tag);
    let reply = mg
        .call_in(
            &mut c,
            "com.microsoft:update-disposition",
            vec![("disposition-status", Value::str("completed")), ("lock-tokens", Value::Array(vec![token.clone()]))],
            Some(&txn),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.description);
    assert_eq!(e.core("q", |c| c.main.len()), 1);
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("q", |c| c.main.len()), 0);
}

#[tokio::test]
async fn management_requests_in_an_unknown_transaction_fail() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (_r, d) = one_locked(&e, &mut c, "q").await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let reply = mg
        .call_in(
            &mut c,
            "com.microsoft:update-disposition",
            vec![("disposition-status", Value::str("completed")), ("lock-tokens", Value::Array(vec![uuid_value(&d.tag)]))],
            Some(b"unknown"),
        )
        .await;
    assert_eq!(reply.condition.as_deref(), Some("amqp:transaction:unknown-id"));
    assert_eq!(e.core("q", |c| c.main.len()), 1);
}

#[tokio::test]
async fn set_session_state_in_a_transaction_is_applied_at_commit() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    e.inject("sq", msg_session("x", "S1"));
    let mut c = e.client().await;
    let mut src = Source::with_address("sq");
    src.filter = Some(Value::Map(vec![(Value::sym("com.microsoft:session-filter"), Value::str("S1"))]));
    let _r = c.attach_receiver_source(src, SndSettleMode::Unsettled, RcvSettleMode::Second, 1).await.unwrap();
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
    assert_eq!(e.core("sq", |c| c.session_state("S1")), None);
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("sq", |c| c.session_state("S1")), Some(b"state".to_vec()));
}

// ---- time out, limits, the controller going away -----------------------------------------------------

#[tokio::test]
async fn a_transaction_that_times_out_is_rolled_back() {
    let e = env().await;
    e.shared.set_txn_timeout(Duration::from_millis(400));
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    assert_eq!(c.send_message_in_txn(s, &msg("slow"), &txn).await, accepted_in(&txn));
    tokio::time::sleep(Duration::from_millis(600)).await;
    // more work in it fails
    match c.send_message_in_txn(s, &msg("too late"), &txn).await {
        Some(DeliveryState::Transactional { outcome: Some(o), .. }) => match *o {
            DeliveryState::Rejected { error: Some(err) } => assert_eq!(err.condition, "amqp:transaction:timeout"),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    // the commit fails with the time out and applies nothing
    let err = c.discharge(coord, &txn, false).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:timeout");
    assert_eq!(active(&e, "q"), 0);
    // told once: afterwards the id is unknown
    let err = c.discharge(coord, &txn, false).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:unknown-id");
    // the connection and the link are fine, and a new transaction has a new time
    e.shared.set_txn_timeout(Duration::from_secs(60));
    let txn2 = c.declare(coord).await.unwrap();
    c.send_message_in_txn(s, &msg("fresh"), &txn2).await;
    c.discharge(coord, &txn2, false).await.unwrap();
    assert_eq!(active(&e, "q"), 1);
}

#[tokio::test]
async fn a_rollback_after_the_time_out_is_answered_with_the_time_out() {
    let e = env().await;
    e.shared.set_txn_timeout(Duration::from_millis(200));
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let err = c.discharge(coord, &txn, true).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:timeout");
}

#[tokio::test]
async fn a_message_completed_in_a_transaction_that_timed_out_stays_locked_until_its_lock_runs_out() {
    let e = env().await;
    e.shared.set_txn_timeout(Duration::from_millis(300));
    e.queue("q", |s| s.lock_duration_ms = 1500);
    let mut c = e.client().await;
    let (_r, d) = one_locked(&e, &mut c, "q").await;
    let (coord, txn) = controller(&mut c).await;
    settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(c.discharge(coord, &txn, false).await.is_err());
    assert_eq!((e.core("q", |c| c.main.len()), ready(&e, "q")), (1, 0), "still locked");
}

#[tokio::test]
async fn at_most_100_operations_in_one_transaction() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    for i in 0..100 {
        let out = c.send_message_in_txn(s, &msg(&format!("m{i}")), &txn).await;
        assert_eq!(out, accepted_in(&txn), "send {i}");
    }
    match c.send_message_in_txn(s, &msg("one too many"), &txn).await {
        Some(DeliveryState::Transactional { outcome: Some(o), .. }) => match *o {
            DeliveryState::Rejected { error: Some(err) } => assert_eq!(err.condition, "amqp:resource-limit-exceeded"),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    // the 100 are still good
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(active(&e, "q"), 100);
}

#[tokio::test]
async fn a_batch_over_the_limit_is_refused_as_a_whole() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    for i in 0..99 {
        c.send_message_in_txn(s, &msg(&format!("m{i}")), &txn).await;
    }
    let payload = Message::encode_batch(&[msg("a").encode(), msg("b").encode()]);
    let id = c
        .send_payload_with_state(
            s,
            &payload,
            BATCH_MESSAGE_FORMAT,
            false,
            Some(DeliveryState::Transactional { txn_id: txn.clone(), outcome: None }),
        )
        .await;
    match c.wait_disposition(id).await {
        Some(DeliveryState::Transactional { outcome: Some(o), .. }) => assert!(matches!(*o, DeliveryState::Rejected { .. })),
        other => panic!("{other:?}"),
    }
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(active(&e, "q"), 99);
}

#[tokio::test]
async fn closing_the_coordinator_link_rolls_back_its_transactions() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    c.send_message_in_txn(s, &msg("lost"), &txn).await;
    c.detach(coord).await;
    // a new controller cannot finish the old transaction, and nothing was applied
    let coord2 = c.attach_coordinator().await.unwrap();
    let err = c.discharge(coord2, &txn, false).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:unknown-id");
    let out = c.send_message_in_txn(s, &msg("also lost"), &txn).await;
    assert!(
        matches!(out, Some(DeliveryState::Transactional { outcome: Some(ref o), .. }) if matches!(**o, DeliveryState::Rejected { .. }))
    );
    assert_eq!(active(&e, "q"), 0);
    // the connection goes on
    send(&mut c, s, &msg("plain")).await;
    assert_eq!(active(&e, "q"), 1);
}

#[tokio::test]
async fn dropping_the_connection_in_the_middle_of_a_transaction_applies_nothing() {
    let e = env().await;
    e.queue("in", |s| s.lock_duration_ms = 600);
    e.queue("out", |_| {});
    let mut c = e.client().await;
    let (_r, d) = one_locked(&e, &mut c, "in").await;
    let (_coord, txn) = controller(&mut c).await;
    let out_sender = sender(&mut c, "out").await;
    settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await;
    c.send_message_in_txn(out_sender, &msg("never"), &txn).await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(active(&e, "out"), 0);
    assert_eq!(e.core("in", |c| c.main.len()), 1);
    // the input message is delivered again when its lock ran out
    let mut c2 = e.client().await;
    let r2 = receiver(&mut c2, "in", 2).await;
    let again = c2.recv_for(r2, WAIT).await.expect("redelivered");
    assert_eq!(body(&again.message()), "work");
}

#[tokio::test]
async fn a_waiting_receiver_is_woken_by_the_commit_and_not_before() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut rc = e.client().await;
    let r = receiver_rd(&mut rc, "q", 1).await;
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    c.send_message_in_txn(s, &msg("wake"), &txn).await;
    no_delivery(&mut rc, r, 200).await;
    c.discharge(coord, &txn, false).await.unwrap();
    let got = rc.recv_for(r, WAIT).await.expect("woken by the commit");
    assert_eq!(body(&got.message()), "wake");
}

#[tokio::test]
async fn an_empty_transaction_commits() {
    let e = env().await;
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    c.discharge(coord, &txn, false).await.unwrap();
}
