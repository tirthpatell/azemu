//! A receiver that is already attached (with credit, nothing to deliver) must be woken by every
//! way a message can appear on or return to its sub-queue. Each test attaches the receiver
//! first, then makes the broker do the move itself, and expects the delivery while the same link
//! is still waiting. The Java SDK: the broker's own dead-lettering at the delivery limit did
//! not wake a waiting dead-letter receiver.

mod common;
use std::time::Duration;

use azemu_amqp::message::{Header, Message};
use azemu_amqp::performatives::{RcvSettleMode, SndSettleMode};
use azemu_amqp::terminus::{DeliveryState, Source};
use azemu_amqp::types::Value;
use azemu_servicebus::core::{Core, Incoming};
use azemu_servicebus::store::{new_token, Lock, Msg, MsgState, Owner};
use common::*;

const WAIT: Duration = Duration::from_secs(3);

fn with_ttl(m: Message, ms: u32) -> Message {
    Message { header: Some(Header { ttl: Some(ms), ..Default::default() }), ..m }
}

fn scheduled_in(mut m: Message, ms: i64) -> Message {
    m.message_annotations =
        Some(vec![(Value::sym("x-opt-scheduled-enqueue-time"), Value::Timestamp(azemu_servicebus::state::now_ms() + ms))]);
    m
}

#[tokio::test]
async fn abandon_at_max_delivery_count_wakes_a_waiting_dead_letter_receiver() {
    let e = env().await;
    e.queue("q", |s| s.max_delivery_count = 2);
    e.inject("q", msg("poison"));
    let mut c = e.client().await;
    // the dead-letter receiver attaches first and waits
    let dl = receiver(&mut c, "q/$DeadLetterQueue", 1).await;
    no_delivery(&mut c, dl, 100).await;
    let r = receiver(&mut c, "q", 5).await;
    for _ in 0..2 {
        let d = c.recv(r).await;
        settle(&mut c, &d, DeliveryState::modified(true, false)).await;
    }
    let got = c.recv_for(dl, WAIT).await.expect("the waiting dead-letter receiver gets the message");
    let m = got.message();
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("MaxDeliveryCountExceeded")));
    assert_eq!(body(&m), "poison");
}

#[tokio::test]
async fn lock_expiry_at_max_delivery_count_wakes_a_waiting_dead_letter_receiver() {
    let e = env().await;
    e.queue("q", |s| {
        s.max_delivery_count = 1;
        s.lock_duration_ms = 300;
    });
    e.inject("q", msg("lost"));
    let mut c = e.client().await;
    let dl = receiver(&mut c, "q/$DeadLetterQueue", 1).await;
    let r = receiver(&mut c, "q", 1).await;
    let _locked = c.recv(r).await;
    // nobody settles it: the lock runs out and the limit is reached
    let got = c.recv_for(dl, WAIT).await.expect("woken by the lock expiry");
    assert_eq!(app(&got.message(), "DeadLetterReason"), Some(Value::str("MaxDeliveryCountExceeded")));
}

#[tokio::test]
async fn ttl_expiry_wakes_a_waiting_dead_letter_receiver() {
    let e = env().await;
    e.queue("q", |s| s.dead_letter_on_expiry = true);
    let mut c = e.client().await;
    let dl = receiver(&mut c, "q/$DeadLetterQueue", 1).await;
    e.inject("q", with_ttl(msg("short lived"), 300));
    let got = c.recv_for(dl, WAIT).await.expect("woken by the expiry");
    assert_eq!(app(&got.message(), "DeadLetterReason"), Some(Value::str("TTLExpiredException")));
}

#[tokio::test]
async fn explicit_dead_letter_wakes_a_waiting_dead_letter_receiver() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let dl = receiver(&mut c, "q/$DeadLetterQueue", 1).await;
    let r = receiver(&mut c, "q", 1).await;
    let d = c.recv(r).await;
    let st = DeliveryState::rejected(azemu_amqp::error::Error::new("com.microsoft:dead-letter", "no"));
    settle(&mut c, &d, st).await;
    assert!(c.recv_for(dl, WAIT).await.is_some());
}

#[tokio::test]
async fn max_delivery_count_in_a_subscription_wakes_the_subscription_dead_letter_receiver() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "s", vec![azemu_servicebus::rules::Rule::default_rule(0)], |s| s.max_delivery_count = 1);
    let mut c = e.client().await;
    let dl = receiver(&mut c, "t/Subscriptions/s/$DeadLetterQueue", 1).await;
    let r = receiver(&mut c, "t/Subscriptions/s", 1).await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &msg("fan")).await;
    let d = c.recv(r).await;
    settle(&mut c, &d, DeliveryState::modified(true, false)).await;
    let got = c.recv_for(dl, WAIT).await.expect("subscription dead-letter receiver is woken");
    assert_eq!(body(&got.message()), "fan");
}

#[tokio::test]
async fn forwarded_dead_letters_wake_a_receiver_of_the_target_queue() {
    let e = env().await;
    e.queue("sink", |_| {});
    e.queue("q", |s| {
        s.max_delivery_count = 1;
        s.forward_dead_lettered_to = Some("sink".into());
    });
    e.inject("q", msg("moved"));
    let mut c = e.client().await;
    let waiting = receiver(&mut c, "sink", 1).await;
    no_delivery(&mut c, waiting, 100).await;
    let r = receiver(&mut c, "q", 1).await;
    let d = c.recv(r).await;
    settle(&mut c, &d, DeliveryState::modified(true, false)).await;
    let got = c.recv_for(waiting, WAIT).await.expect("the forward target's receiver is woken");
    assert_eq!(app(&got.message(), "DeadLetterReason"), Some(Value::str("MaxDeliveryCountExceeded")));
}

#[tokio::test]
async fn a_scheduled_message_that_becomes_due_wakes_a_waiting_receiver() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 1).await;
    let s = sender(&mut c, "q").await;
    send(&mut c, s, &scheduled_in(msg("later"), 400)).await;
    no_delivery(&mut c, r, 100).await;
    let got = c.recv_for(r, WAIT).await.expect("woken when due");
    assert_eq!(body(&got.message()), "later");
}

#[tokio::test]
async fn a_scheduled_topic_message_that_becomes_due_wakes_a_waiting_subscription_receiver() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "s", vec![azemu_servicebus::rules::Rule::default_rule(0)], |_| {});
    let mut c = e.client().await;
    let r = receiver(&mut c, "t/Subscriptions/s", 1).await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &scheduled_in(msg("later"), 400)).await;
    no_delivery(&mut c, r, 100).await;
    let got = c.recv_for(r, WAIT).await.expect("woken when the fan-out happens");
    assert_eq!(body(&got.message()), "later");
}

#[tokio::test]
async fn topic_fan_out_wakes_every_waiting_subscription_receiver() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "a", vec![azemu_servicebus::rules::Rule::default_rule(0)], |_| {});
    e.sub("t", "b", vec![azemu_servicebus::rules::Rule::default_rule(0)], |_| {});
    let mut c = e.client().await;
    let ra = receiver(&mut c, "t/Subscriptions/a", 1).await;
    let rb = receiver(&mut c, "t/Subscriptions/b", 1).await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &msg("both")).await;
    assert!(c.recv_for(ra, WAIT).await.is_some());
    assert!(c.recv_for(rb, WAIT).await.is_some());
}

#[tokio::test]
async fn a_message_returned_by_abandon_wakes_another_waiting_receiver() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c1 = e.client().await;
    let mut c2 = e.client().await;
    let r1 = receiver(&mut c1, "q", 1).await;
    let d = c1.recv(r1).await;
    let r2 = receiver(&mut c2, "q", 1).await;
    no_delivery(&mut c2, r2, 100).await;
    settle(&mut c1, &d, DeliveryState::modified(true, false)).await;
    let got = c2.recv_for(r2, WAIT).await.expect("the second receiver is woken");
    assert_eq!(got.message().header.unwrap().delivery_count, Some(1));
}

#[tokio::test]
async fn a_message_returned_by_lock_expiry_wakes_a_waiting_receiver() {
    let e = env().await;
    e.queue("q", |s| s.lock_duration_ms = 300);
    e.inject("q", msg("x"));
    let mut c1 = e.client().await;
    let mut c2 = e.client().await;
    let r1 = receiver(&mut c1, "q", 1).await;
    let _d = c1.recv(r1).await;
    let r2 = receiver(&mut c2, "q", 1).await;
    let got = c2.recv_for(r2, WAIT).await.expect("woken when the lock runs out");
    assert_eq!(got.message().header.unwrap().delivery_count, Some(1));
}

fn session_source(address: &str, sid: &str) -> Source {
    let mut s = Source::with_address(address);
    s.filter = Some(Value::Map(vec![(Value::sym("com.microsoft:session-filter"), Value::str(sid))]));
    s
}

#[tokio::test]
async fn a_message_for_a_session_wakes_the_receiver_bound_to_it() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    let r = c.attach_receiver_source(session_source("sq", "S1"), SndSettleMode::Unsettled, RcvSettleMode::Second, 1).await.unwrap();
    no_delivery(&mut c, r, 100).await;
    let mut c2 = e.client().await;
    let s = sender(&mut c2, "sq").await;
    send(&mut c2, s, &msg_session("one", "S1")).await;
    let got = c.recv_for(r, WAIT).await.expect("woken by a message of its session");
    assert_eq!(body(&got.message()), "one");
}

#[tokio::test]
async fn a_session_message_returned_by_abandon_wakes_the_session_receiver() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    e.inject("sq", msg_session("one", "S1"));
    let mut c = e.client().await;
    let r = c.attach_receiver_source(session_source("sq", "S1"), SndSettleMode::Unsettled, RcvSettleMode::Second, 1).await.unwrap();
    let d = c.recv(r).await;
    // credit 1 is used: give another and abandon, the message must come back on the waiting link
    c.flow(r, 1, false).await;
    settle(&mut c, &d, DeliveryState::modified(true, false)).await;
    let again = c.recv_for(r, WAIT).await.expect("abandoned session message comes back");
    assert_eq!(again.message().header.unwrap().delivery_count, Some(1));
}

#[tokio::test]
async fn a_waiter_on_the_transfer_dead_letter_queue_is_woken_by_its_own_sub_queue() {
    // nothing puts messages in `$Transfer/$DeadLetterQueue`; the engine still wakes by sub-queue,
    // so a message whose lock runs out there wakes that queue's receiver and not the other one
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let tdl = receiver(&mut c, "q/$Transfer/$DeadLetterQueue", 1).await;
    let dl = receiver(&mut c, "q/$DeadLetterQueue", 1).await;
    no_delivery(&mut c, tdl, 100).await;
    let now = azemu_servicebus::state::now_ms();
    e.core("q", |core| {
        let m = Message::data(b"t".to_vec());
        let seq = core.tdlq.alloc_seq();
        core.tdlq.insert(Msg {
            seq,
            enq_seq: seq,
            state: MsgState::Locked,
            message: m.into(),
            enqueued_ms: now,
            scheduled_ms: None,
            ttl_ms: None,
            expires_ms: None,
            delivery_count: 0,
            lock: Some(Lock { token: new_token(), until_ms: now + 200, owner: Owner::default(), via_session: false }),
            session_id: None,
            size: 10,
            dead_letter_source: None,
            deferred: false,
        });
    });
    let got = c.recv_for(tdl, WAIT).await.expect("transfer dead-letter receiver is woken");
    assert_eq!(body(&got.message()), "t");
    no_delivery(&mut c, dl, 100).await;
}

#[tokio::test]
async fn forwards_found_by_an_operation_that_cannot_forward_are_not_lost() {
    // a message whose time to live ran out is noticed by whatever operation touches the entity
    // first (here a peek); its dead-letter forward must still reach the target
    let e = env().await;
    e.queue("sink", |_| {});
    e.queue("q", |s| {
        s.dead_letter_on_expiry = true;
        s.forward_dead_lettered_to = Some("sink".into());
    });
    let mut core = Core::new("q", {
        let mut s = azemu_servicebus::model::Settings::queue_default(0);
        s.dead_letter_on_expiry = true;
        s.forward_dead_lettered_to = Some("sink".into());
        s
    });
    let m = with_ttl(msg("x"), 1000);
    let size = m.encode().len();
    core.enqueue(Incoming::new(m, size), 0).unwrap();
    // an operation at t=5000 applies the expiry; the forward cannot be returned to its caller
    assert!(core.peek(azemu_servicebus::model::SubQueue::Main, 0, 10, None, 5000).is_empty());
    assert_eq!(core.next_due(), Some(i64::MIN), "the forward is pending, the ticker must run");
    let fwd = core.tick(5000);
    assert_eq!(fwd.len(), 1);
    assert_eq!(fwd[0].target, "sink");
    assert!(core.next_due().is_none());

    // and through the server: the ticker delivers it to the sink queue
    let mut c = e.client().await;
    let sink = receiver(&mut c, "sink", 1).await;
    e.inject("q", with_ttl(msg("y"), 200));
    let _ = e.core("q", |core| core.peek(azemu_servicebus::model::SubQueue::Main, 0, 10, None, azemu_servicebus::state::now_ms() + 5000));
    let got = c.recv_for(sink, WAIT).await.expect("the forward reaches the sink");
    assert_eq!(body(&got.message()), "y");
}
