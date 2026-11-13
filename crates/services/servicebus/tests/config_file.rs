mod common;
use azemu_amqp::types::Value;
use azemu_servicebus::Shared;
use common::*;

const CONFIG: &str = r#"{
  "UserConfig": {
    "Namespaces": [
      {
        "Name": "sbemulatorns",
        "Queues": [
          { "Name": "queue.1", "Properties": { "DeadLetteringOnMessageExpiration": true, "DefaultMessageTimeToLive": "PT1H", "DuplicateDetectionHistoryTimeWindow": "PT20S", "ForwardDeadLetteredMessagesTo": "", "ForwardTo": "", "LockDuration": "PT30S", "MaxDeliveryCount": 4, "RequiresDuplicateDetection": true, "RequiresSession": false } },
          { "Name": "session.queue", "Properties": { "RequiresSession": true } }
        ],
        "Topics": [
          {
            "Name": "topic.1",
            "Properties": { "DefaultMessageTimeToLive": "PT1H", "RequiresDuplicateDetection": false },
            "Subscriptions": [
              { "Name": "subscription.1", "Properties": { "MaxDeliveryCount": 7, "LockDuration": "PT1M" },
                "Rules": [
                  { "Name": "corr", "Properties": { "FilterType": "Correlation", "CorrelationFilter": { "Label": "subject1", "CorrelationId": "id1", "Properties": { "prop3": "value3" } } } },
                  { "Name": "sql", "Properties": { "FilterType": "Sql", "SqlFilter": { "SqlExpression": "color = 'red'" }, "Action": { "SqlExpression": "SET seen = 1" } } }
                ] },
              { "Name": "subscription.2", "Properties": {} }
            ]
          }
        ]
      }
    ],
    "Logging": { "Type": "File" }
  }
}"#;

fn write_config(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("azemu-sb-{}-{}.json", std::process::id(), name));
    std::fs::write(&p, CONFIG).unwrap();
    p
}

#[tokio::test]
async fn config_creates_entities_with_their_properties() {
    let path = write_config("create");
    let shared = Shared::new(true, Some(path.clone()));
    shared.load_config();
    let e = env_with(shared).await;
    e.state(|st| {
        let ns = st.ns("sbemulatorns");
        let q = &ns.queue("queue.1").unwrap().core.settings;
        assert_eq!(
            (q.lock_duration_ms, q.max_delivery_count, q.default_ttl_ms, q.duplicate_window_ms),
            (30_000, 4, Some(3_600_000), 20_000)
        );
        assert!(q.dead_letter_on_expiry && q.requires_duplicate_detection && q.forward_to.is_none());
        assert!(ns.queue("session.queue").unwrap().core.settings.requires_session);
        let t = ns.topic("topic.1").unwrap();
        let s = &t.subs["subscription.1"];
        assert_eq!(s.core.settings.max_delivery_count, 7);
        assert_eq!(s.rules.len(), 2);
        assert_eq!(t.subs["subscription.2"].rules[0].name, "$Default", "a subscription without rules gets $Default");
    });
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn a_single_configured_namespace_answers_to_localhost_too() {
    let path = write_config("alias");
    let shared = Shared::new(true, Some(path.clone()));
    shared.load_config();
    let e = env_with(shared).await;
    // sb://localhost -> namespace "default" -> the configured namespace
    let mut c = e.client_for("localhost").await;
    let s = sender(&mut c, "queue.1").await;
    send(&mut c, s, &msg_id("x", "1")).await;
    assert_eq!(e.state(|st| st.ns("sbemulatorns").queue("queue.1").unwrap().core.counts().active), 1);
    // by its own name as well
    let mut c2 = e.client_for("sbemulatorns.servicebus.localhost").await;
    let s2 = sender(&mut c2, "queue.1").await;
    send(&mut c2, s2, &msg_id("y", "2")).await;
    assert_eq!(e.state(|st| st.ns("sbemulatorns").queue("queue.1").unwrap().core.counts().active), 2);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn reset_reloads_the_config_file() {
    let path = write_config("reset");
    let shared = Shared::new(true, Some(path.clone()));
    shared.load_config();
    let e = env_with(shared).await;
    e.state(|st| {
        st.ns("sbemulatorns").create_queue("extra", azemu_servicebus::model::Settings::queue_default(0)).unwrap();
    });
    e.inject_ns("sbemulatorns", "queue.1");
    e.srv.reset();
    e.state(|st| {
        let ns = st.ns("sbemulatorns");
        assert!(ns.queue("extra").is_none(), "state is dropped");
        assert_eq!(ns.queue("queue.1").unwrap().core.counts().active, 0, "messages are gone");
        assert!(ns.topic("topic.1").is_some(), "the configured entities are back");
    });
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn configured_rules_filter_and_act() {
    let path = write_config("rules");
    let shared = Shared::new(true, Some(path.clone()));
    shared.load_config();
    let e = env_with(shared).await;
    let mut c = e.client_for("localhost").await;
    let s = sender(&mut c, "topic.1").await;
    let mut m = msg("red");
    m.application_properties = Some(vec![(Value::str("color"), Value::str("red"))]);
    send(&mut c, s, &m).await;
    let n = e.state(|st| st.ns("sbemulatorns").topic("topic.1").unwrap().subs["subscription.1"].core.counts().active);
    assert_eq!(n, 1, "the sql rule matched, the correlation rule did not");
    let r = receiver_rd(&mut c, "topic.1/Subscriptions/subscription.1", 2).await;
    assert_eq!(app(&c.recv(r).await.message(), "seen"), Some(Value::Long(1)));
    let _ = std::fs::remove_file(path);
}

#[test]
fn bad_config_is_reported_not_fatal() {
    let shared = Shared::new(true, Some("/definitely/not/there.json".into()));
    shared.load_config(); // logs a warning, does not panic
    assert!(azemu_servicebus::config::parse_str("{ not json").is_err());
}

#[test]
fn forward_targets_are_normalised() {
    assert_eq!(azemu_servicebus::config::normalize_path("sb://h.servicebus.windows.net/dest"), "dest");
    assert_eq!(azemu_servicebus::config::normalize_path("dest"), "dest");
}

/// The short example of `docs/services/servicebus.md` ("Entities") works as written.
#[tokio::test]
async fn the_example_of_the_docs_creates_a_queue_and_a_topic_with_a_subscription() {
    let doc = r#"{"UserConfig": {"Namespaces": [{"Name": "default",
        "Queues": [{"Name": "orders", "Properties": {"LockDuration": "PT30S"}}],
        "Topics": [{"Name": "events", "Subscriptions": [{"Name": "all"}]}]}]}}"#;
    let p = std::env::temp_dir().join(format!("azemu-sb-{}-docs-example.json", std::process::id()));
    std::fs::write(&p, doc).unwrap();
    let shared = Shared::new(false, Some(p.clone()));
    shared.load_config();
    let e = env_with(shared).await;
    e.state(|st| {
        let ns = &st.namespaces["default"];
        assert_eq!(ns.queues["orders"].core.settings.lock_duration_ms, 30_000);
        assert!(ns.topics["events"].subs.contains_key("all"));
    });
    let _ = std::fs::remove_file(p);
}
