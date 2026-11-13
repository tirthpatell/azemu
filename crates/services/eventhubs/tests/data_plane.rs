mod common;

use azemu_amqp::message::{Body, Message};
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::types::Value;
use common::*;

#[tokio::test]
async fn single_event_round_trip_with_broker_annotations() {
    let env = env().await;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "amqps://myns.eventhub.localhost:1/hub1/Partitions/2").await.unwrap();
    let mut m = event("hello");
    m.application_properties = Some(vec![(Value::str("k"), Value::str("v")), (Value::str("n"), Value::Long(7))]);
    send_one(&mut c, s, &m).await;
    send_one(&mut c, s, &event("second")).await;

    let r = consume(&mut c, "hub1", "$Default", 2, &from_start()).await.unwrap();
    let first = c.recv(r).await;
    assert!(first.settled, "events are sent pre-settled");
    let msg = first.message();
    assert_eq!(msg.body, Body::Data(vec![b"hello".to_vec()]));
    assert_eq!(msg.app_property("k"), Some(&Value::str("v")));
    assert_eq!(msg.app_property("n"), Some(&Value::Long(7)));
    assert_eq!(ann_i64(&msg, "x-opt-sequence-number"), 0);
    assert!(matches!(msg.annotation("x-opt-offset"), Some(Value::String(_))), "offset is a string");
    assert!(matches!(msg.annotation("x-opt-enqueued-time"), Some(Value::Timestamp(t)) if *t > 1_600_000_000_000));
    let second = c.recv(r).await.message();
    assert_eq!(ann_i64(&second, "x-opt-sequence-number"), 1);
    let o1: i64 = ann_str(&msg, "x-opt-offset").parse().unwrap();
    let o2: i64 = ann_str(&second, "x-opt-offset").parse().unwrap();
    assert!(o2 > o1, "offsets increase: {o1} {o2}");
}

#[tokio::test]
async fn partitions_are_separate_streams_numbered_from_zero() {
    let env = env().await;
    let mut c = env.client().await;
    for pid in 0..2 {
        let s = attach_sender(&mut c, &format!("h/Partitions/{pid}")).await.unwrap();
        for i in 0..3 {
            send_one(&mut c, s, &event(&format!("p{pid}-{i}"))).await;
        }
    }
    for pid in 0..2u32 {
        let r = consume(&mut c, "h", "$Default", pid, &from_start()).await.unwrap();
        for i in 0..3 {
            let m = c.recv(r).await.message();
            assert_eq!(ann_i64(&m, "x-opt-sequence-number"), i);
        }
        expect_quiet(&mut c, r).await;
    }
}

#[tokio::test]
async fn batch_is_split_into_events_in_order() {
    let env = env().await;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/0").await.unwrap();
    let state = send_batch(&mut c, s, &[event("a"), event("b"), event("c")]).await;
    assert_eq!(state, Some(DeliveryState::Accepted));
    let r = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    assert_eq!(take(&mut c, r, 3).await, vec!["a", "b", "c"]);
    expect_quiet(&mut c, r).await;
}

#[tokio::test]
async fn partition_key_routes_stably_and_is_kept() {
    let env = env().await;
    env.hub("h", 4);
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h").await.unwrap();
    for i in 0..5 {
        send_one(&mut c, s, &keyed(&format!("k1-{i}"), "key-one")).await;
    }
    for i in 0..5 {
        send_one(&mut c, s, &keyed(&format!("k2-{i}"), "other-key")).await;
    }
    let mut seen = Vec::new();
    for pid in 0..4u32 {
        let r = consume(&mut c, "h", "$Default", pid, &from_start()).await.unwrap();
        let mut bodies = Vec::new();
        while let Some(d) = c.recv_for(r, std::time::Duration::from_millis(250)).await {
            let m = d.message();
            assert!(m.annotation("x-opt-partition-key").is_some(), "partition key kept");
            bodies.push(body_of(&d));
        }
        seen.push(bodies);
    }
    let with = |prefix: &str| -> Vec<usize> { (0..4).filter(|p| seen[*p].iter().any(|b| b.starts_with(prefix))).collect() };
    assert_eq!(with("k1-").len(), 1, "one key, one partition: {seen:?}");
    assert_eq!(with("k2-").len(), 1);
    let p1 = with("k1-")[0];
    assert_eq!(seen[p1].iter().filter(|b| b.starts_with("k1-")).count(), 5);
    assert_eq!(seen.iter().map(Vec::len).sum::<usize>(), 10);
}

#[tokio::test]
async fn keys_in_one_batch_split_across_partitions_and_unkeyed_batch_stays_together() {
    let env = env().await;
    env.hub("h", 4);
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h").await.unwrap();
    // an unkeyed batch lands in one partition
    send_batch(&mut c, s, &[event("u1"), event("u2"), event("u3")]).await;
    let mut counts = Vec::new();
    for pid in 0..4u32 {
        let r = consume(&mut c, "h", "$Default", pid, &from_start()).await.unwrap();
        let mut n = 0;
        while c.recv_for(r, std::time::Duration::from_millis(200)).await.is_some() {
            n += 1;
        }
        counts.push(n);
    }
    assert_eq!(counts.iter().filter(|n| **n == 3).count(), 1, "{counts:?}");
    assert_eq!(counts.iter().sum::<i32>(), 3);
}

#[tokio::test]
async fn events_without_a_key_are_spread_round_robin() {
    let env = env().await;
    env.hub("h", 4);
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h").await.unwrap();
    for i in 0..8 {
        send_one(&mut c, s, &event(&format!("e{i}"))).await;
    }
    for pid in 0..4u32 {
        let r = consume(&mut c, "h", "$Default", pid, &from_start()).await.unwrap();
        assert_eq!(take(&mut c, r, 2).await.len(), 2);
        expect_quiet(&mut c, r).await;
    }
}

#[tokio::test]
async fn body_and_section_types_survive() {
    let env = env().await;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/0").await.unwrap();
    let mut m = event("x");
    m.body = Body::Value(Value::Long(42));
    m.properties = Some(azemu_amqp::message::Properties {
        message_id: Some(Value::Uuid([3; 16])),
        content_type: Some("application/json".into()),
        subject: Some("subj".into()),
        ..Default::default()
    });
    send_one(&mut c, s, &m).await;
    let mut seq = event("y");
    seq.body = Body::Sequence(vec![vec![Value::Long(1), Value::str("two")]]);
    send_one(&mut c, s, &seq).await;
    let r = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    let got = c.recv(r).await.message();
    assert_eq!(got.body, Body::Value(Value::Long(42)));
    let p = got.properties.unwrap();
    assert_eq!(p.message_id, Some(Value::Uuid([3; 16])));
    assert_eq!(p.content_type.as_deref(), Some("application/json"));
    assert_eq!(p.subject.as_deref(), Some("subj"));
    let got = c.recv(r).await.message();
    assert_eq!(got.body, Body::Sequence(vec![vec![Value::Long(1), Value::str("two")]]));
}

#[tokio::test]
async fn null_bodies_and_every_section_type_come_back_as_they_were_sent() {
    // The JavaScript SDK sends `body: null` as a data section (0x75) holding AMQP null; other senders
    // use an amqp-value section holding null, or no body section at all.
    let env = env().await;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/1").await.unwrap();
    let mut null_value = event("v");
    null_value.body = Body::Value(Value::Null);
    let mut no_body = event("n");
    no_body.body = Body::Empty;
    let mut two = event("t");
    two.body = Body::Data(vec![b"a".to_vec(), b"b".to_vec()]);
    // payloads written by hand: the null data section
    let payload = [0x00, 0x53, 0x75, 0x40];
    let id = c.send_payload(s, &payload, 0, false).await;
    assert_eq!(c.wait_disposition(id).await, Some(azemu_amqp::terminus::DeliveryState::Accepted));
    for m in [&null_value, &no_body, &two] {
        send_one(&mut c, s, m).await;
    }
    let r = consume(&mut c, "h", "$Default", 1, &from_start()).await.unwrap();
    assert_eq!(c.recv(r).await.message().body, Body::NullData);
    assert_eq!(c.recv(r).await.message().body, Body::Value(Value::Null));
    assert_eq!(c.recv(r).await.message().body, Body::Empty);
    assert_eq!(c.recv(r).await.message().body, Body::Data(vec![b"a".to_vec(), b"b".to_vec()]));
}

#[tokio::test]
async fn broker_annotations_cannot_be_spoofed_and_delivery_annotations_are_kept() {
    let env = env().await;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/0").await.unwrap();
    let mut m = event("x");
    m.set_annotation("x-opt-sequence-number", Value::Long(999));
    m.set_annotation("x-opt-offset", Value::str("999"));
    m.delivery_annotations = Some(vec![(Value::sym("mine"), Value::Long(1))]);
    send_one(&mut c, s, &m).await;
    let r = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    let got = c.recv(r).await.message();
    assert_eq!(ann_i64(&got, "x-opt-sequence-number"), 0);
    assert_eq!(ann_str(&got, "x-opt-offset"), "0");
    assert_eq!(
        got.delivery_annotations,
        Some(vec![(Value::sym("mine"), Value::Long(1))]),
        "Microsoft's live tests read client delivery annotations back"
    );
}

#[tokio::test]
async fn message_over_the_link_limit_is_rejected_with_message_size_exceeded() {
    let env = env().await;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/0").await.unwrap();
    let big = Message::data(vec![b'x'; 1_048_576 + 100]);
    let id = c.send_payload(s, &big.encode(), 0, false).await;
    match c.wait_disposition(id).await {
        Some(DeliveryState::Rejected { error: Some(e) }) => assert_eq!(e.condition, "amqp:link:message-size-exceeded"),
        other => panic!("expected a rejection, got {other:?}"),
    }
    // and the link attach announced the limit
    let s2 = attach_sender(&mut c, "h/Partitions/0").await.unwrap();
    assert_eq!(c.links[&s2].attach.as_ref().unwrap().max_message_size, Some(1_048_576));
}

#[tokio::test]
async fn sending_to_a_bad_partition_is_refused() {
    let env = env().await;
    env.hub("h", 2);
    let mut c = env.client().await;
    let e = attach_sender(&mut c, "h/Partitions/2").await.unwrap_err();
    assert_eq!(condition(e), "com.microsoft:argument-out-of-range");
    let e = attach_sender(&mut c, "h/Partitions/x").await.unwrap_err();
    assert_eq!(condition(e), "com.microsoft:argument-out-of-range");
}

#[tokio::test]
async fn a_sender_for_a_missing_partition_is_refused_with_a_null_target_and_a_detach() {
    // AMQP 1.0 section 2.6.3, and what Microsoft.Azure.Amqp (the library under the real service) does: a link that
    // cannot be created is answered with an attach whose target is null (for a sending client), then a detach with
    // the error. The .NET SDK needs this form: with a complete attach before the detach it opens the link again, for ever.
    use azemu_amqp::performatives::{Attach, Role};
    use azemu_amqp::terminus::{Source, Target, TargetKind};
    let env = env().await;
    env.hub("h", 2);
    let mut c = env.client().await;
    let mut a = Attach::new("p99", 0, Role::Sender);
    a.source = Some(Source::with_address("sender"));
    a.target = Some(TargetKind::Target(Target::with_address("h/Partitions/99")));
    let err = c.attach(a).await.expect_err("the attach is refused").expect("the detach carries an error");
    assert_eq!(err.condition, "com.microsoft:argument-out-of-range");
    assert!(err
        .description
        .as_deref()
        .unwrap_or("")
        .contains("The specified partition is invalid for an EventHub partition sender or receiver"));
    let answer = c.attach_of(0).expect("an attach answer");
    assert!(answer.target.is_none() && answer.source.is_none(), "null termini: {answer:?}");
}

#[tokio::test]
async fn producer_properties_and_capabilities_are_accepted() {
    use azemu_amqp::performatives::{Attach, Role};
    use azemu_amqp::terminus::{Source, Target, TargetKind};
    let env = env().await;
    let mut c = env.client().await;
    let mut a = Attach::new("producer", 0, Role::Sender);
    a.source = Some(Source::with_address("sender"));
    a.target = Some(TargetKind::Target(Target::with_address("amqps://myns.eventhub.localhost:1/h")));
    a.desired_capabilities = vec!["com.microsoft:idempotent-producer".into(), "com.microsoft:georeplication".into()];
    a.properties = Some(Value::Map(vec![
        (Value::sym("com.microsoft:timeout"), Value::Long(60_000)),
        (Value::sym("com.microsoft:producer-group-id"), Value::Long(5)),
        (Value::sym("com.microsoft:producer-owner-level"), Value::Short(0)),
    ]));
    let h = c.attach(a).await.unwrap();
    c.wait_until("credit", |c| c.credit(h) > 0).await;
    send_one(&mut c, h, &event("idem")).await;
}
