mod common;
use azemu_amqp::message::{Message, BATCH_MESSAGE_FORMAT};
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::types::Value;
use common::*;

#[tokio::test]
async fn send_receive_complete_with_service_bus_annotations() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    send(&mut c, s, &msg_id("hello", "m-1")).await;
    send(&mut c, s, &msg("second")).await;

    let r = receiver(&mut c, "q", 10).await;
    let (d, m) = recv_body(&mut c, r).await;
    assert_eq!(body(&m), "hello");
    assert_eq!(d.tag.len(), 16, "the delivery tag is the lock token");
    assert!(!d.settled);
    assert_eq!(ann(&m, "x-opt-sequence-number"), Some(Value::Long(1)));
    assert!(matches!(ann(&m, "x-opt-enqueued-time"), Some(Value::Timestamp(t)) if t > 1_600_000_000_000));
    assert!(matches!(ann(&m, "x-opt-locked-until"), Some(Value::Timestamp(t)) if t > 1_600_000_000_000));
    assert_eq!(m.header.as_ref().unwrap().delivery_count, Some(0));
    assert_eq!(m.properties.as_ref().unwrap().message_id, Some(Value::str("m-1")));

    // two-phase settlement: the broker echoes the outcome settled
    assert_eq!(settle(&mut c, &d, DeliveryState::Accepted).await, Some(DeliveryState::Accepted));
    let (d2, m2) = recv_body(&mut c, r).await;
    assert_eq!(body(&m2), "second");
    assert_eq!(ann(&m2, "x-opt-sequence-number"), Some(Value::Long(2)));
    settle(&mut c, &d2, DeliveryState::Accepted).await;
    assert_eq!(e.core("q", |c| c.counts().active), 0);
}

#[tokio::test]
async fn receive_and_delete_is_presettled() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver_rd(&mut c, "q", 5).await;
    let d = c.recv(r).await;
    assert!(d.settled);
    assert_eq!(e.core("q", |c| c.counts().active), 0);
}

#[tokio::test]
async fn batch_format_sends_every_message() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    let payload = Message::encode_batch(&[msg_id("a", "1").encode(), msg_id("b", "2").encode(), msg("c").encode()]);
    let id = c.send_payload(s, &payload, BATCH_MESSAGE_FORMAT, false).await;
    assert_eq!(c.wait_disposition(id).await, Some(DeliveryState::Accepted));
    assert_eq!(e.core("q", |c| c.counts().active), 3);
    let r = receiver_rd(&mut c, "q", 5).await;
    let got: Vec<String> = vec![body(&c.recv(r).await.message()), body(&c.recv(r).await.message()), body(&c.recv(r).await.message())];
    assert_eq!(got, ["a", "b", "c"]);
}

#[tokio::test]
async fn competing_receivers_share_the_messages_and_never_duplicate() {
    let e = env().await;
    e.queue("q", |_| {});
    for i in 0..20 {
        e.inject("q", msg(&format!("m{i}")));
    }
    let mut c1 = e.client().await;
    let mut c2 = e.client().await;
    let r1 = receiver_rd(&mut c1, "q", 20).await;
    let r2 = receiver_rd(&mut c2, "q", 20).await;
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..20 {
        let got = tokio::select! {
            d = c1.recv_for(r1, std::time::Duration::from_millis(500)) => d,
            d = c2.recv_for(r2, std::time::Duration::from_millis(500)) => d,
        };
        let d = got.expect("a delivery");
        assert!(seen.insert(body(&d.message())), "duplicate delivery");
    }
    assert_eq!(seen.len(), 20);
}

#[tokio::test]
async fn waiting_receiver_gets_a_message_sent_later() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut r = e.client().await;
    let rh = receiver(&mut r, "q", 5).await;
    let mut s = e.client().await;
    let sh = sender(&mut s, "q").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    send(&mut s, sh, &msg("late")).await;
    let d = r.recv(rh).await;
    assert_eq!(body(&d.message()), "late");
}

#[tokio::test]
async fn link_credit_is_respected_and_drain_works() {
    let e = env().await;
    e.queue("q", |_| {});
    for i in 0..5 {
        e.inject("q", msg(&format!("m{i}")));
    }
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 2).await;
    c.recv(r).await;
    c.recv(r).await;
    no_delivery(&mut c, r, 300).await;
    c.flow(r, 10, true).await;
    for _ in 0..3 {
        c.recv(r).await;
    }
    c.wait_until("drain", |c| c.links[&r].drain_flow.is_some()).await;
}

#[tokio::test]
async fn sending_to_a_session_queue_without_session_id_is_rejected() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    let s = sender(&mut c, "sq").await;
    match c.send_message_wait(s, &msg("x")).await {
        Some(DeliveryState::Rejected { error: Some(err) }) => assert_eq!(err.condition, "com.microsoft:argument-error"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn entity_names_are_case_insensitive() {
    let e = env().await;
    e.queue("MyQueue", |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "myqueue").await;
    send(&mut c, s, &msg("x")).await;
    assert_eq!(e.core("MYQUEUE", |c| c.counts().active), 1);
}

#[tokio::test]
async fn message_over_the_entity_limit_is_refused_by_the_link() {
    let e = env().await;
    e.queue("q", |s| s.max_message_size_kb = 1);
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    assert_eq!(c.links[&s].attach.as_ref().unwrap().max_message_size, Some(1024));
    match c.send_message_wait(s, &Message::data(vec![1; 4000])).await {
        Some(DeliveryState::Rejected { error: Some(err) }) => assert_eq!(err.condition, "amqp:link:message-size-exceeded"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn full_entity_rejects_with_quota_exceeded() {
    let e = env().await;
    e.queue("q", |s| s.max_size_mb = 1);
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    let big = Message::data(vec![7; 200_000]);
    let mut rejected = None;
    for _ in 0..8 {
        match c.send_message_wait(s, &big).await {
            Some(DeliveryState::Accepted) => {}
            Some(DeliveryState::Rejected { error: Some(err) }) => {
                rejected = Some(err);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(rejected.expect("quota error").condition, "amqp:resource-limit-exceeded");
}

#[tokio::test]
async fn missing_entity_with_autocreate_off_is_not_found() {
    let e = env_with(azemu_servicebus::Shared::new(false, None)).await;
    let mut c = e.client().await;
    let err = c.attach_sender("nope").await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:not-found");
    assert!(err.description.unwrap().contains("could not be found"));
    let err = c.attach_receiver("nope/Subscriptions/x", 1).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:not-found");
}

#[tokio::test]
async fn autocreate_makes_queues_topics_and_subscriptions() {
    let e = env().await;
    let mut c = e.client().await;
    c.attach_sender("auto-q").await.unwrap();
    c.attach_receiver("auto-t/Subscriptions/auto-s", 1).await.unwrap();
    e.state(|st| {
        let ns = st.ns("default");
        assert!(ns.queue("auto-q").is_some());
        let t = ns.topic("auto-t").expect("topic created");
        assert_eq!(t.subs["auto-s"].rules.len(), 1, "$Default rule");
    });
}

#[tokio::test]
async fn namespaces_come_from_the_open_hostname_and_are_isolated() {
    let e = env().await;
    let mut a = e.client_for("alpha.servicebus.localhost").await;
    let mut d = e.client_for("localhost:4000").await;
    let mut i = e.client_for("127.0.0.1").await;
    let sa = sender(&mut a, "q").await;
    send(&mut a, sa, &msg("x")).await;
    let sd = sender(&mut d, "q").await;
    send(&mut d, sd, &msg("y")).await;
    let si = sender(&mut i, "q").await;
    send(&mut i, si, &msg("z")).await;
    e.state(|st| {
        assert_eq!(st.ns("alpha").queue("q").unwrap().core.counts().active, 1);
        assert_eq!(st.ns("default").queue("q").unwrap().core.counts().active, 2, "localhost and an IP are the default namespace");
    });
}

#[tokio::test]
async fn reset_drops_everything() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    e.srv.reset();
    assert!(e.state(|st| st.ns("default").queue("q").is_none()));
}

#[tokio::test]
async fn cbs_put_token_is_accepted() {
    let e = env().await;
    let mut c = e.client().await;
    let resp = receiver_rd(&mut c, "$cbs", 10).await;
    let req = sender(&mut c, "$cbs").await;
    let m = Message {
        properties: Some(azemu_amqp::message::Properties {
            message_id: Some(Value::Uuid([7; 16])),
            reply_to: Some("$cbs".into()),
            ..Default::default()
        }),
        application_properties: Some(vec![
            (Value::str("operation"), Value::str("put-token")),
            (Value::str("type"), Value::str("servicebus.windows.net:sastoken")),
            (Value::str("name"), Value::str("sb://default.servicebus.localhost/q")),
            (Value::str("expiration"), Value::Timestamp(4_000_000_000_000)),
        ]),
        body: azemu_amqp::message::Body::Value(Value::str("SharedAccessSignature sr=x&sig=y&se=1&skn=k")),
        ..Default::default()
    };
    send(&mut c, req, &m).await;
    let reply = c.recv(resp).await.message();
    assert_eq!(reply.properties.unwrap().correlation_id, Some(Value::Uuid([7; 16])));
    assert_eq!(
        reply.application_properties.as_ref().unwrap().iter().find(|(k, _)| k.as_str() == Some("status-code")).unwrap().1.as_i64(),
        Some(202)
    );
}

#[tokio::test]
async fn address_with_scheme_and_host_is_accepted() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "amqps://default.servicebus.localhost:5671/q").await;
    send(&mut c, s, &msg("x")).await;
    assert_eq!(e.core("q", |c| c.counts().active), 1);
}

#[tokio::test]
async fn a_message_sent_without_a_message_id_gets_one() {
    // Service Bus gives every message a message-id; the JavaScript SDK cannot settle one without it
    let e = env().await;
    e.queue("q", |_| {});
    e.topic("t");
    e.sub("t", "s1", vec![azemu_servicebus::rules::Rule::default_rule(0)], |_| {});
    e.sub("t", "s2", vec![azemu_servicebus::rules::Rule::default_rule(0)], |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    let mut plain = msg("no id");
    plain.properties = None;
    send(&mut c, s, &plain).await;
    send(&mut c, s, &msg_id("with id", "mine")).await;
    let r = receiver_rd(&mut c, "q", 5).await;
    let a = c.recv(r).await.message();
    let id = a.properties.as_ref().and_then(|p| p.message_id.clone());
    let Some(Value::String(id)) = id else { panic!("no message id was assigned") };
    assert_eq!(id.len(), 32, "a GUID without dashes: {id}");
    let b = c.recv(r).await.message();
    assert_eq!(b.properties.as_ref().unwrap().message_id, Some(Value::str("mine")));

    // a topic message has one id for all its copies
    let ts = sender(&mut c, "t").await;
    send(&mut c, ts, &plain).await;
    let r1 = receiver_rd(&mut c, "t/Subscriptions/s1", 2).await;
    let r2 = receiver_rd(&mut c, "t/Subscriptions/s2", 2).await;
    let m1 = c.recv(r1).await.message();
    let m2 = c.recv(r2).await.message();
    let id1 = m1.properties.as_ref().and_then(|p| p.message_id.clone());
    assert!(id1.is_some());
    assert_eq!(id1, m2.properties.as_ref().and_then(|p| p.message_id.clone()));
}
