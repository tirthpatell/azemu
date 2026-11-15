//! Behaviour that was checked against a real Standard namespace in November 2026 (each test says what Azure answered).
mod common;
use std::time::Duration;

use azemu_amqp::error::Error;
use azemu_amqp::message::{Message, Properties};
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::types::Value;
use azemu_servicebus::rules::Rule;
use azemu_servicebus_sql::Params;
use common::*;

fn sql(name: &str, expr: &str) -> Rule {
    Rule::sql_filter(name, expr, Params::new(), 0).unwrap()
}

fn with_props(text: &str, props: Vec<(&str, Value)>) -> Message {
    Message {
        properties: Some(Properties { message_id: Some(Value::str(text)), ..Default::default() }),
        application_properties: Some(props.into_iter().map(|(k, v)| (Value::str(k), v)).collect()),
        ..msg(text)
    }
}

async fn put_token(c: &mut azemu_amqp::client::TestClient, resp: u32, req: u32, audience: &str) -> Message {
    let m = Message {
        properties: Some(Properties { message_id: Some(Value::Uuid([7; 16])), reply_to: Some("$cbs".into()), ..Default::default() }),
        application_properties: Some(vec![
            (Value::str("operation"), Value::str("put-token")),
            (Value::str("type"), Value::str("jwt")),
            (Value::str("name"), Value::str(audience)),
        ]),
        body: azemu_amqp::message::Body::Value(Value::str("token")),
        ..Default::default()
    };
    send(c, req, &m).await;
    c.recv(resp).await.message()
}

fn status(m: &Message) -> i64 {
    m.app_property("status-code").and_then(|v| v.as_i64()).unwrap()
}

// ---- finding 1: the token of an entity that does not exist ----

#[tokio::test]
async fn a_token_for_an_entity_that_is_not_there_is_refused_not_found() {
    let e = env_with(azemu_servicebus::Shared::new(false, None)).await;
    e.queue("q", |_| {});
    e.topic("t");
    e.sub("t", "s", vec![Rule::default_rule(0)], |_| {});
    let mut c = e.client().await;
    let resp = receiver_rd(&mut c, "$cbs", 10).await;
    let req = sender(&mut c, "$cbs").await;
    // what exists gets 202: a queue, its dead-letter queue, a topic, a subscription, and a subscription that is missing under a topic
    // that is there (its attach is refused instead)
    for ok in [
        "sb://default.servicebus.windows.net/q",
        "sb://default.servicebus.windows.net/q/$DeadLetterQueue",
        "sb://default.servicebus.windows.net/Q/$management",
        "sb://default.servicebus.windows.net/t",
        "sb://default.servicebus.windows.net/t/Subscriptions/s",
        "sb://default.servicebus.windows.net/t/Subscriptions/not-there",
    ] {
        assert_eq!(status(&put_token(&mut c, resp, req, ok).await), 202, "{ok}");
    }
    for missing in [
        "sb://default.servicebus.windows.net/nope",
        "sb://default.servicebus.windows.net/nope/$DeadLetterQueue",
        "sb://default.servicebus.windows.net/no-topic/Subscriptions/s",
        "sb://default.servicebus.windows.net/no-topic/Subscriptions/s/$DeadLetterQueue",
    ] {
        let reply = put_token(&mut c, resp, req, missing).await;
        assert_eq!(status(&reply), 404, "{missing}");
        assert_eq!(reply.app_property("error-condition"), Some(&Value::Symbol("amqp:not-found".into())), "{missing}");
        let text = reply.app_property("status-description").and_then(|v| v.as_str()).unwrap().to_string();
        assert!(text.starts_with(&format!("The messaging entity '{missing}' could not be found. To know more visit https://aka.ms/sbResourceMgrExceptions.  TrackingId:")), "{text}");
    }
}

#[tokio::test]
async fn with_auto_creation_the_token_of_a_missing_entity_is_accepted() {
    let e = env().await;
    let mut c = e.client().await;
    let resp = receiver_rd(&mut c, "$cbs", 10).await;
    let req = sender(&mut c, "$cbs").await;
    assert_eq!(status(&put_token(&mut c, resp, req, "sb://default.servicebus.windows.net/made-by-the-attach").await), 202);
}

// ---- finding 2: a receiver on an entity that forwards ----

#[tokio::test]
async fn a_receiver_on_an_entity_with_forwarding_is_refused() {
    let e = env_with(azemu_servicebus::Shared::new(false, None)).await;
    e.queue("dest", |_| {});
    e.queue("src", |s| s.forward_to = Some("dest".into()));
    e.queue("dlsrc", |s| s.forward_dead_lettered_to = Some("dest".into()));
    e.topic("t");
    e.sub("t", "fwd", vec![Rule::default_rule(0)], |s| s.forward_to = Some("dest".into()));
    let mut c = e.client().await;
    for address in ["src", "t/Subscriptions/fwd", "dlsrc/$DeadLetterQueue"] {
        let err = c.attach_receiver(address, 1).await.unwrap_err().unwrap();
        assert_eq!(err.condition, "amqp:not-allowed", "{address}");
        assert!(
            err.description.unwrap().starts_with("Cannot create a message receiver on an entity with auto-forwarding enabled. TrackingId:"),
            "{address}"
        );
    }
    // the main queue of an entity that only forwards its dead letters is a plain queue; the dead-letter queue of one that forwards
    // its messages is too
    c.attach_receiver("dlsrc", 1).await.unwrap();
    c.attach_receiver("src/$DeadLetterQueue", 1).await.unwrap();
}

// ---- finding 3: forwarding ----

#[tokio::test]
async fn a_message_forwarded_between_queues_has_no_enqueue_sequence_number_but_one_from_a_topic_has() {
    let e = env().await;
    e.queue("dest", |_| {});
    e.queue("src", |s| s.forward_to = Some("dest".into()));
    e.queue("direct", |_| {});
    e.topic("t");
    e.sub("t", "plain", vec![Rule::default_rule(0)], |_| {});
    e.sub("t", "fwd", vec![Rule::default_rule(0)], |s| s.forward_to = Some("dest".into()));
    let mut c = e.client().await;
    for target in ["src", "direct"] {
        let s = sender(&mut c, target).await;
        send(&mut c, s, &msg(target)).await;
    }
    let dest = receiver_rd(&mut c, "dest", 5).await;
    assert_eq!(ann(&c.recv(dest).await.message(), "x-opt-enqueue-sequence-number"), None, "queue to queue");
    let r = receiver_rd(&mut c, "direct", 5).await;
    assert_eq!(ann(&c.recv(r).await.message(), "x-opt-enqueue-sequence-number"), None, "sent straight to the queue");
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &msg("t1")).await;
    send(&mut c, s, &msg("t2")).await;
    let r = receiver_rd(&mut c, "t/Subscriptions/plain", 5).await;
    let (first, second) = (c.recv(r).await.message(), c.recv(r).await.message());
    assert_eq!(ann(&first, "x-opt-enqueue-sequence-number"), Some(Value::Long(1)));
    assert_eq!(ann(&second, "x-opt-enqueue-sequence-number"), Some(Value::Long(2)));
    // the copy that the subscription forwards to the queue has the topic's number too
    let m = c.recv(dest).await.message();
    assert_eq!(ann(&m, "x-opt-enqueue-sequence-number"), Some(Value::Long(1)));
}

#[tokio::test]
async fn the_source_of_a_dead_letter_of_a_subscription_is_topic_subscriptions_sub() {
    let e = env().await;
    e.queue("dest", |_| {});
    e.topic("t");
    e.sub("t", "s", vec![Rule::default_rule(0)], |s| s.forward_dead_lettered_to = Some("dest".into()));
    let mut c = e.client().await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &msg("poison")).await;
    let r = receiver(&mut c, "t/Subscriptions/s", 1).await;
    let d = c.recv(r).await;
    let dl = Error::new("com.microsoft:dead-letter", "").with_info("DeadLetterReason", Value::str("r"));
    settle(&mut c, &d, DeliveryState::rejected(dl)).await;
    let r = receiver_rd(&mut c, "dest", 1).await;
    let m = c.recv(r).await.message();
    assert_eq!(ann(&m, "x-opt-deadletter-source"), Some(Value::str("t/Subscriptions/s")));
}

// ---- finding 4: a rule that fails when it is evaluated ----

#[tokio::test]
async fn a_filter_that_cannot_be_evaluated_dead_letters_the_message_in_the_subscription() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "dl", vec![sql("r", "n > 5")], |_| {});
    e.sub("t", "drop", vec![sql("r", "n > 5")], |s| s.dead_letter_on_filter_exception = false);
    e.sub("t", "act", vec![Rule::default_rule(0).with_action("SET m = n - 1", Params::new()).unwrap()], |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &with_props("number", vec![("n", Value::Long(10))])).await;
    send(&mut c, s, &with_props("text", vec![("n", Value::str("abc"))])).await;
    send(&mut c, s, &with_props("none", vec![])).await;
    let seen = |sub: &str| e.state(|st| st.ns("default").topic("t").unwrap().subs[sub].core.counts().clone_counts());
    // `dl`: the number is delivered, the string is dead-lettered, the missing property is no match and no error
    assert_eq!(seen("dl"), (1, 1));
    // `drop`: the failing message is dropped (nothing is dead-lettered)
    assert_eq!(seen("drop"), (1, 0));
    // the action fails for the string only, and for the missing property it stores null
    assert_eq!(seen("act"), (2, 1));
    let r = receiver_rd(&mut c, "t/Subscriptions/dl/$DeadLetterQueue", 5).await;
    let m = c.recv(r).await.message();
    assert_eq!(body(&m), "text");
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("FilterException")));
    assert_eq!(
        app(&m, "DeadLetterErrorDescription"),
        Some(Value::str("Operator '>' cannot be applied to operands of type 'string' and 'long'"))
    );
    let r = receiver_rd(&mut c, "t/Subscriptions/act/$DeadLetterQueue", 5).await;
    let m = c.recv(r).await.message();
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("RuleActionException")));
    assert_eq!(
        app(&m, "DeadLetterErrorDescription"),
        Some(Value::str("Operator '-' cannot be applied to operands of type 'string' and 'long'"))
    );
}

trait CountsExt {
    fn clone_counts(&self) -> (usize, usize);
}
impl CountsExt for azemu_servicebus::core::Counts {
    fn clone_counts(&self) -> (usize, usize) {
        (self.active, self.dead_letter)
    }
}

// ---- finding 5: scheduled messages ----

fn scheduled(text: &str, in_ms: i64) -> Message {
    let mut m = msg_id(text, text);
    m.message_annotations =
        Some(vec![(Value::sym("x-opt-scheduled-enqueue-time"), Value::Timestamp(azemu_servicebus::state::now_ms() + in_ms))]);
    m
}

#[tokio::test]
async fn a_scheduled_message_that_arrived_is_still_in_the_state_scheduled() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    send(&mut c, s, &scheduled("later", 300)).await;
    send(&mut c, s, &msg("now")).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let r = receiver(&mut c, "q", 5).await;
    let first = c.recv(r).await.message();
    let second = c.recv(r).await.message();
    let state = |m: &Message| ann(m, "x-opt-message-state");
    // the message that was sent first is in the queue first (the scheduled one got its number when it was sent)
    assert_eq!((body(&first), state(&first)), ("later".to_string(), Some(Value::Int(2))));
    assert_eq!((body(&second), state(&second)), ("now".to_string(), Some(Value::Int(0))));
}

#[tokio::test]
async fn peek_stops_before_a_scheduled_message_unless_it_starts_at_one() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    send(&mut c, s, &msg("now1")).await;
    send(&mut c, s, &scheduled("later1", 120_000)).await;
    send(&mut c, s, &msg("now2")).await;
    send(&mut c, s, &scheduled("later2", 120_000)).await;
    send(&mut c, s, &msg("now3")).await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let mut peek = async |c: &mut azemu_amqp::client::TestClient, from: i64| -> Vec<String> {
        let reply = mg
            .call(c, "com.microsoft:peek-message", vec![("from-sequence-number", Value::Long(from)), ("message-count", Value::Int(10))])
            .await;
        match reply.body.map_get("messages").and_then(|v| v.as_list()) {
            Some(l) => l.iter().map(|m| body(&Message::decode(m.map_get("message").unwrap().as_binary().unwrap()).unwrap())).collect(),
            None => vec![],
        }
    };
    // Azure (checked with a queue of the same shape): from 1 only "now1", from 2 everything after, from 3 only "now2"
    assert_eq!(peek(&mut c, 1).await, ["now1"]);
    assert_eq!(peek(&mut c, 2).await, ["later1", "now2", "later2", "now3"]);
    assert_eq!(peek(&mut c, 3).await, ["now2"]);
    assert_eq!(peek(&mut c, 4).await, ["later2", "now3"]);
}

#[tokio::test]
async fn a_scheduled_duplicate_is_dropped_but_uses_up_a_sequence_number() {
    let e = env().await;
    e.queue("q", |s| s.requires_duplicate_detection = true);
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let mut schedule = async |c: &mut azemu_amqp::client::TestClient, id: &str| -> i64 {
        let m = scheduled(id, 60_000);
        let item = Value::Map(vec![(Value::str("message-id"), Value::str(id)), (Value::str("message"), Value::Binary(m.encode()))]);
        let reply = mg.call(c, "com.microsoft:schedule-message", vec![("messages", Value::List(vec![item]))]).await;
        reply.body.map_get("sequence-numbers").unwrap().as_list().unwrap()[0].as_i64().unwrap()
    };
    assert_eq!(schedule(&mut c, "same").await, 1);
    assert_eq!(schedule(&mut c, "same").await, 2, "a real number for the duplicate");
    assert_eq!(schedule(&mut c, "other").await, 3);
    assert_eq!(e.core("q", |c| c.counts().scheduled), 2);
}

// ---- finding 6: dead letters, counts, deferred messages ----

#[tokio::test]
async fn a_dead_letter_without_a_reason_has_no_reason_and_no_description() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 1).await;
    let d = c.recv(r).await;
    // the Python SDK: the dead-letter condition, null reason and null description
    let dl = Error::new("com.microsoft:dead-letter", "")
        .with_info("DeadLetterReason", Value::Null)
        .with_info("DeadLetterErrorDescription", Value::Null);
    settle(&mut c, &d, DeliveryState::rejected(dl)).await;
    let r = receiver_rd(&mut c, "q/$DeadLetterQueue", 1).await;
    let m = c.recv(r).await.message();
    assert_eq!(app(&m, "DeadLetterReason"), None);
    assert_eq!(app(&m, "DeadLetterErrorDescription"), None);
}

#[tokio::test]
async fn a_message_abandoned_in_the_dead_letter_queue_keeps_its_delivery_count() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("x"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 5).await;
    let d = c.recv(r).await;
    settle(&mut c, &d, DeliveryState::modified(true, false)).await; // one abandon: count 1
    let d = c.recv(r).await;
    settle(&mut c, &d, DeliveryState::rejected(Error::new("com.microsoft:dead-letter", ""))).await;
    let dlq = receiver(&mut c, "q/$DeadLetterQueue", 5).await;
    for _ in 0..3 {
        let (d, m) = recv_body(&mut c, dlq).await;
        assert_eq!(m.header.unwrap().delivery_count, Some(1));
        settle(&mut c, &d, DeliveryState::modified(true, false)).await;
    }
}

#[tokio::test]
async fn a_deferred_message_counts_as_delivered_and_its_errors_are_the_services() {
    let e = env().await;
    e.queue("q", |_| {});
    e.inject("q", msg("a"));
    e.inject("q", msg("b"));
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 2).await;
    let (da, _) = recv_body(&mut c, r).await;
    let (db, _) = recv_body(&mut c, r).await;
    settle(&mut c, &da, DeliveryState::modified(false, true)).await; // defer a
    settle(&mut c, &db, DeliveryState::Accepted).await; // complete b
    let mut mg = Mgmt::open(&mut c, "q/$management").await;
    let mut deferred = async |c: &mut azemu_amqp::client::TestClient, seq: i64| {
        mg.call(
            c,
            "com.microsoft:receive-by-sequence-number",
            vec![("sequence-numbers", Value::Array(vec![Value::Long(seq)])), ("receiver-settle-mode", Value::Uint(1))],
        )
        .await
    };
    let reply = deferred(&mut c, 1).await;
    assert_eq!(reply.status, 200);
    let m = Message::decode(reply.body.map_get("messages").unwrap().as_list().unwrap()[0].map_get("message").unwrap().as_binary().unwrap())
        .unwrap();
    assert_eq!(m.header.unwrap().delivery_count, Some(1), "a deferred message was delivered once");
    for (seq, text) in [
        (1, "Failed to lock one or more specified messages. Either the message is already locked or does not exist."),
        (2, "Failed to lock one or more specified messages as they were not deferred messages."),
        (999, "Messages not found. One or more sequence numbers missing."),
    ] {
        let reply = deferred(&mut c, seq).await;
        assert_eq!((reply.status, reply.condition.as_deref()), (500, Some("com.microsoft:message-not-found")), "{seq}");
        assert!(reply.description.starts_with(text), "{seq}: {}", reply.description);
    }
}

#[tokio::test]
async fn a_message_with_a_time_to_live_of_zero_is_refused() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    let m = Message { header: Some(azemu_amqp::message::Header { ttl: Some(0), ..Default::default() }), ..msg("x") };
    match c.send_message_wait(s, &m).await {
        Some(DeliveryState::Rejected { error: Some(err) }) => {
            assert_eq!(err.condition, "com.microsoft:argument-out-of-range");
            assert!(err
                .description
                .unwrap()
                .starts_with("Argument TimeToLIve must be a positive timeout value. The provided value was 00:00:00."));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(e.core("q", |c| c.counts().active), 0);
}

// ---- finding 7: application property names ----

#[tokio::test]
async fn application_property_names_are_not_case_sensitive() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "q").await;
    let m = with_props("p", vec![("Key", Value::Long(1)), ("key", Value::Long(2)), ("KEY", Value::Long(3)), ("other", Value::str("x"))]);
    send(&mut c, s, &m).await;
    let r = receiver_rd(&mut c, "q", 1).await;
    let got = c.recv(r).await.message();
    let props = got.application_properties.unwrap();
    assert_eq!(props.len(), 2);
    assert_eq!((props[0].0.as_str(), props[0].1.as_i64()), (Some("Key"), Some(3)), "the first spelling, the last value");
    assert_eq!(props[1].0.as_str(), Some("other"));
}

// ---- finding 9, 10: texts ----

#[tokio::test]
async fn session_and_disabled_texts_are_the_services() {
    let e = env().await;
    e.queue("plain", |_| {});
    e.queue("sq", |s| s.requires_session = true);
    e.queue("off", |s| s.status = "Disabled".into());
    e.topic("t");
    e.sub("t", "s", vec![Rule::default_rule(0)], |s| s.status = "Disabled".into());
    let mut c = e.client().await;
    let s = sender(&mut c, "sq").await;
    match c.send_message_wait(s, &msg("x")).await {
        Some(DeliveryState::Rejected { error: Some(err) }) => {
            assert_eq!(err.condition, "amqp:not-allowed");
            assert!(err.description.unwrap().starts_with("The SessionId was not set on a message, and it cannot be sent to the entity."));
        }
        other => panic!("{other:?}"),
    }
    let mut source = azemu_amqp::terminus::Source::with_address("plain");
    source.filter = Some(Value::Map(vec![(Value::sym("com.microsoft:session-filter"), Value::str("x"))]));
    let err = c
        .attach_receiver_source(
            source,
            azemu_amqp::performatives::SndSettleMode::Unsettled,
            azemu_amqp::performatives::RcvSettleMode::Second,
            1,
        )
        .await
        .unwrap_err()
        .unwrap();
    assert_eq!(err.condition, "amqp:not-allowed");
    assert!(err
        .description
        .unwrap()
        .starts_with("A sessionful message receiver cannot be created on an entity that does not require sessions."));
    let err = c.attach_receiver("off", 1).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "com.microsoft:entity-disabled");
    assert!(
        err.description.unwrap().starts_with("Messaging entity 'sb://default.servicebus.windows.net/off' is currently disabled. For more information please see https://aka.ms/ServiceBusExceptions . TrackingId:")
    );
    let err = c.attach_receiver("t/Subscriptions/s", 1).await.unwrap_err().unwrap();
    assert!(err.description.unwrap().starts_with("Messaging entity 'default:Topic:t|s' is currently disabled."));
}
