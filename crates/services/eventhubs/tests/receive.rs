mod common;

use std::time::Duration;

use azemu_amqp::types::Value;
use common::*;

/// Five events in partition 0 of hub `h`; returns their (sequence, offset, enqueued time).
async fn five(env: &Env) -> Vec<(i64, String, i64)> {
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/0").await.unwrap();
    for i in 0..5 {
        send_one(&mut c, s, &event(&format!("e{i}"))).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let r = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    let mut out = Vec::new();
    for _ in 0..5 {
        let m = c.recv(r).await.message();
        let t = match m.annotation("x-opt-enqueued-time") {
            Some(Value::Timestamp(t)) => *t,
            other => panic!("{other:?}"),
        };
        out.push((ann_i64(&m, "x-opt-sequence-number"), ann_str(&m, "x-opt-offset"), t));
    }
    out
}

#[tokio::test]
async fn start_positions_by_offset_sequence_number_and_time() {
    let env = env().await;
    let ev = five(&env).await;
    let mut c = env.client().await;
    let cases: Vec<(String, Vec<&str>)> = vec![
        ("amqp.annotation.x-opt-offset > '-1'".into(), vec!["e0", "e1", "e2", "e3", "e4"]),
        (format!("amqp.annotation.x-opt-offset > '{}'", ev[1].1), vec!["e2", "e3", "e4"]),
        (format!("amqp.annotation.x-opt-offset >= '{}'", ev[1].1), vec!["e1", "e2", "e3", "e4"]),
        (format!("amqp.annotation.x-opt-offset > '{}'", ev[4].1), vec![]),
        ("amqp.annotation.x-opt-sequence-number > '2'".into(), vec!["e3", "e4"]),
        ("amqp.annotation.x-opt-sequence-number >= '2'".into(), vec!["e2", "e3", "e4"]),
        ("amqp.annotation.x-opt-sequence-number > 4".into(), vec![]),
        ("amqp.annotation.x-opt-sequence-number > '-1'".into(), vec!["e0", "e1", "e2", "e3", "e4"]),
        (
            format!("amqp.annotation.x-opt-enqueued-time > '{}'", ev[2].2),
            vec!["e3", "e4"].into_iter().filter(|_| ev[3].2 > ev[2].2).collect(),
        ),
        (format!("amqp.annotation.x-opt-enqueued-time >= '{}'", ev[4].2), vec!["e4"]),
        ("amqp.annotation.x-opt-enqueued-time > '1000'".into(), vec!["e0", "e1", "e2", "e3", "e4"]),
        ("amqp.annotation.x-opt-enqueued-time > '9999999999999'".into(), vec![]),
        ("amqp.annotation.x-opt-offset > '@latest'".into(), vec![]),
    ];
    for (filter, want) in cases {
        let r = consume(&mut c, "h", "$Default", 0, &with_filter(&filter)).await.unwrap();
        assert_eq!(take(&mut c, r, want.len()).await, want, "filter {filter}");
        expect_quiet(&mut c, r).await;
        c.detach(r).await;
    }
}

#[tokio::test]
async fn no_filter_reads_from_the_start() {
    let env = env().await;
    five(&env).await;
    let mut c = env.client().await;
    let r = consume(&mut c, "h", "$Default", 0, &Consumer { credit: 10, ..Default::default() }).await.unwrap();
    assert_eq!(take(&mut c, r, 5).await.len(), 5);
}

#[tokio::test]
async fn latest_only_sees_new_events_and_tails_live() {
    let env = env().await;
    five(&env).await;
    let mut c = env.client().await;
    let r = consume(&mut c, "h", "$Default", 0, &with_filter("amqp.annotation.x-opt-offset > '@latest'")).await.unwrap();
    expect_quiet(&mut c, r).await;
    let mut p = env.client().await;
    let s = attach_sender(&mut p, "h/Partitions/0").await.unwrap();
    send_one(&mut p, s, &event("live-1")).await;
    send_one(&mut p, s, &event("live-2")).await;
    assert_eq!(take(&mut c, r, 2).await, vec!["live-1", "live-2"]);
    // a receiver that is idle at the end keeps getting events
    send_one(&mut p, s, &event("live-3")).await;
    assert_eq!(take(&mut c, r, 1).await, vec!["live-3"]);
}

#[tokio::test]
async fn a_receiver_that_reads_to_the_end_gets_later_events() {
    let env = env().await;
    five(&env).await;
    let mut c = env.client().await;
    let r = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    assert_eq!(take(&mut c, r, 5).await.len(), 5);
    let mut p = env.client().await;
    let s = attach_sender(&mut p, "h/Partitions/0").await.unwrap();
    send_one(&mut p, s, &event("late")).await;
    assert_eq!(take(&mut c, r, 1).await, vec!["late"]);
}

#[tokio::test]
async fn reading_does_not_remove_events_and_readers_have_their_own_position() {
    let env = env().await;
    five(&env).await;
    let mut a = env.client().await;
    let mut b = env.client().await;
    let ra = consume(&mut a, "h", "$Default", 0, &from_start()).await.unwrap();
    let rb = consume(&mut b, "h", "$Default", 0, &with_filter("amqp.annotation.x-opt-sequence-number > '2'")).await.unwrap();
    assert_eq!(take(&mut a, ra, 5).await.len(), 5);
    assert_eq!(take(&mut b, rb, 2).await, vec!["e3", "e4"]);
    // reading again from the start still sees everything
    a.detach(ra).await;
    let ra = consume(&mut a, "h", "$Default", 0, &from_start()).await.unwrap();
    assert_eq!(take(&mut a, ra, 5).await.len(), 5);
}

#[tokio::test]
async fn consumer_groups_are_independent_and_created_on_first_attach() {
    let env = env().await;
    five(&env).await;
    let mut c = env.client().await;
    let r1 = consume(&mut c, "h", "cg-one", 0, &from_start()).await.unwrap();
    let r2 = consume(&mut c, "h", "CG-TWO", 0, &from_start()).await.unwrap();
    assert_eq!(take(&mut c, r1, 5).await.len(), 5);
    assert_eq!(take(&mut c, r2, 5).await.len(), 5);
    let groups = env.shared.state.lock().hub("myns", "h").unwrap().consumer_groups.values().cloned().collect::<Vec<_>>();
    assert!(
        groups.contains(&"$Default".to_string()) && groups.contains(&"cg-one".to_string()) && groups.contains(&"CG-TWO".to_string()),
        "{groups:?}"
    );
}

#[tokio::test]
async fn link_credit_limits_what_is_sent() {
    let env = env().await;
    five(&env).await;
    let mut c = env.client().await;
    let r = consume(&mut c, "h", "$Default", 0, &Consumer { credit: 2, ..from_start() }).await.unwrap();
    assert_eq!(take(&mut c, r, 2).await, vec!["e0", "e1"]);
    expect_quiet(&mut c, r).await;
    c.flow(r, 2, false).await;
    assert_eq!(take(&mut c, r, 2).await, vec!["e2", "e3"]);
    c.flow(r, 10, false).await;
    assert_eq!(take(&mut c, r, 1).await, vec!["e4"]);
}

#[tokio::test]
async fn drain_returns_the_credit() {
    let env = env().await;
    five(&env).await;
    let mut c = env.client().await;
    let r = consume(&mut c, "h", "$Default", 0, &Consumer { credit: 0, ..from_start() }).await.unwrap();
    c.flow(r, 100, true).await;
    assert_eq!(take(&mut c, r, 5).await.len(), 5);
    c.wait_until("drain flow", |c| c.links[&r].drain_flow.is_some()).await;
}

#[tokio::test]
async fn runtime_metrics_ride_on_delivery_annotations() {
    let env = env().await;
    let ev = five(&env).await;
    let mut c = env.client().await;
    let r = consume(&mut c, "h", "$Default", 0, &Consumer { metrics: true, ..from_start() }).await.unwrap();
    let first = c.recv(r).await.message();
    let da = Value::Map(first.delivery_annotations.clone().expect("delivery annotations"));
    assert_eq!(da.map_get("last_enqueued_sequence_number").and_then(|v| v.as_i64()), Some(4));
    assert_eq!(da.map_get("last_enqueued_offset").and_then(|v| v.as_str()), Some(ev[4].1.as_str()));
    assert!(matches!(da.map_get("last_enqueued_time_utc"), Some(Value::Timestamp(t)) if *t == ev[4].2));
    assert!(matches!(da.map_get("runtime_info_retrieval_time_utc"), Some(Value::Timestamp(_))));
    // the attach reply offers the capability
    assert!(c.links[&r].attach.as_ref().unwrap().offered_capabilities.iter().any(|x| x == "com.microsoft:enable-receiver-runtime-metric"));
    // without the capability there are none
    let r2 = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    assert!(c.recv(r2).await.message().delivery_annotations.is_none());
}

#[tokio::test]
async fn bad_selector_and_bad_position_are_refused() {
    let env = env().await;
    five(&env).await;
    let mut c = env.client().await;
    for f in ["amqp.annotation.x-opt-offset > 'abc'", "amqp.annotation.x-opt-foo > '1'", "garbage"] {
        let e = consume(&mut c, "h", "$Default", 0, &with_filter(f)).await.unwrap_err();
        assert_eq!(condition(e), "com.microsoft:argument-error", "{f}");
    }
    let e = consume(&mut c, "h", "$Default", 0, &with_filter("amqp.annotation.x-opt-sequence-number > '50'")).await.unwrap_err();
    assert_eq!(condition(e), "com.microsoft:argument-out-of-range");
    let e = consume(&mut c, "h", "$Default", 9, &from_start()).await.unwrap_err();
    assert_eq!(condition(e), "com.microsoft:argument-out-of-range");
}

#[tokio::test]
async fn receiving_from_the_hub_address_without_a_consumer_group_is_refused() {
    let env = env().await;
    let mut c = env.client().await;
    let e = attach_receiver(&mut c, "h/Partitions/0", 10).await.unwrap_err();
    assert_eq!(condition(e), "amqp:not-allowed");
    let e = attach_receiver(&mut c, "h", 10).await.unwrap_err();
    assert_eq!(condition(e), "amqp:not-allowed");
}

#[tokio::test]
async fn epoch_receivers_steal_from_lower_ones_and_refuse_lower_ones() {
    let env = env().await;
    five(&env).await;
    let mut a = env.client().await;
    let mut b = env.client().await;
    let ra = consume(&mut a, "h", "$Default", 0, &Consumer { epoch: Some(5), ..from_start() }).await.unwrap();
    assert_eq!(take(&mut a, ra, 5).await.len(), 5);
    // a lower epoch cannot attach
    let e = consume(&mut b, "h", "$Default", 0, &Consumer { epoch: Some(4), ..from_start() }).await.unwrap_err();
    assert_eq!(condition(e.clone()), "amqp:link:stolen");
    assert!(e.unwrap().description.unwrap().contains("higher epoch of '5'"));
    // neither can a receiver without an epoch
    let e = consume(&mut b, "h", "$Default", 0, &from_start()).await.unwrap_err();
    assert_eq!(condition(e), "amqp:link:stolen");
    // a higher one takes over and the first is detached with link:stolen
    let rb = consume(&mut b, "h", "$Default", 0, &Consumer { epoch: Some(6), ..from_start() }).await.unwrap();
    let e = a.wait_detached(ra).await.expect("stolen error");
    assert_eq!(e.condition, "amqp:link:stolen");
    assert!(e.description.unwrap().contains("higher epoch of '6'"));
    assert_eq!(take(&mut b, rb, 5).await.len(), 5);
    // an equal epoch steals too (the SDK's load balancing relies on it)
    let rc = consume(&mut a, "h", "$Default", 0, &Consumer { epoch: Some(6), ..from_start() }).await.unwrap();
    assert_eq!(b.wait_detached(rb).await.unwrap().condition, "amqp:link:stolen");
    assert_eq!(take(&mut a, rc, 5).await.len(), 5);
}

#[tokio::test]
async fn epochs_are_per_partition_and_per_consumer_group() {
    let env = env().await;
    env.hub("h", 2);
    let mut a = env.client().await;
    let mut b = env.client().await;
    consume(&mut a, "h", "$Default", 0, &Consumer { epoch: Some(5), ..from_start() }).await.unwrap();
    consume(&mut b, "h", "$Default", 1, &Consumer { epoch: Some(1), ..from_start() }).await.unwrap();
    consume(&mut b, "h", "other", 0, &Consumer { epoch: Some(1), ..from_start() }).await.unwrap();
    // non-epoch receivers share a partition
    let mut c = env.client().await;
    consume(&mut c, "h", "third", 0, &from_start()).await.unwrap();
    consume(&mut c, "h", "third", 0, &from_start()).await.unwrap();
}

#[tokio::test]
async fn a_sixth_receiver_without_an_epoch_is_refused() {
    let env = env().await;
    env.hub("h", 2);
    let mut c = env.client().await;
    let mut links = Vec::new();
    for _ in 0..5 {
        links.push(consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap());
    }
    let e = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap_err().unwrap();
    assert_eq!(e.condition, "amqp:resource-limit-exceeded");
    assert!(e.description.unwrap().contains("maximum number of allowed receivers"));
    // another partition and another group have their own five
    consume(&mut c, "h", "$Default", 1, &from_start()).await.unwrap();
    consume(&mut c, "h", "other", 0, &from_start()).await.unwrap();
    // closing one makes room
    c.detach(links[0]).await;
    consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    // an epoch receiver takes over all of them
    let epoch = consume(&mut c, "h", "$Default", 0, &Consumer { epoch: Some(1), ..from_start() }).await.unwrap();
    assert!(c.links[&epoch].detached.is_none());
}

#[tokio::test]
async fn closing_a_receiver_frees_the_partition_for_a_lower_epoch() {
    let env = env().await;
    env.hub("h", 1);
    let mut a = env.client().await;
    let mut b = env.client().await;
    let ra = consume(&mut a, "h", "$Default", 0, &Consumer { epoch: Some(5), ..from_start() }).await.unwrap();
    a.detach(ra).await;
    // the registration goes with the link
    consume(&mut b, "h", "$Default", 0, &Consumer { epoch: Some(1), ..from_start() }).await.unwrap();
    assert_eq!(env.shared.state.lock().receiver_count("myns", "h"), 1);
}

#[tokio::test]
async fn dropping_the_connection_unregisters_receivers() {
    let env = env().await;
    env.hub("h", 1);
    {
        let mut a = env.client().await;
        consume(&mut a, "h", "$Default", 0, &Consumer { epoch: Some(5), ..from_start() }).await.unwrap();
        assert_eq!(env.shared.state.lock().receiver_count("myns", "h"), 1);
    }
    for _ in 0..40 {
        if env.shared.state.lock().receiver_count("myns", "h") == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("receiver still registered after the connection dropped");
}

#[tokio::test]
async fn retention_trims_old_events_lazily() {
    let env = env().await;
    env.hub("h", 1);
    env.shared.state.lock().hub_mut("myns", "h").unwrap().retention_ms = 700;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/0").await.unwrap();
    send_one(&mut c, s, &event("old")).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    send_one(&mut c, s, &event("new")).await;
    // the old event is gone for a new reader; sequence numbers keep counting
    let r = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    let m = c.recv(r).await.message();
    assert_eq!(ann_i64(&m, "x-opt-sequence-number"), 1);
    assert_eq!(take_body(&m), "new");
    expect_quiet(&mut c, r).await;
    // everything expires: the partition is empty, its counters stay
    tokio::time::sleep(Duration::from_millis(900)).await;
    let links = mgmt_links(&mut c).await;
    let (status, _, body) =
        mgmt(&mut c, links, vec![("name", "h"), ("partition", "0"), ("operation", "READ"), ("type", "com.microsoft:partition")]).await;
    assert_eq!(status, 200);
    assert_eq!(map_get(&body, "is_partition_empty"), &Value::Bool(true));
    assert_eq!(map_get(&body, "last_enqueued_sequence_number").as_i64(), Some(1));
    assert_eq!(map_get(&body, "begin_sequence_number").as_i64(), Some(2));
}

fn take_body(m: &azemu_amqp::message::Message) -> String {
    match &m.body {
        azemu_amqp::message::Body::Data(p) => String::from_utf8_lossy(&p[0]).into_owned(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_receiver_whose_position_was_trimmed_starts_at_the_oldest_event() {
    let env = env().await;
    env.hub("h", 1);
    env.shared.state.lock().hub_mut("myns", "h").unwrap().retention_ms = 600;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/0").await.unwrap();
    for i in 0..3 {
        send_one(&mut c, s, &event(&format!("e{i}"))).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    send_one(&mut c, s, &event("fresh")).await;
    let r = consume(&mut c, "h", "$Default", 0, &with_filter("amqp.annotation.x-opt-sequence-number >= '1'")).await.unwrap();
    assert_eq!(take(&mut c, r, 1).await, vec!["fresh"]);
}
