mod common;
use azemu_amqp::message::{Message, Properties};
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::types::Value;
use azemu_servicebus::rules::{CorrelationFilter, Rule, RuleFilter};
use azemu_servicebus_sql::Params;
use common::*;

fn sql(name: &str, expr: &str) -> Rule {
    Rule::sql_filter(name, expr, Params::new(), 0).unwrap()
}

fn with_props(body_text: &str, props: Vec<(&str, Value)>, label: Option<&str>) -> Message {
    Message {
        properties: Some(Properties { subject: label.map(str::to_string), message_id: Some(Value::str("m")), ..Default::default() }),
        application_properties: Some(props.into_iter().map(|(k, v)| (Value::str(k), v)).collect()),
        ..msg(body_text)
    }
}

fn sub_count(e: &Env, topic: &str, sub: &str) -> usize {
    e.state(|st| st.ns("default").topic(topic).unwrap().subs[sub].core.counts().active)
}

#[tokio::test]
async fn every_subscription_gets_a_copy_and_the_default_rule_accepts_everything() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "s1", vec![Rule::default_rule(0)], |_| {});
    e.sub("t", "s2", vec![Rule::default_rule(0)], |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &msg("fan")).await;
    assert_eq!((sub_count(&e, "t", "s1"), sub_count(&e, "t", "s2")), (1, 1));
    for sub in ["s1", "s2"] {
        let r = receiver_rd(&mut c, &format!("t/Subscriptions/{sub}"), 5).await;
        assert_eq!(body(&c.recv(r).await.message()), "fan");
    }
}

#[tokio::test]
async fn sql_correlation_true_and_false_filters() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "red", vec![sql("r", "color = 'red' AND price > 10")], |_| {});
    e.sub("t", "bus", vec![sql("r", "sys.Label LIKE '%bus%'")], |_| {});
    e.sub(
        "t",
        "corr",
        vec![Rule {
            name: "c".into(),
            filter: RuleFilter::Correlation(CorrelationFilter {
                label: Some("vip".into()),
                properties: [("tier".to_string(), azemu_servicebus_sql::Value::Str("gold".into()))].into_iter().collect(),
                ..Default::default()
            }),
            action: None,
            created_ms: 0,
        }],
        |_| {},
    );
    e.sub("t", "yes", vec![Rule { name: "t".into(), filter: RuleFilter::True, action: None, created_ms: 0 }], |_| {});
    e.sub("t", "no", vec![Rule { name: "f".into(), filter: RuleFilter::False, action: None, created_ms: 0 }], |_| {});
    e.sub("t", "none", vec![], |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &with_props("1", vec![("color", Value::str("red")), ("price", Value::Int(11))], Some("servicebus"))).await;
    send(
        &mut c,
        s,
        &with_props("2", vec![("color", Value::str("red")), ("price", Value::Int(5)), ("tier", Value::str("gold"))], Some("vip")),
    )
    .await;
    let counts: Vec<usize> = ["red", "bus", "corr", "yes", "no", "none"].iter().map(|n| sub_count(&e, "t", n)).collect();
    assert_eq!(counts, [1, 1, 1, 2, 0, 0]);
}

#[tokio::test]
async fn rules_without_actions_make_one_copy_and_each_action_rule_its_own() {
    let e = env().await;
    e.topic("t");
    let plain1 = sql("p1", "1=1");
    let plain2 = sql("p2", "x = 1");
    let act1 = sql("a1", "x = 1").with_action("SET tag = 'one'", Params::new()).unwrap();
    let act2 = sql("a2", "1=1").with_action("SET tag = 'two'; REMOVE x", Params::new()).unwrap();
    e.sub("t", "s", vec![plain1, plain2, act1, act2], |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &with_props("m", vec![("x", Value::Int(1))], None)).await;
    assert_eq!(sub_count(&e, "t", "s"), 3);
    let r = receiver_rd(&mut c, "t/Subscriptions/s", 5).await;
    let mut tags = vec![];
    for _ in 0..3 {
        let m = c.recv(r).await.message();
        tags.push((app(&m, "tag").and_then(|v| v.as_str().map(str::to_string)), app(&m, "x").is_some()));
    }
    tags.sort();
    assert_eq!(tags, vec![(None, true), (Some("one".into()), true), (Some("two".into()), false)]);
}

#[tokio::test]
async fn subscription_dead_letter_queue_and_settlement() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "s", vec![Rule::default_rule(0)], |s| s.max_delivery_count = 1);
    let mut c = e.client().await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &msg("x")).await;
    let r = receiver(&mut c, "t/Subscriptions/s", 5).await;
    let d = c.recv(r).await;
    settle(&mut c, &d, DeliveryState::modified(true, false)).await;
    let dl = receiver_rd(&mut c, "t/Subscriptions/s/$DeadLetterQueue", 5).await;
    let m = c.recv(dl).await.message();
    assert_eq!(app(&m, "DeadLetterReason"), Some(Value::str("MaxDeliveryCountExceeded")));
    assert_eq!(ann(&m, "x-opt-deadletter-source"), None, "only set on dead letters that were forwarded");
}

#[tokio::test]
async fn scheduled_topic_message_fans_out_at_its_time() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "s", vec![Rule::default_rule(0)], |_| {});
    let mut c = e.client().await;
    let s = sender(&mut c, "t").await;
    let mut m = msg("later");
    m.message_annotations =
        Some(vec![(Value::sym("x-opt-scheduled-enqueue-time"), Value::Timestamp(azemu_servicebus::state::now_ms() + 300))]);
    send(&mut c, s, &m).await;
    assert_eq!(sub_count(&e, "t", "s"), 0);
    let r = receiver_rd(&mut c, "t/Subscriptions/s", 5).await;
    assert_eq!(body(&c.recv(r).await.message()), "later");
}

#[tokio::test]
async fn subscription_forwarding_to_a_queue() {
    let e = env().await;
    e.topic("t");
    e.queue("dest", |_| {});
    e.sub("t", "s", vec![Rule::default_rule(0)], |s| s.forward_to = Some("dest".into()));
    let mut c = e.client().await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &msg("x")).await;
    assert_eq!(e.core("dest", |c| c.counts().active), 1);
}

#[tokio::test]
async fn rules_over_management_add_enumerate_remove() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "s", vec![Rule::default_rule(0)], |_| {});
    let mut c = e.client().await;
    let mut mg = Mgmt::open(&mut c, "t/Subscriptions/s/$management").await;
    let sql_filter = Value::described_code(
        0x0000_0013_7000_0006,
        Value::List(vec![Value::str("color = 'blue'"), Value::Int(20), Value::Bool(false), Value::Null]),
    );
    let action = Value::described_code(
        0x0000_0137_0000_0006,
        Value::List(vec![Value::str("SET seen = 1"), Value::Int(20), Value::Bool(false), Value::Null]),
    );
    let desc = Value::described_code(0x0000_0137_0000_0004, Value::List(vec![sql_filter, action, Value::str("blue")]));
    let r = mg.call(&mut c, "com.microsoft:add-rule", vec![("rule-name", Value::str("blue")), ("rule-description", desc.clone())]).await;
    assert_eq!(r.status, 200, "{}", r.description);
    let r = mg.call(&mut c, "com.microsoft:add-rule", vec![("rule-name", Value::str("blue")), ("rule-description", desc)]).await;
    assert_eq!((r.status, r.condition.as_deref()), (409, Some("com.microsoft:entity-already-exists")));
    let r = mg.call(&mut c, "com.microsoft:enumerate-rules", vec![("top", Value::Int(10)), ("skip", Value::Int(0))]).await;
    let rules = r.body.map_get("rules").unwrap().as_list().unwrap();
    assert_eq!(rules.len(), 2);
    let names: Vec<String> = rules
        .iter()
        .map(|r| r.map_get("rule-description").unwrap().as_described().unwrap().value.as_list().unwrap()[2].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["$Default", "blue"]);
    let r = mg.call(&mut c, "com.microsoft:remove-rule", vec![("rule-name", Value::str("$Default"))]).await;
    assert_eq!(r.status, 200);
    let r = mg.call(&mut c, "com.microsoft:remove-rule", vec![("rule-name", Value::str("$Default"))]).await;
    assert_eq!(r.status, 404);
    // the new rule filters and its action runs
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &with_props("no", vec![("color", Value::str("red"))], None)).await;
    send(&mut c, s, &with_props("yes", vec![("color", Value::str("blue"))], None)).await;
    let rcv = receiver_rd(&mut c, "t/Subscriptions/s", 5).await;
    let m = c.recv(rcv).await.message();
    assert_eq!((body(&m), app(&m, "seen")), ("yes".to_string(), Some(Value::Long(1))));
}

#[tokio::test]
async fn topics_cannot_be_received_from_and_subscriptions_cannot_be_sent_to() {
    let e = env().await;
    e.topic("t");
    e.sub("t", "s", vec![Rule::default_rule(0)], |_| {});
    let mut c = e.client().await;
    assert!(c.attach_receiver("t", 1).await.is_err());
    assert!(c.attach_sender("t/Subscriptions/s").await.is_err());
}

#[tokio::test]
async fn action_copies_carry_the_rule_name() {
    let e = env().await;
    e.topic("t");
    e.sub(
        "t",
        "s",
        vec![
            sql("first", "1=1").with_action("SET a = 1", Params::new()).unwrap(),
            sql("second", "1=1").with_action("SET a = 2", Params::new()).unwrap(),
        ],
        |_| {},
    );
    let mut c = e.client().await;
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &msg("m")).await;
    let r = receiver_rd(&mut c, "t/Subscriptions/s", 5).await;
    let mut names = vec![];
    for _ in 0..2 {
        names.push(app(&c.recv(r).await.message(), "RuleName").unwrap().as_str().unwrap().to_string());
    }
    names.sort();
    assert_eq!(names, ["first", "second"]);
}

#[tokio::test]
async fn duplicate_detection_on_a_topic_and_on_a_forwarding_queue() {
    let e = env().await;
    e.state(|st| {
        let mut s = azemu_servicebus::model::Settings::topic_default(0);
        s.requires_duplicate_detection = true;
        st.ns("default").create_topic("dt", s).unwrap();
    });
    e.sub("dt", "s", vec![Rule::default_rule(0)], |_| {});
    e.queue("dest", |_| {});
    e.queue("src", |s| {
        s.requires_duplicate_detection = true;
        s.forward_to = Some("dest".into());
    });
    let mut c = e.client().await;
    let t = sender(&mut c, "dt").await;
    let q = sender(&mut c, "src").await;
    for _ in 0..3 {
        send(&mut c, t, &msg_id("x", "same")).await;
        send(&mut c, q, &msg_id("x", "same")).await;
    }
    assert_eq!(sub_count(&e, "dt", "s"), 1);
    assert_eq!(e.core("dest", |c| c.counts().active), 1);
}
