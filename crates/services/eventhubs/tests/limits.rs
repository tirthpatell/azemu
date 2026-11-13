//! Limits on what a client can make the emulator hold.

mod common;
use azemu_eventhubs::model::{MAX_CONSUMER_GROUPS, MAX_HUBS_PER_NAMESPACE};
use common::*;

#[tokio::test]
async fn a_partition_over_its_byte_limit_drops_its_oldest_events() {
    let env = env().await;
    env.hub("h", 1);
    env.shared.state.lock().hub_mut("myns", "h").unwrap().partitions[0].max_bytes = 100_000;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/0").await.unwrap();
    for i in 0..50 {
        send_one(&mut c, s, &event(&format!("{i:03}{}", "x".repeat(9_997)))).await;
    }
    let (held, begin, last) = {
        let st = env.shared.state.lock();
        let p = &st.hub("myns", "h").unwrap().partitions[0];
        (p.bytes(), p.first_seq(), p.last_seq())
    };
    assert!(held <= 100_000, "{held} bytes kept");
    assert_eq!(last, 49);
    assert!(begin >= 38, "the oldest events should be gone, first is {begin}");
    // a receiver from the start gets what is left, newest included
    let r = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    let first = body_of(&c.recv(r).await);
    assert!(first.starts_with(&format!("{begin:03}")), "{}", &first[..3]);
    let mut n = 1;
    let mut last_body = first;
    while let Some(d) = c.recv_for(r, std::time::Duration::from_millis(200)).await {
        last_body = body_of(&d);
        n += 1;
    }
    assert!(last_body.starts_with("049"));
    assert_eq!(n as i64, 50 - begin);
}

#[tokio::test]
async fn retention_by_time_keeps_the_byte_count_right() {
    let env = env().await;
    env.hub("h", 1);
    let mut st = env.shared.state.lock();
    let h = st.hub_mut("myns", "h").unwrap();
    let p = &mut h.partitions[0];
    p.append(event("aaaa"), 1_000);
    p.append(event("bbbb"), 2_000);
    let both = p.bytes();
    assert!(both > 0);
    p.trim(1_500 + 100, 600);
    assert_eq!(p.events.len(), 1);
    assert_eq!(p.bytes(), both / 2);
    p.trim(10_000, 600);
    assert_eq!(p.bytes(), 0);
}

#[tokio::test]
async fn a_namespace_holds_at_most_1000_event_hubs() {
    let env = env().await;
    for i in 0..MAX_HUBS_PER_NAMESPACE {
        env.hub(&format!("h{i}"), 1);
    }
    let err = env.shared.state.lock().create_hub("myns", "one-more", 1, 3_600_000, 0).err().unwrap();
    assert_eq!((err.condition, err.status), ("amqp:resource-limit-exceeded", 403));
    // an attach that would auto-create one is refused, an existing hub still works
    let mut c = env.client().await;
    assert_eq!(condition(attach_sender(&mut c, "brand-new").await.unwrap_err()), "amqp:resource-limit-exceeded");
    assert!(attach_sender(&mut c, "h5").await.is_ok());
    // other namespaces have their own count
    assert!(env.shared.state.lock().create_hub("other", "h", 1, 3_600_000, 0).is_ok());
}

#[tokio::test]
async fn a_hub_holds_at_most_100_consumer_groups() {
    let env = env().await;
    env.hub("h", 1);
    {
        let mut st = env.shared.state.lock();
        let h = st.hub_mut("myns", "h").unwrap();
        for i in 0..(MAX_CONSUMER_GROUPS - 1) {
            h.add_consumer_group(&format!("g{i}"));
        }
    }
    let mut c = env.client().await;
    assert!(consume(&mut c, "h", "g0", 0, &from_start()).await.is_ok());
    assert_eq!(condition(consume(&mut c, "h", "one-too-many", 0, &from_start()).await.unwrap_err()), "amqp:resource-limit-exceeded");
}

#[tokio::test]
async fn event_hubs_has_no_transactions_so_the_coordinator_attach_is_refused() {
    let e = env().await;
    let mut c = e.client().await;
    let mut a = azemu_amqp::performatives::Attach::new("txn", 0, azemu_amqp::performatives::Role::Sender);
    a.source = Some(azemu_amqp::terminus::Source::default());
    a.target = Some(azemu_amqp::terminus::TargetKind::Coordinator(Default::default()));
    // an attach that is refused is answered with null termini and a detach that carries the error
    let err = c.attach(a).await.unwrap_err().expect("a detach with an error");
    assert_eq!(err.condition, "amqp:not-implemented");
}

#[tokio::test]
async fn a_batch_of_empty_events_is_limited_in_number_and_each_event_is_charged_its_overhead() {
    use azemu_eventhubs::model::{EVENT_OVERHEAD, MAX_EVENTS_PER_BATCH};
    let env = env().await;
    env.hub("h", 1);
    env.shared.state.lock().hub_mut("myns", "h").unwrap().partitions[0].max_bytes = 1000 * EVENT_OVERHEAD;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/0").await.unwrap();
    // 60,000 empty events are about 300 KB on the wire, under the 1 MiB message limit, but over the limit on events
    let empty = azemu_amqp::message::Message::default();
    let too_many = vec![empty.clone(); MAX_EVENTS_PER_BATCH + 1];
    let state = send_batch(&mut c, s, &too_many).await;
    match state {
        Some(azemu_amqp::terminus::DeliveryState::Rejected { error: Some(e) }) => {
            assert_eq!(e.condition, "amqp:link:message-size-exceeded")
        }
        other => panic!("a batch over the event limit must be rejected: {other:?}"),
    }
    assert!(env.shared.state.lock().hub("myns", "h").unwrap().partitions[0].is_empty());
    // within the limit it is stored, and empty events cost their overhead against the partition's byte limit
    let ok = vec![empty; MAX_EVENTS_PER_BATCH];
    assert_eq!(send_batch(&mut c, s, &ok).await, Some(azemu_amqp::terminus::DeliveryState::Accepted));
    let st = env.shared.state.lock();
    let p = &st.hub("myns", "h").unwrap().partitions[0];
    assert!(p.bytes() <= 1000 * EVENT_OVERHEAD && p.events.len() <= 1000, "{} events, {} bytes", p.events.len(), p.bytes());
    assert_eq!(p.last_seq(), MAX_EVENTS_PER_BATCH as i64 - 1);
}
