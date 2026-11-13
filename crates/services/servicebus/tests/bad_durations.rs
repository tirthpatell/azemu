//! A duration that is not ISO 8601 must be an error, not a silent default: over HTTP (400, what
//! the real service answers), in the config file (the file is refused with a message that names
//! the entity) and through `config::try_apply_properties` (used by the ARM crate).

mod admin_common;
use admin_common::*;
use azemu_servicebus::config::{self, apply_properties, try_apply_properties};
use azemu_servicebus::model::Settings;
use azemu_servicebus::state::State;
use serde_json::json;

// ---- HTTP management API --------------------------------------------------------------------

#[tokio::test]
async fn http_bad_durations_are_400_and_create_nothing() {
    let a = admin().await;
    let cases = [
        ("queue", "LockDuration"),
        ("queue", "DefaultMessageTimeToLive"),
        ("queue", "DuplicateDetectionHistoryTimeWindow"),
        ("queue", "AutoDeleteOnIdle"),
        ("topic", "DefaultMessageTimeToLive"),
        ("topic", "DuplicateDetectionHistoryTimeWindow"),
        ("topic", "AutoDeleteOnIdle"),
        ("sub", "LockDuration"),
        ("sub", "DefaultMessageTimeToLive"),
        ("sub", "AutoDeleteOnIdle"),
    ];
    for (i, (kind, prop)) in cases.iter().enumerate() {
        let name = format!("bad{i}");
        for bad in ["bogus", "10 minutes", "PT", "1M"] {
            let inner = format!("<{prop}>{bad}</{prop}>");
            let r = match *kind {
                "queue" => a.queue(&name, &inner).await,
                "topic" => a.topic(&name, &inner).await,
                _ => {
                    a.topic("parent", "").await;
                    a.sub("parent", &name, &inner).await
                }
            };
            assert_eq!(r.status, 400, "{kind} {prop}={bad}: {}", r.text());
            assert!(r.text().contains("<Code>400</Code>"), "{}", r.text());
        }
        let exists =
            a.shared.state.lock().ns("default").queue(&name).is_some() || a.shared.state.lock().ns("default").topic(&name).is_some();
        assert!(!exists, "{kind} {prop}: the entity was created in spite of the 400");
    }
}

// ---- config file ----------------------------------------------------------------------------

fn config_with(queue_props: serde_json::Value) -> config::Config {
    config::Config {
        raw: json!({"UserConfig": {"Namespaces": [{
            "Name": "ns1",
            "Queues": [
                {"Name": "good", "Properties": {"LockDuration": "PT30S"}},
                {"Name": "bad", "Properties": queue_props}
            ]
        }]}}),
    }
}

#[test]
fn a_config_file_with_a_bad_duration_is_refused_and_creates_nothing() {
    for (prop, value) in [
        ("LockDuration", "thirty seconds"),
        ("DefaultMessageTimeToLive", "1 hour"),
        ("DuplicateDetectionHistoryTimeWindow", "PT"),
        ("AutoDeleteOnIdle", "P"),
    ] {
        let cfg = config_with(json!({ prop: value }));
        let mut st = State::default();
        let err = config::apply(&mut st, &cfg, 0).expect_err(prop);
        assert!(err.contains("queue 'bad'") && err.contains(prop) && err.contains(value), "{err}");
        // the valid queue before it is not created either: the file is all or nothing
        assert!(st.ns("ns1").queue("good").is_none(), "{prop}: part of the file was applied");
    }
}

#[test]
fn a_config_file_with_good_durations_still_applies() {
    let cfg =
        config_with(json!({"LockDuration": "PT45S", "AutoDeleteOnIdle": "P1D", "DefaultMessageTimeToLive": "P10675199DT2H48M5.4775807S"}));
    let mut st = State::default();
    config::apply(&mut st, &cfg, 0).unwrap();
    let s = &st.ns("ns1").queue("bad").unwrap().core.settings;
    assert_eq!((s.lock_duration_ms, s.auto_delete_on_idle_ms, s.default_ttl_ms), (45_000, Some(86_400_000), None));
}

#[test]
fn a_bad_subscription_duration_names_the_subscription() {
    let cfg = config::Config {
        raw: json!({"UserConfig": {"Namespaces": [{"Name": "ns1", "Topics": [
            {"Name": "t", "Subscriptions": [{"Name": "s", "Properties": {"LockDuration": "soon"}}]}
        ]}]}}),
    };
    let err = config::apply(&mut State::default(), &cfg, 0).unwrap_err();
    assert!(err.contains("subscription 't/s'") && err.contains("LockDuration"), "{err}");
}

// ---- try_apply_properties (the ARM crate) ---------------------------------------------------

#[test]
fn try_apply_properties_reports_the_property_and_the_value() {
    let mut s = Settings::queue_default(0);
    let err = try_apply_properties(&mut s, &json!({"lockDuration": "abc"})).unwrap_err();
    assert_eq!(err.property, "lockDuration");
    assert!(err.message.contains("'abc'") && err.message.contains("lockDuration") && err.message.contains("ISO 8601"), "{}", err.message);
    // a number is no duration either
    assert!(try_apply_properties(&mut s, &json!({"defaultMessageTimeToLive": 60})).is_err());
    // JSON null means "not set"
    assert!(try_apply_properties(&mut s, &json!({"lockDuration": null, "autoDeleteOnIdle": null})).is_ok());
}

#[test]
fn try_apply_properties_applies_the_good_values() {
    let mut s = Settings::queue_default(0);
    try_apply_properties(
        &mut s,
        &json!({"lockDuration": "PT2M", "autoDeleteOnIdle": "PT10M", "duplicateDetectionHistoryTimeWindow": "PT1H", "maxDeliveryCount": 3}),
    )
    .unwrap();
    assert_eq!(
        (s.lock_duration_ms, s.auto_delete_on_idle_ms, s.duplicate_window_ms, s.max_delivery_count),
        (120_000, Some(600_000), 3_600_000, 3)
    );
}

#[test]
fn apply_properties_keeps_its_lenient_behaviour_for_existing_callers() {
    let mut s = Settings::queue_default(0);
    let before = s.lock_duration_ms;
    apply_properties(&mut s, &json!({"lockDuration": "abc", "maxDeliveryCount": 9}));
    assert_eq!(s.lock_duration_ms, before);
    assert_eq!(s.max_delivery_count, 9);
}

// ---- quota over HTTP ------------------------------------------------------------------------

#[tokio::test]
async fn http_create_over_the_entity_quota_is_403_not_409() {
    let a = admin().await;
    for i in 0..azemu_servicebus::state::MAX_ENTITIES_PER_NAMESPACE {
        a.shared.state.lock().ns("default").create_queue(&format!("q{i}"), Settings::queue_default(0)).unwrap();
    }
    let r = a.queue("one-more", "").await;
    assert_eq!(r.status, 403, "{}", r.text());
    assert!(r.text().contains("maximum number of queues and topics"), "{}", r.text());
    // an existing name is still a conflict
    assert_eq!(a.queue("q1", "").await.status, 409);
}
