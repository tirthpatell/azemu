//! The inspector of Service Bus (`/_azemu/inspect/servicebus/...`).

mod common;

use azemu_amqp::error::Error;
use azemu_amqp::message::Message;
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::types::Value;
use azemu_core::inspect::walk::walk;
use azemu_servicebus::core::Incoming;
use azemu_servicebus::rules::Rule;
use common::*;

async fn insp(e: &Env, p: &str) -> azemu_core::testing::TestResponse {
    e.srv.send("GET", &format!("{}/_azemu/inspect{p}", e.srv.gateway_url()), &[], "").await
}

fn with_props(body: &str) -> Message {
    let mut m = msg_id(body, &format!("id-{body}"));
    m.properties.as_mut().unwrap().content_type = Some("text/plain".into());
    m.application_properties = Some(vec![(Value::str("kind"), Value::str("order")), (Value::str("n"), Value::Int(3))]);
    m
}

/// Active, locked, dead-lettered, scheduled and deferred messages in queue `orders`, a session queue, and a topic with a subscription and a rule.
async fn fill(e: &Env) -> azemu_amqp::client::TestClient {
    e.queue("orders", |s| s.lock_duration_ms = 600_000);
    for i in 0..4 {
        e.inject("orders", with_props(&format!("m{i}")));
    }
    let mut later = with_props("later");
    later.message_annotations =
        Some(vec![(Value::sym("x-opt-scheduled-enqueue-time"), Value::Timestamp(azemu_servicebus::state::now_ms() + 3_600_000))]);
    e.inject("orders", later);
    e.inject("orders", Message::data(vec![0, 159, 146, 150]));
    let mut c = e.client().await;
    let r = receiver(&mut c, "orders", 2).await;
    // the first is dead-lettered with a reason, the second stays locked
    let (d, _) = recv_body(&mut c, r).await;
    let st = DeliveryState::rejected(
        Error::new("com.microsoft:dead-letter", "bad")
            .with_info("DeadLetterReason", Value::str("Poison"))
            .with_info("DeadLetterErrorDescription", Value::str("cannot parse")),
    );
    settle(&mut c, &d, st).await;
    let (_d2, _) = recv_body(&mut c, r).await;
    // a deferred one
    e.core("orders", |core| {
        let seq = *core.main.msgs.iter().find(|(_, m)| m.state == azemu_servicebus::store::MsgState::Ready).map(|(s, _)| s).unwrap();
        core.main.update(seq, |m| m.state = azemu_servicebus::store::MsgState::Deferred);
    });
    // sessions
    e.queue("sq", |s| s.requires_session = true);
    for s in ["A", "B"] {
        e.inject("sq", msg_session("s", s));
    }
    e.core("sq", |c| {
        c.sessions.entry("A".into()).or_default().state = Some(b"progress=2".to_vec());
    });
    // a topic with a subscription and a rule
    e.topic("events");
    e.sub("events", "all", vec![Rule::default_rule(0)], |_| {});
    e.sub("events", "big", vec![Rule::sql_filter("large", "n > 5", Default::default(), 0).unwrap()], |_| {});
    e.state(|st| {
        let sub = st.ns("default").topic_mut("events").unwrap().subs.get_mut("all").unwrap();
        let m = with_props("evt");
        let size = m.encode().len();
        sub.core.enqueue(Incoming::new(m, size), azemu_servicebus::state::now_ms()).unwrap();
    });
    c
}

#[tokio::test]
async fn the_whole_tree_can_be_walked_and_paging_ends() {
    let e = env().await;
    let _c = fill(&e).await;
    for limit in [1, 2, 50] {
        let nodes = walk(&e.srv, "/servicebus", limit).await;
        let kinds: Vec<&str> = nodes.iter().map(|n| n.node["kind"].as_str().unwrap()).collect();
        for k in [
            "service",
            "namespace",
            "queues",
            "queue",
            "messages",
            "topics",
            "topic",
            "subscriptions",
            "subscription",
            "rules",
            "sessions",
            "session",
        ] {
            assert!(kinds.contains(&k), "no {k} among {kinds:?}");
        }
    }
}

#[tokio::test]
async fn messages_of_each_state_show_system_and_application_properties_and_the_body() {
    let e = env().await;
    let _c = fill(&e).await;
    let q = insp(&e, "/servicebus/default/queues/orders").await.json();
    assert_eq!(q["properties"]["activeMessages"], 2);
    assert_eq!(q["properties"]["lockedMessages"], 1);
    assert_eq!(q["properties"]["deferredMessages"], 1);
    assert_eq!(q["properties"]["scheduledMessages"], 1);
    assert_eq!(q["properties"]["deadLetterMessages"], 1);
    assert_eq!(q["properties"]["lockDurationMs"], 600_000);
    let locked = insp(&e, "/servicebus/default/queues/orders/locked").await.json();
    let l = &locked["items"][0];
    assert_eq!(l["state"], "locked");
    assert!(l["lock"]["until"].is_string() && l["lock"]["heldByConnection"].is_number());
    assert!(!locked.to_string().to_lowercase().contains("token"), "a lock token is shown");
    let dead = insp(&e, "/servicebus/default/queues/orders/deadletter").await.json();
    assert_eq!(dead["items"][0]["deadLetterReason"], "Poison");
    assert_eq!(dead["items"][0]["deadLetterDescription"], "cannot parse");
    assert_eq!(dead["items"][0]["body"]["text"], "m0");
    let sched = insp(&e, "/servicebus/default/queues/orders/scheduled").await.json();
    assert_eq!(sched["items"][0]["body"]["text"], "later");
    assert!(sched["items"][0]["scheduledEnqueueTime"].is_string());
    let active = insp(&e, "/servicebus/default/queues/orders/active").await.json();
    let rows = active["items"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["messageId"], "id-m3");
    assert_eq!(rows[0]["contentType"], "text/plain");
    assert_eq!(rows[0]["applicationProperties"]["kind"], "order");
    assert_eq!(rows[0]["applicationProperties"]["n"], 3);
    assert!(rows[1]["body"].get("base64").is_some(), "a binary body is base64: {}", rows[1]);
    // sessions and rules
    let s = insp(&e, "/servicebus/default/queues/sq/sessions").await.json();
    assert_eq!(s["items"].as_array().unwrap().len(), 2);
    let a = insp(&e, "/servicebus/default/queues/sq/sessions/A").await.json();
    assert_eq!(a["content"]["text"], "progress=2");
    let rules = insp(&e, "/servicebus/default/topics/events/subscriptions/big/rules").await.json();
    assert_eq!(rules["items"][0]["name"], "large");
    assert_eq!(rules["items"][0]["filter"]["expression"], "n > 5");
    let sub = insp(&e, "/servicebus/default/topics/events/subscriptions/all").await.json();
    assert_eq!(sub["properties"]["activeMessages"], 1);
    // the rule keys are not shown
    assert!(!insp(&e, "/servicebus/default").await.text().contains("primaryKey"));
    assert!(insp(&e, "/servicebus/default").await.json()["properties"]["authorizationRules"].is_array());
}

#[tokio::test]
async fn looking_changes_nothing_a_receiver_can_see() {
    let e = env().await;
    let _c = fill(&e).await;
    let view = || {
        e.state(|st| {
            let q = &st.ns("default").queue_mut("orders").unwrap().core;
            let msgs: Vec<_> = q
                .main
                .msgs
                .values()
                .map(|m| (m.seq, format!("{:?}", m.state), m.delivery_count, m.lock.as_ref().map(|l| (l.until_ms, l.token))))
                .collect();
            (msgs, q.main.next_seq(), q.main.rev, q.dlq.len(), q.dlq.rev)
        })
    };
    let before = view();
    walk(&e.srv, "", 1).await;
    walk(&e.srv, "/servicebus", 50).await;
    assert_eq!(before, view());
    // a message that was locked is still locked for its receiver
    for p in [
        "/servicebus/ghost",
        "/servicebus/default/queues/none",
        "/servicebus/default/queues/orders/other",
        "/servicebus/default/topics/events/subscriptions/none",
    ] {
        assert_eq!(insp(&e, p).await.status, 404, "{p}");
    }
    assert!(e.state(|st| !st.namespaces.contains_key("ghost")), "looking made a namespace");
}
