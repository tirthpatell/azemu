//! Limits on what a client can make the emulator hold or send back.

mod common;
use azemu_amqp::message::Message;
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::types::Value;
use common::*;

fn big(n: usize) -> Message {
    Message::data(vec![b'x'; n])
}

#[tokio::test]
async fn request_links_announce_a_max_message_size() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let cbs = c.attach_sender("$cbs").await.unwrap();
    assert_eq!(c.links[&cbs].attach.as_ref().unwrap().max_message_size, Some(1024 * 1024));
    let mgmt = c.attach_sender("q/$management").await.unwrap();
    assert_eq!(c.links[&mgmt].attach.as_ref().unwrap().max_message_size, Some(16 * 1024 * 1024));
}

#[tokio::test]
async fn a_cbs_request_over_the_limit_is_rejected() {
    let e = env().await;
    let mut c = e.client().await;
    let cbs = c.attach_sender("$cbs").await.unwrap();
    let out = c.send_message_wait(cbs, &big(2 * 1024 * 1024)).await;
    match out {
        Some(DeliveryState::Rejected { error: Some(err) }) => assert_eq!(err.condition, "amqp:link:message-size-exceeded"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn peek_returns_at_most_250_messages() {
    let e = env().await;
    e.queue("q", |_| {});
    for i in 0..300 {
        e.inject("q", msg(&format!("m{i}")));
    }
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let r = mg
        .call(&mut c, "com.microsoft:peek-message", vec![("from-sequence-number", Value::Long(1)), ("message-count", Value::Int(1000))])
        .await;
    assert_eq!(r.status, 200);
    let n = r.body.map_get("messages").unwrap().as_list().unwrap().len();
    assert_eq!(n, 250);
}

#[tokio::test]
async fn a_peek_answer_stops_after_about_8_mib() {
    let e = env().await;
    e.queue("q", |_| {});
    for _ in 0..60 {
        e.inject("q", big(200_000));
    }
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let r = mg
        .call(&mut c, "com.microsoft:peek-message", vec![("from-sequence-number", Value::Long(1)), ("message-count", Value::Int(250))])
        .await;
    assert_eq!(r.status, 200);
    let n = r.body.map_get("messages").unwrap().as_list().unwrap().len();
    assert!((40..=44).contains(&n), "{n} messages of 200 KB in one answer");
    // the rest is there for the next peek
    let r = mg
        .call(
            &mut c,
            "com.microsoft:peek-message",
            vec![("from-sequence-number", Value::Long(1 + n as i64)), ("message-count", Value::Int(250))],
        )
        .await;
    assert_eq!(r.body.map_get("messages").unwrap().as_list().unwrap().len(), 60 - n);
}

#[tokio::test]
async fn a_namespace_holds_at_most_10000_queues_and_topics() {
    let e = env().await;
    for i in 0..10_000 {
        e.state(|st| st.ns("default").create_queue(&format!("q{i}"), azemu_servicebus::model::Settings::queue_default(0))).unwrap();
    }
    let err = e.state(|st| st.ns("default").create_queue("one-more", azemu_servicebus::model::Settings::queue_default(0))).unwrap_err();
    assert_eq!((err.condition, err.status), ("amqp:resource-limit-exceeded", 403));
    let err = e.state(|st| st.ns("default").create_topic("one-more", azemu_servicebus::model::Settings::topic_default(0))).unwrap_err();
    assert_eq!(err.status, 403);
    // an auto-created entity from an AMQP attach is refused the same way, and the connection lives on
    let mut c = e.client().await;
    let refused = c.attach_sender("brand-new").await.unwrap_err().unwrap();
    assert_eq!(refused.condition, "amqp:resource-limit-exceeded");
    // another namespace has its own quota
    assert!(e.state(|st| st.ns("other").create_queue("q", azemu_servicebus::model::Settings::queue_default(0))).is_ok());
}

#[tokio::test]
async fn a_topic_holds_at_most_2000_subscriptions() {
    let e = env().await;
    e.topic("t");
    for i in 0..2000 {
        e.sub("t", &format!("s{i}"), vec![azemu_servicebus::rules::Rule::default_rule(0)], |_| {});
    }
    let err = e
        .state(|st| {
            st.ns("default").create_subscription(
                "t",
                "s2000",
                azemu_servicebus::model::Settings::subscription_default(0),
                vec![azemu_servicebus::rules::Rule::default_rule(0)],
            )
        })
        .unwrap_err();
    assert_eq!((err.condition, err.status), ("amqp:resource-limit-exceeded", 403));
}

// ---- a batch of empty messages (the Kafka K1 pattern) ----

async fn send_batch(c: &mut azemu_amqp::client::TestClient, link: u32, messages: &[Message]) -> Option<DeliveryState> {
    let encoded: Vec<Vec<u8>> = messages.iter().map(|m| m.encode()).collect();
    let id = c.send_payload(link, &Message::encode_batch(&encoded), azemu_amqp::message::BATCH_MESSAGE_FORMAT, false).await;
    c.wait_disposition(id).await
}

#[tokio::test]
async fn a_batch_holds_at_most_50000_messages() {
    use azemu_servicebus::state::MAX_MESSAGES_PER_BATCH;
    let e = env().await;
    // a Premium style entity (messages up to 100 MB): its link does not stop a batch of 50,001 empty messages
    // (about 250 KB on the wire) by size, so the limit on the number of messages has to
    e.queue("q", |s| s.max_message_size_kb = 100 * 1024);
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    let empty = Message::default();
    match send_batch(&mut c, s, &vec![empty.clone(); MAX_MESSAGES_PER_BATCH + 1]).await {
        Some(DeliveryState::Rejected { error: Some(err) }) => {
            assert_eq!(err.condition, "amqp:link:message-size-exceeded");
            assert!(err.description.unwrap_or_default().contains("at most 50000"));
        }
        other => panic!("a batch over the limit must be rejected: {other:?}"),
    }
    assert_eq!(e.core("q", |c| c.main.len()), 0);
    // the connection goes on working
    send(&mut c, s, &msg("after")).await;
    assert_eq!(e.core("q", |c| c.main.len()), 1);
}

#[tokio::test]
async fn empty_messages_are_charged_their_overhead_and_the_entity_refuses_at_its_size_limit() {
    use azemu_servicebus::store::MESSAGE_OVERHEAD;
    let e = env().await;
    e.queue("q", |s| s.max_size_mb = 1);
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    let empty = Message::default();
    // one batch of 50,000 would be over the 1 MB limit once each message is charged: refused whole, nothing stored
    match send_batch(&mut c, s, &vec![empty.clone(); 50_000]).await {
        Some(DeliveryState::Rejected { error: Some(err) }) => assert_eq!(err.condition, "amqp:resource-limit-exceeded"),
        other => panic!("{other:?}"),
    }
    assert_eq!(e.core("q", |c| c.main.len()), 0);
    // many transfers of 500: two fit (500 x about 1 KB each), the rest are refused and memory stays bounded
    let mut accepted = 0;
    for _ in 0..200 {
        match send_batch(&mut c, s, &vec![empty.clone(); 500]).await {
            Some(DeliveryState::Accepted) => accepted += 1,
            Some(DeliveryState::Rejected { error: Some(err) }) => assert_eq!(err.condition, "amqp:resource-limit-exceeded"),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(accepted, 2);
    let (n, bytes) = e.core("q", |c| (c.main.len(), c.size_bytes()));
    assert_eq!(n, 1000);
    assert!(bytes <= 1024 * 1024 && bytes >= (n * MESSAGE_OVERHEAD) as u64, "{bytes} bytes charged for {n} messages");
    // single messages fit until the limit and are refused after it
    let mut refused = false;
    for _ in 0..100 {
        match c.send_message_wait(s, &Message::default()).await {
            Some(DeliveryState::Accepted) => {}
            Some(DeliveryState::Rejected { error: Some(err) }) => {
                assert_eq!(err.condition, "amqp:resource-limit-exceeded");
                refused = true;
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(refused);
    assert!(e.core("q", |c| c.main.len()) <= 1024);
}

#[tokio::test]
async fn every_way_a_message_is_stored_charges_the_overhead() {
    use azemu_servicebus::enqueue::{EnqueueMessage, EnqueueTarget};
    use azemu_servicebus::store::MESSAGE_OVERHEAD;
    let e = env().await;
    e.queue("q", |s| s.max_size_mb = 1);
    // Event Grid style delivery through enqueue.rs
    let mut stored = 0;
    for _ in 0..2000 {
        if e.shared.enqueue_message("default", EnqueueTarget::Queue("q"), EnqueueMessage::default()).is_ok() {
            stored += 1;
        }
    }
    assert!((1000..=1024).contains(&stored), "{stored} empty deliveries stored in a 1 MB queue");
    // topic fan-out copies: each subscription charges its own copy; the full one refuses, the other keeps taking
    e.topic("t");
    e.sub("t", "small", vec![azemu_servicebus::rules::Rule::default_rule(0)], |s| s.max_size_mb = 1);
    e.sub("t", "big", vec![azemu_servicebus::rules::Rule::default_rule(0)], |_| {});
    for _ in 0..1500 {
        let _ = e.shared.enqueue_message("default", EnqueueTarget::Topic("t"), EnqueueMessage::default());
    }
    let (small, big) = e.state(|st| {
        let t = st.ns("default").topic_mut("t").unwrap();
        (t.subs["small"].core.main.len(), t.subs["big"].core.main.len())
    });
    assert!(small <= 1024, "{small} messages in a 1 MB subscription");
    assert!(big > small, "{big}");
    // scheduled messages sent to a topic wait in the topic's own store, charged the same way
    e.state(|st| st.ns("default").topic_mut("t").unwrap().settings.max_size_mb = 1);
    let mut c = e.client().await;
    let s = sender(&mut c, "t").await;
    let mut when = Message::default();
    when.set_annotation("x-opt-scheduled-enqueue-time", Value::Timestamp(azemu_servicebus::state::now_ms() + 3_600_000));
    let mut ok = 0;
    for _ in 0..4 {
        if send_batch(&mut c, s, &vec![when.clone(); 300]).await == Some(DeliveryState::Accepted) {
            ok += 1;
        }
    }
    assert_eq!(ok, 3, "three batches of 300 fit the 1 MB scheduled store, the fourth does not");
    let scheduled = e.state(|st| st.ns("default").topic("t").unwrap().scheduled.bytes);
    assert!(scheduled >= (900 * MESSAGE_OVERHEAD) as u64 && scheduled <= 1024 * 1024, "{scheduled}");
    // a dead-lettered message keeps its charge (it moves, it does not grow)
    e.queue("dl", |_| {});
    e.inject("dl", msg("x"));
    let before = e.core("dl", |c| c.size_bytes());
    let mut c2 = e.client().await;
    let r = receiver(&mut c2, "dl", 1).await;
    let (d, _) = recv_body(&mut c2, r).await;
    settle(&mut c2, &d, DeliveryState::Rejected { error: None }).await;
    let (main, dlq, total) = e.core("dl", |c| (c.main.len(), c.dlq.len(), c.size_bytes()));
    assert_eq!((main, dlq), (0, 1));
    assert!(total >= MESSAGE_OVERHEAD as u64 && total <= before + 200, "{before} -> {total}");
}
