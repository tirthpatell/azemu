mod common;
use std::time::Duration;

use azemu_amqp::error::Error;
use azemu_amqp::message::{Header, Message, Properties};
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::types::Value;
use common::*;

fn dead_letter(reason: &str, desc: &str) -> DeliveryState {
    DeliveryState::rejected(
        Error::new("com.microsoft:dead-letter", desc)
            .with_info("DeadLetterReason", Value::str(reason))
            .with_info("DeadLetterErrorDescription", Value::str(desc)),
    )
}

#[tokio::test]
async fn abandon_counts_a_delivery_and_returns_the_message() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 5).await;
    let (d, m) = recv_body(&mut c, r).await;
    assert_eq!(m.header.unwrap().delivery_count, Some(0));
    let echo = settle(&mut c, &d, DeliveryState::modified(true, false)).await;
    assert_eq!(echo, Some(DeliveryState::modified(true, false)));
    let (d2, m2) = recv_body(&mut c, r).await;
    assert_eq!(m2.header.unwrap().delivery_count, Some(1));
    assert_ne!(d.tag, d2.tag, "a new lock token per delivery");
    settle(&mut c, &d2, DeliveryState::Accepted).await;
}

#[tokio::test]
async fn released_does_not_count_a_delivery() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 1).await;
    let d = c.recv(r).await;
    settle(&mut c, &d, DeliveryState::Released).await;
    c.flow(r, 1, false).await;
    let (_, m) = recv_body(&mut c, r).await;
    assert_eq!(m.header.unwrap().delivery_count, Some(0));
}

#[tokio::test]
async fn abandon_with_property_modifications() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 2).await;
    let d = c.recv(r).await;
    let st = DeliveryState::Modified {
        delivery_failed: true,
        undeliverable_here: false,
        message_annotations: Some(Value::Map(vec![(Value::str("retry"), Value::Int(3))])),
    };
    settle(&mut c, &d, st).await;
    let (_, m) = recv_body(&mut c, r).await;
    assert_eq!(app(&m, "retry"), Some(Value::Int(3)));
}

#[tokio::test]
async fn max_delivery_count_dead_letters_with_the_documented_reason() {
    let e = env().await;
    e.queue("q", |s| s.max_delivery_count = 3);
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 5).await;
    for attempt in 0..3 {
        let (d, m) = recv_body(&mut c, r).await;
        assert_eq!(m.header.unwrap().delivery_count, Some(attempt));
        settle(&mut c, &d, DeliveryState::modified(true, false)).await;
    }
    no_delivery(&mut c, r, 200).await;
    assert_eq!(e.core("q", |c| (c.counts().active, c.counts().dead_letter)), (0, 1));
    let dl = receiver(&mut c, "q/$DeadLetterQueue", 5).await;
    let (d, m) = recv_body(&mut c, dl).await;
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("MaxDeliveryCountExceeded")));
    assert!(app(&m, "DeadLetterErrorDescription").unwrap().as_str().unwrap().contains("3 delivery attempts"));
    assert_eq!(body(&m), "x");
    assert_eq!(settle(&mut c, &d, DeliveryState::Accepted).await, Some(DeliveryState::Accepted));
    assert_eq!(e.core("q", |c| c.counts().dead_letter), 0);
}

#[tokio::test]
async fn explicit_dead_letter_with_reason_and_description() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 2).await;
    let d = c.recv(r).await;
    let echo = settle(&mut c, &d, dead_letter("bad", "it is bad")).await;
    assert_eq!(echo, Some(DeliveryState::Rejected { error: None }), "success is a rejected without an error");
    let dl = receiver_rd(&mut c, "q/$DeadLetterQueue", 2).await;
    let m = c.recv(dl).await.message();
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("bad")));
    assert_eq!(app(&m, "DeadLetterErrorDescription"), Some(Value::str("it is bad")));
}

#[tokio::test]
async fn defer_then_receive_by_sequence_number_and_settle_over_management() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("keep"));
    e.inject("q", msg("other"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 5).await;
    let (d, m) = recv_body(&mut c, r).await;
    let seq = ann(&m, "x-opt-sequence-number").unwrap();
    let echo = settle(&mut c, &d, DeliveryState::modified(true, true)).await;
    assert_eq!(echo, Some(DeliveryState::modified(true, true)));
    let (d2, _) = recv_body(&mut c, r).await;
    settle(&mut c, &d2, DeliveryState::Accepted).await;
    no_delivery(&mut c, r, 200).await;

    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let reply = mg
        .call(
            &mut c,
            "com.microsoft:receive-by-sequence-number",
            vec![("sequence-numbers", Value::Array(vec![seq.clone()])), ("receiver-settle-mode", Value::Uint(1))],
        )
        .await;
    assert_eq!(reply.status, 200);
    let msgs = reply.body.map_get("messages").unwrap().as_list().unwrap();
    let inner = Message::decode(msgs[0].map_get("message").unwrap().as_binary().unwrap()).unwrap();
    assert_eq!(body(&inner), "keep");
    assert_eq!(ann(&inner, "x-opt-message-state"), Some(Value::Int(1)));
    let token = msgs[0].map_get("lock-token").unwrap().clone();

    let reply = mg
        .call(
            &mut c,
            "com.microsoft:update-disposition",
            vec![("disposition-status", Value::str("completed")), ("lock-tokens", Value::Array(vec![token.clone()]))],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.description);
    assert_eq!(e.core("q", |c| c.counts().active), 0);
    // the same lock again is lost
    let reply = mg
        .call(
            &mut c,
            "com.microsoft:update-disposition",
            vec![("disposition-status", Value::str("completed")), ("lock-tokens", Value::Array(vec![token]))],
        )
        .await;
    assert_eq!((reply.status, reply.condition.as_deref()), (410, Some("com.microsoft:message-lock-lost")));
}

#[tokio::test]
async fn receive_deferred_of_an_unknown_sequence_number_is_message_not_found() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let reply = mg
        .call(
            &mut c,
            "com.microsoft:receive-by-sequence-number",
            vec![("sequence-numbers", Value::Array(vec![Value::Long(99)])), ("receiver-settle-mode", Value::Uint(1))],
        )
        .await;
    assert_eq!((reply.status, reply.condition.as_deref()), (404, Some("com.microsoft:message-not-found")));
}

#[tokio::test]
async fn lock_expiry_returns_the_message_with_the_count_incremented() {
    let e = env().await;
    e.queue("q", |s| s.lock_duration_ms = 300);
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 5).await;
    let (d, _) = recv_body(&mut c, r).await;
    // the lock lapses with nobody renewing it; the waiting link is woken without a timer per message
    let (d2, m2) = recv_body(&mut c, r).await;
    assert_eq!(m2.header.unwrap().delivery_count, Some(1));
    // settling the first (expired) lock fails the way Service Bus reports it
    match settle(&mut c, &d, DeliveryState::Accepted).await {
        Some(DeliveryState::Rejected { error: Some(err) }) => assert_eq!(err.condition, "com.microsoft:message-lock-lost"),
        other => panic!("{other:?}"),
    }
    settle(&mut c, &d2, DeliveryState::Accepted).await;
}

#[tokio::test]
async fn repeated_lock_expiry_ends_in_the_dead_letter_queue() {
    let e = env().await;
    e.queue("q", |s| {
        s.lock_duration_ms = 150;
        s.max_delivery_count = 2;
    });
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 5).await;
    c.recv(r).await;
    c.recv(r).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(e.core("q", |c| (c.counts().active, c.counts().dead_letter)), (0, 1));
}

#[tokio::test]
async fn renew_lock_extends_it() {
    let e = env().await;
    e.queue("q", |s| s.lock_duration_ms = 400);
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 5).await;
    let (d, m) = recv_body(&mut c, r).await;
    let first_until = match ann(&m, "x-opt-locked-until") {
        Some(Value::Timestamp(t)) => t,
        o => panic!("{o:?}"),
    };
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let reply = mg.call(&mut c, "com.microsoft:renew-lock", vec![("lock-tokens", Value::Array(vec![uuid_value(&d.tag)]))]).await;
        assert_eq!(reply.status, 200);
        let exp = reply.body.map_get("expirations").unwrap().as_list().unwrap()[0].clone();
        assert!(matches!(exp, Value::Timestamp(t) if t > first_until));
    }
    assert_eq!(
        settle(&mut c, &d, DeliveryState::Accepted).await,
        Some(DeliveryState::Accepted),
        "still locked after 750 ms of a 400 ms lock"
    );
}

#[tokio::test]
async fn renew_lock_of_an_unknown_token_is_message_lock_lost() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let reply = mg.call(&mut c, "com.microsoft:renew-lock", vec![("lock-tokens", Value::Array(vec![Value::Uuid([1; 16])]))]).await;
    assert_eq!((reply.status, reply.condition.as_deref()), (410, Some("com.microsoft:message-lock-lost")));
}

#[tokio::test]
async fn expired_message_is_dropped_or_dead_lettered_per_the_entity() {
    let e = env().await;
    e.queue("drop", |s| s.default_ttl_ms = Some(200));
    e.queue("dlq", |s| {
        s.default_ttl_ms = Some(200);
        s.dead_letter_on_expiry = true;
    });
    e.inject("drop", msg("x"));
    e.inject("dlq", msg("y"));
    // a per-message ttl shorter than the entity's wins, a longer one is capped
    e.inject("dlq", Message { header: Some(Header { ttl: Some(60_000), ..Default::default() }), ..msg("capped") });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(e.core("drop", |c| (c.counts().active, c.counts().dead_letter)), (0, 0));
    assert_eq!(e.core("dlq", |c| (c.counts().active, c.counts().dead_letter)), (0, 2));
    let mut c = e.client().await;
    let r = receiver_rd(&mut c, "dlq/$DeadLetterQueue", 5).await;
    let m = c.recv(r).await.message();
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("TTLExpiredException")));
}

#[tokio::test]
async fn a_locked_message_does_not_expire_until_released() {
    let e = env().await;
    e.queue("q", |s| {
        s.default_ttl_ms = Some(150);
        s.dead_letter_on_expiry = true;
    });
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 1).await;
    let d = c.recv(r).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(e.core("q", |c| c.counts().dead_letter), 0);
    assert_eq!(settle(&mut c, &d, DeliveryState::Accepted).await, Some(DeliveryState::Accepted));
}

#[tokio::test]
async fn peek_does_not_lock_and_starts_at_the_sequence_number() {
    let e = env().await;
    e.queue("q", |_| {});
    for i in 0..5 {
        e.inject("q", msg(&format!("m{i}")));
    }
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let reply = mg
        .call(&mut c, "com.microsoft:peek-message", vec![("from-sequence-number", Value::Long(2)), ("message-count", Value::Int(2))])
        .await;
    assert_eq!(reply.status, 200);
    let msgs: Vec<String> = reply
        .body
        .map_get("messages")
        .unwrap()
        .as_list()
        .unwrap()
        .iter()
        .map(|m| body(&Message::decode(m.map_get("message").unwrap().as_binary().unwrap()).unwrap()))
        .collect();
    assert_eq!(msgs, ["m1", "m2"]);
    let reply = mg
        .call(&mut c, "com.microsoft:peek-message", vec![("from-sequence-number", Value::Long(99)), ("message-count", Value::Int(2))])
        .await;
    assert_eq!(reply.status, 204);
    assert_eq!(e.core("q", |c| c.counts().active), 5);
}

#[tokio::test]
async fn schedule_and_cancel_through_management_and_delivery_at_the_time() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let when = azemu_servicebus::state::now_ms() + 400;
    let scheduled = |text: &str| {
        let mut m = msg_id(text, text);
        m.message_annotations = Some(vec![(Value::sym("x-opt-scheduled-enqueue-time"), Value::Timestamp(when))]);
        Value::Map(vec![(Value::str("message-id"), Value::str(text)), (Value::str("message"), Value::Binary(m.encode()))])
    };
    let reply = mg
        .call(&mut c, "com.microsoft:schedule-message", vec![("messages", Value::List(vec![scheduled("later"), scheduled("cancelled")]))])
        .await;
    assert_eq!(reply.status, 200);
    let seqs = reply.body.map_get("sequence-numbers").unwrap().as_list().unwrap().to_vec();
    assert_eq!(seqs.len(), 2);
    assert_eq!(e.core("q", |c| (c.counts().active, c.counts().scheduled)), (0, 2));

    let reply =
        mg.call(&mut c, "com.microsoft:cancel-scheduled-message", vec![("sequence-numbers", Value::Array(vec![seqs[1].clone()]))]).await;
    assert_eq!(reply.status, 200);
    let reply =
        mg.call(&mut c, "com.microsoft:cancel-scheduled-message", vec![("sequence-numbers", Value::Array(vec![seqs[1].clone()]))]).await;
    assert_eq!(reply.status, 404, "cancelling twice");

    // peek shows it with state 2
    let reply = mg
        .call(&mut c, "com.microsoft:peek-message", vec![("from-sequence-number", Value::Long(1)), ("message-count", Value::Int(5))])
        .await;
    let first =
        Message::decode(reply.body.map_get("messages").unwrap().as_list().unwrap()[0].map_get("message").unwrap().as_binary().unwrap())
            .unwrap();
    assert_eq!(ann(&first, "x-opt-message-state"), Some(Value::Int(2)));

    let r = receiver(&mut c, "q", 5).await;
    no_delivery(&mut c, r, 100).await;
    let (d, m) = recv_body(&mut c, r).await;
    assert_eq!(body(&m), "later");
    assert!(azemu_servicebus::state::now_ms() >= when - 5);
    settle(&mut c, &d, DeliveryState::Accepted).await;
}

#[tokio::test]
async fn a_scheduled_annotation_on_a_plain_send_also_schedules() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    let mut m = msg("later");
    m.message_annotations =
        Some(vec![(Value::sym("x-opt-scheduled-enqueue-time"), Value::Timestamp(azemu_servicebus::state::now_ms() + 300))]);
    send(&mut c, s, &m).await;
    assert_eq!(e.core("q", |c| c.counts().scheduled), 1);
    let r = receiver_rd(&mut c, "q", 5).await;
    assert_eq!(body(&c.recv(r).await.message()), "later");
}

#[tokio::test]
async fn duplicate_detection_drops_repeated_message_ids() {
    let e = env().await;
    e.queue("q", |s| {
        s.requires_duplicate_detection = true;
        s.duplicate_window_ms = 60_000;
    });
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    for _ in 0..5 {
        send(&mut c, s, &msg_id("x", "same")).await;
    }
    send(&mut c, s, &msg_id("y", "other")).await;
    assert_eq!(e.core("q", |c| c.counts().active), 2);
}

#[tokio::test]
async fn duplicate_detection_window_expires() {
    let e = env().await;
    e.queue("q", |s| {
        s.requires_duplicate_detection = true;
        s.duplicate_window_ms = 200;
    });
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    send(&mut c, s, &msg_id("x", "same")).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    send(&mut c, s, &msg_id("x", "same")).await;
    assert_eq!(e.core("q", |c| c.counts().active), 2);
}

#[tokio::test]
async fn forwarding_chain_and_dead_letter_forwarding() {
    let e = env().await;
    e.queue("dest", |_| {});
    e.queue("dl-dest", |_| {});
    e.queue("mid", |s| s.forward_to = Some("dest".into()));
    e.queue("src", |s| {
        s.forward_to = Some("mid".into());
        s.forward_dead_lettered_to = Some("dl-dest".into());
    });
    let mut c = e.client().await;
    let s = sender(&mut c, "src").await;
    send(&mut c, s, &msg("x")).await;
    assert_eq!(e.core("dest", |c| c.counts().active), 1);
    assert_eq!(e.core("src", |c| c.counts().active), 0);
    let r = receiver_rd(&mut c, "dest", 1).await;
    let m = c.recv(r).await.message();
    assert_eq!(ann(&m, "x-opt-sequence-number"), Some(Value::Long(1)));

    // dead letters of `dl-source` go to its forward target and name their source
    e.queue("dl-source", |s| s.forward_dead_lettered_to = Some("dl-dest".into()));
    e.inject("dl-source", msg("poison"));
    let r2 = receiver(&mut c, "dl-source", 1).await;
    let d = c.recv(r2).await;
    settle(&mut c, &d, dead_letter("r", "d")).await;
    assert_eq!(e.core("dl-dest", |c| c.counts().active), 1);
    let r3 = receiver_rd(&mut c, "dl-dest", 5).await;
    let m = c.recv(r3).await.message();
    assert_eq!(body(&m), "poison");
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("r")));
    assert_eq!(ann(&m, "x-opt-deadletter-source"), Some(Value::str("dl-source")));
    assert_eq!(ann(&m, "x-opt-enqueue-sequence-number"), Some(Value::Long(1)), "the first sequence number is kept");
}

#[tokio::test]
async fn forwarding_loops_are_cut_off() {
    let e = env().await;
    e.queue("a", |s| s.forward_to = Some("b".into()));
    e.queue("b", |s| s.forward_to = Some("a".into()));
    let mut c = e.client().await;
    let s = sender(&mut c, "a").await;
    match c.send_message_wait(s, &msg("x")).await {
        Some(DeliveryState::Rejected { error: Some(err) }) => assert_eq!(err.condition, "amqp:resource-limit-exceeded"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn properties_survive_the_round_trip() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    let m = Message {
        properties: Some(Properties {
            message_id: Some(Value::str("id")),
            to: Some("to".into()),
            subject: Some("subj".into()),
            reply_to: Some("rt".into()),
            correlation_id: Some(Value::str("corr")),
            content_type: Some("text/plain".into()),
            group_id: None,
            ..Default::default()
        }),
        application_properties: Some(vec![
            (Value::str("a"), Value::Int(1)),
            (Value::str("b"), Value::String("two".into())),
            (Value::str("c"), Value::Bool(true)),
        ]),
        message_annotations: Some(vec![(Value::sym("x-opt-partition-key"), Value::str("pk"))]),
        header: Some(Header { ttl: Some(60_000), durable: Some(true), ..Default::default() }),
        ..msg("body")
    };
    send(&mut c, s, &m).await;
    let r = receiver_rd(&mut c, "q", 1).await;
    let got = c.recv(r).await.message();
    let p = got.properties.as_ref().unwrap();
    assert_eq!(
        (p.to.as_deref(), p.subject.as_deref(), p.reply_to.as_deref(), p.content_type.as_deref()),
        (Some("to"), Some("subj"), Some("rt"), Some("text/plain"))
    );
    assert_eq!(p.correlation_id, Some(Value::str("corr")));
    assert_eq!(app(&got, "a"), Some(Value::Int(1)));
    assert_eq!(app(&got, "c"), Some(Value::Bool(true)));
    assert_eq!(ann(&got, "x-opt-partition-key"), Some(Value::str("pk")));
    assert_eq!(got.header.unwrap().ttl, Some(60_000));
}

// ---- what the JavaScript and Go SDKs send ----------------------------------------------------
// Abandon is `modified` with `undeliverable-here` false and no `delivery-failed` (JavaScript, Go).

#[tokio::test]
async fn abandon_without_delivery_failed_counts_a_delivery() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 5).await;
    let (d, m) = recv_body(&mut c, r).await;
    assert_eq!(m.header.unwrap().delivery_count, Some(0));
    settle(&mut c, &d, DeliveryState::modified(false, false)).await;
    let (_, m2) = recv_body(&mut c, r).await;
    assert_eq!(m2.header.unwrap().delivery_count, Some(1));
}

#[tokio::test]
async fn abandon_without_delivery_failed_reaches_max_delivery_count() {
    let e = env().await;
    e.queue("q", |s| s.max_delivery_count = 2);
    e.inject("q", msg("poison"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 5).await;
    for _ in 0..2 {
        let (d, _) = recv_body(&mut c, r).await;
        settle(&mut c, &d, DeliveryState::modified(false, false)).await;
    }
    no_delivery(&mut c, r, 200).await;
    assert_eq!(e.core("q", |c| (c.counts().active, c.counts().dead_letter)), (0, 1));
    let dl = receiver_rd(&mut c, "q/$DeadLetterQueue", 2).await;
    let m = c.recv(dl).await.message();
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("MaxDeliveryCountExceeded")));
}

#[tokio::test]
async fn defer_without_delivery_failed_defers_and_applies_properties() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 5).await;
    let d = c.recv(r).await;
    let st = DeliveryState::Modified {
        delivery_failed: false,
        undeliverable_here: true,
        message_annotations: Some(Value::Map(vec![(Value::str("why"), Value::str("later"))])),
    };
    settle(&mut c, &d, st).await;
    no_delivery(&mut c, r, 200).await;
    e.core("q", |c| {
        let m = c.peek(azemu_servicebus::model::SubQueue::Main, 0, 10, None, azemu_servicebus::state::now_ms());
        assert_eq!(m.len(), 1);
        assert_eq!(app(&m[0].message, "why"), Some(Value::str("later")));
    });
}

#[tokio::test]
async fn abandon_applies_properties_to_modify_as_the_js_sdk_sends_them() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 2).await;
    let d = c.recv(r).await;
    // JavaScript: modified { undeliverable_here: false, message_annotations: propertiesToModify }
    let st = DeliveryState::Modified {
        delivery_failed: false,
        undeliverable_here: false,
        message_annotations: Some(Value::Map(vec![(Value::sym("retry"), Value::Int(3)), (Value::sym("note"), Value::str("n"))])),
    };
    settle(&mut c, &d, st).await;
    let (_, m) = recv_body(&mut c, r).await;
    assert_eq!(app(&m, "retry"), Some(Value::Int(3)));
    assert_eq!(app(&m, "note"), Some(Value::str("n")));
    assert_eq!(m.header.unwrap().delivery_count, Some(1));
}

#[tokio::test]
async fn dead_letter_copies_extra_info_entries_to_application_properties() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 2).await;
    let d = c.recv(r).await;
    let st = DeliveryState::rejected(
        Error::new("com.microsoft:dead-letter", "")
            .with_info("custom", Value::str("v"))
            .with_info("count", Value::Int(7))
            .with_info("DeadLetterReason", Value::str("bad"))
            .with_info("DeadLetterErrorDescription", Value::str("it is bad")),
    );
    settle(&mut c, &d, st).await;
    let dl = receiver_rd(&mut c, "q/$DeadLetterQueue", 2).await;
    let m = c.recv(dl).await.message();
    assert_eq!(app(&m, "custom"), Some(Value::str("v")));
    assert_eq!(app(&m, "count"), Some(Value::Int(7)));
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("bad")));
    assert_eq!(app(&m, "DeadLetterErrorDescription"), Some(Value::str("it is bad")));
}
