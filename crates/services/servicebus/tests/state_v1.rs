//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/servicebus/` holds the files `Service::save` wrote for the small fixed state built by `fixed_state`
//! (`AZEMU-STATE servicebus 1` and `AZEMU-STATE servicebus-entity 1`). The files are checked in and are never rewritten
//! by the test run. `version_1_files_still_load` loads them into a new service and checks what they hold.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in files
//! and do not weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (and read it into the
//! same state this test expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-servicebus --test state_v1 -- --ignored write_the_fixture`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_amqp::message::{Message, Properties};
use azemu_amqp::types::Value;
use azemu_core::Service;
use azemu_servicebus::core::Incoming;
use azemu_servicebus::model::Settings;
use azemu_servicebus::rules::Rule;
use azemu_servicebus::state::Shared;
use azemu_servicebus::ServiceBusService;

const NOW: i64 = 1_700_000_000_000;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("servicebus")
}

fn message(body: &str, id: &str) -> Message {
    Message {
        properties: Some(Properties { message_id: Some(Value::str(id)), ..Default::default() }),
        ..Message::data(body.as_bytes().to_vec())
    }
}

fn fixed_state() -> (Arc<ServiceBusService>, Arc<Shared>) {
    let shared = Shared::new(false, None);
    let (svc, _amqp) = azemu_servicebus::pair_with(shared.clone());
    {
        let mut st = shared.state.lock();
        let ns = st.ns("fixture");
        let mut q = Settings::queue_default(NOW);
        q.lock_duration_ms = 45_000;
        q.max_delivery_count = 5;
        q.requires_duplicate_detection = true;
        q.user_metadata = Some("kept".into());
        ns.create_queue("orders", q).unwrap();
        ns.create_queue("plain", Settings::queue_default(NOW)).unwrap();
        ns.create_topic("events", Settings::topic_default(NOW)).unwrap();
        let sql = Rule::sql_filter("big", "amount > 10", Default::default(), NOW).unwrap();
        ns.create_subscription("events", "all", Settings::subscription_default(NOW), vec![Rule::default_rule(NOW)]).unwrap();
        ns.create_subscription("events", "filtered", Settings::subscription_default(NOW), vec![sql]).unwrap();
        for (body, id) in [("first", "id-1"), ("second", "id-2")] {
            let m = message(body, id);
            let size = m.encode().len();
            ns.queue_mut("orders").unwrap().core.enqueue(Incoming::new(m, size), NOW).unwrap();
        }
    }
    (svc, shared)
}

#[test]
#[ignore = "writes the checked-in fixture; see the module text"]
fn write_the_fixture() {
    let (svc, _shared) = fixed_state();
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    svc.save(&dir).unwrap();
}

#[test]
fn version_1_files_still_load() {
    let dir = std::env::temp_dir().join(format!("azemu-sb-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut copied = 0;
    for entry in std::fs::read_dir(fixture_dir()).expect("tests/state-v1/servicebus is checked in") {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
        copied += 1;
    }
    assert!(copied >= 4, "the fixture has the definitions file and one file per entity ({copied} files found)");

    let shared = Shared::new(false, None);
    let (svc, _amqp) = azemu_servicebus::pair_with(shared.clone());
    if let Err(e) = svc.load(&dir) {
        panic!(
            "Version 1 Service Bus state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/persist.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        );
    }

    let mut st = shared.state.lock();
    let ns = st.namespaces.get_mut("fixture").expect("namespace fixture");
    let mut queues: Vec<_> = ns.queues.keys().cloned().collect();
    queues.sort();
    assert_eq!(queues, vec!["orders", "plain"]);
    let orders = ns.queues.get("orders").unwrap();
    assert_eq!(orders.core.settings.lock_duration_ms, 45_000);
    assert_eq!(orders.core.settings.max_delivery_count, 5);
    assert!(orders.core.settings.requires_duplicate_detection);
    assert_eq!(orders.core.settings.user_metadata.as_deref(), Some("kept"));
    let bodies: Vec<String> = orders
        .core
        .main
        .msgs
        .values()
        .map(|m| match &m.message.body {
            azemu_amqp::message::Body::Data(parts) => String::from_utf8_lossy(&parts.concat()).to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(bodies, vec!["first", "second"]);
    let first = orders.core.main.msgs.values().next().unwrap();
    assert_eq!((first.seq, first.enqueued_ms, first.delivery_count), (1, NOW, 0));
    assert_eq!(ns.queues.get("plain").unwrap().core.main.msgs.len(), 0);

    let events = ns.topics.get("events").expect("topic events");
    let mut subs: Vec<_> = events.subs.keys().cloned().collect();
    subs.sort();
    assert_eq!(subs, vec!["all", "filtered"]);
    assert_eq!(events.subs["all"].rules.len(), 1);
    let rule = &events.subs["filtered"].rules[0];
    assert_eq!(rule.name, "big");
    assert_eq!(rule.created_ms, NOW);
    assert!(matches!(&rule.filter, azemu_servicebus::rules::RuleFilter::Sql { expression, .. } if expression == "amount > 10"));
}
