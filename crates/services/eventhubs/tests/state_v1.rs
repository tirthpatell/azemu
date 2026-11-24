//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/eventhubs/` holds the files `Service::save` wrote for the small fixed state built by `fixed_state`
//! (`AZEMU-STATE eventhubs 1` and `AZEMU-STATE eventhubs-partition 1`). The files are checked in and are never rewritten
//! by the test run. `version_1_files_still_load` loads them into a new service and checks what they hold.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in files
//! and do not weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (and read it into the
//! same state this test expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-eventhubs --test state_v1 -- --ignored write_the_fixture`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_amqp::message::Message;
use azemu_amqp::types::Value;
use azemu_core::Service;
use azemu_eventhubs::kafka::groups::Committed;
use azemu_eventhubs::state::Shared;
use azemu_eventhubs::EventHubsService;

const NOW: i64 = 1_700_000_000_000;
/// ten years, so that nothing is trimmed whenever the test runs
const RETENTION_MS: i64 = 315_360_000_000;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("eventhubs")
}

fn event(body: &str, key: Option<&str>) -> Message {
    let mut m = Message::data(body.as_bytes().to_vec());
    if let Some(k) = key {
        m.message_annotations = Some(vec![(Value::sym("x-opt-partition-key"), Value::str(k))]);
    }
    m
}

fn fixed_state() -> (Arc<EventHubsService>, Arc<Shared>) {
    let shared = Shared::new(false, None);
    let (svc, _amqp) = azemu_eventhubs::pair_with(shared.clone());
    {
        let mut st = shared.state.lock();
        let hub = st.create_hub("fixture", "telemetry", 2, RETENTION_MS, NOW).unwrap();
        hub.add_consumer_group("readers");
        hub.partitions[0].append(event("zero-a", Some("device-1")), NOW);
        hub.partitions[0].append(event("zero-b", None), NOW + 10);
        hub.partitions[1].append(event("one-a", None), NOW + 20);
        st.create_hub("fixture", "empty", 1, RETENTION_MS, NOW).unwrap();
    }
    let c = Committed { offset: 5, metadata: Some("m".into()), leader_epoch: -1 };
    assert_eq!(shared.kafka.commit("fixture", "group-1", -1, "", vec![("telemetry".into(), 1, c)]), 0);
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
    let dir = std::env::temp_dir().join(format!("azemu-eh-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut copied = 0;
    for entry in std::fs::read_dir(fixture_dir()).expect("tests/state-v1/eventhubs is checked in") {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
        copied += 1;
    }
    assert!(copied >= 4, "the fixture has the definitions file and one file per partition ({copied} files found)");

    let shared = Shared::new(false, None);
    let (svc, _amqp) = azemu_eventhubs::pair_with(shared.clone());
    if let Err(e) = svc.load(&dir) {
        panic!(
            "Version 1 Event Hubs state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/persist.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        );
    }

    let st = shared.state.lock();
    let ns = st.namespaces.get("fixture").expect("namespace fixture");
    let mut names: Vec<_> = ns.hubs.keys().cloned().collect();
    names.sort();
    assert_eq!(names, vec!["empty", "telemetry"]);
    let hub = &ns.hubs["telemetry"];
    assert_eq!((hub.partitions.len(), hub.retention_ms, hub.created_ms), (2, RETENTION_MS, NOW));
    let mut groups: Vec<_> = hub.consumer_groups.values().cloned().collect();
    groups.sort();
    assert_eq!(groups, vec!["$Default", "readers"]);
    let body = |e: &azemu_eventhubs::model::Event| match &e.message.body {
        azemu_amqp::message::Body::Data(parts) => String::from_utf8_lossy(&parts.concat()).to_string(),
        other => format!("{other:?}"),
    };
    let p0: Vec<_> = hub.partitions[0].events.iter().map(|e| (e.seq, body(e), e.partition_key.clone(), e.enqueued_ms)).collect();
    assert_eq!(
        p0,
        vec![(0, "zero-a".to_string(), Some("device-1".to_string()), NOW), (1, "zero-b".to_string(), None, NOW + 10)],
        "events of partition 0 with sequence numbers, partition keys and enqueue times"
    );
    let p1: Vec<_> = hub.partitions[1].events.iter().map(|e| (e.seq, body(e), e.enqueued_ms)).collect();
    assert_eq!(p1, vec![(0, "one-a".to_string(), NOW + 20)]);
    assert_eq!(ns.hubs["empty"].partitions.len(), 1);
    drop(st);

    let committed = shared.kafka.fetch_offset("fixture", "group-1", "telemetry", 1).expect("the committed Kafka offset");
    assert_eq!((committed.offset, committed.metadata.as_deref()), (5, Some("m")));
}

// ---- the Schema Registry's file ---------------------------------------------------------------------------------
//
// `schemas.state` (kind `eventhubs-schemas`, version 1) is a file of its own: a directory from before the registry has none, and
// the files above are not changed by it. `tests/state-v1/eventhubs-schemas/` holds the file for the fixed state built below (the generator also writes `eventhubs.state`, which is left out).
// To make it again (only when this state itself changes, which also means a new version):
// `cargo test -p azemu-eventhubs --test state_v1 -- --ignored write_the_schema_fixture`.

fn schema_fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("eventhubs-schemas")
}

const ID_1: &str = "258e22ae9c4d463abe61dbd251bdaab4";
const ID_2: &str = "6c9729268f974d3ebbd5dd14e8ad8f96";
const ORDER_1: &str = r#"{"type":"record","name":"Order","fields":[{"name":"id","type":"string"}]}"#;
const ORDER_2: &str = r#"{"type":"record","name":"Order","fields":[{"name":"id","type":"string"},{"name":"qty","type":"int"}]}"#;

fn schema_state() -> (Arc<EventHubsService>, Arc<Shared>) {
    use azemu_eventhubs::schema_registry::*;
    let shared = Shared::new(false, None);
    let (svc, _amqp) = azemu_eventhubs::pair_with(shared.clone());
    {
        let mut st = shared.state.lock();
        let mut g = SchemaGroup::new("Orders", SchemaType::Avro, Compatibility::Forward, NOW);
        g.updated_ms = NOW + 5;
        g.etag = "a3583ada-c241-4a13-91d1-8e4c93545749".into();
        g.properties.insert("owner".into(), "team".into());
        let v = |version, id: &str, content: &str| SchemaVersion {
            version,
            kind: SchemaType::Avro,
            id: id.into(),
            content: content.into(),
            created_ms: NOW + 10,
        };
        g.schemas.insert(
            "order".into(),
            SchemaEntry { name: "Order".into(), versions: vec![v(2, ID_2, ORDER_2)] }, // version 1 was deleted
        );
        g.schemas.insert("other".into(), SchemaEntry { name: "other".into(), versions: vec![v(1, ID_1, ORDER_1)] });
        st.namespaces.entry("fixture".into()).or_default().schema_groups.insert("orders".into(), g);
    }
    (svc, shared)
}

#[test]
#[ignore = "writes the checked-in fixture; see the text above"]
fn write_the_schema_fixture() {
    let (svc, _shared) = schema_state();
    let dir = schema_fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    svc.save(&dir).unwrap();
}

#[test]
fn schema_registry_version_1_file_still_loads() {
    let dir = std::env::temp_dir().join(format!("azemu-eh-schemas-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(schema_fixture_dir().join("schemas.state"), dir.join("schemas.state"))
        .expect("tests/state-v1/eventhubs-schemas/schemas.state is checked in");
    let shared = Shared::new(false, None);
    let (svc, _amqp) = azemu_eventhubs::pair_with(shared.clone());
    if let Err(e) = svc.load(&dir) {
        panic!(
            "The version 1 schema registry file no longer loads: {e}\nRaise VERSION in src/schema_persist.rs, keep a reader for version 1 and add a fixture for the \
             new version. Do not edit tests/state-v1."
        );
    }
    let st = shared.state.lock();
    let g = st.schema_group("fixture", "ORDERS").expect("the group");
    assert_eq!(
        (g.name.as_str(), g.schema_type.name(), g.compatibility.name(), g.created_ms, g.updated_ms),
        ("Orders", "Avro", "Forward", NOW, NOW + 5)
    );
    assert_eq!(g.etag, "a3583ada-c241-4a13-91d1-8e4c93545749");
    assert_eq!(g.properties.get("owner").map(String::as_str), Some("team"));
    let order = &g.schemas["order"];
    assert_eq!(order.name, "Order");
    assert_eq!(order.versions.iter().map(|v| (v.version, v.id.as_str(), v.content.as_str())).collect::<Vec<_>>(), vec![(2, ID_2, ORDER_2)]);
    assert_eq!(g.schemas["other"].versions[0].id, ID_1);
    assert_eq!(st.schema_by_id("fixture", ID_1).map(|f| (f.schema, f.version.version)), Some(("other".to_string(), 1)));
    drop(st);
    // saving it again gives the same file: nothing in the layout changed
    let again = std::env::temp_dir().join(format!("azemu-eh-schemas-v1-again-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&again);
    std::fs::create_dir_all(&again).unwrap();
    svc.save(&again).unwrap();
    assert_eq!(std::fs::read(again.join("schemas.state")).unwrap(), std::fs::read(dir.join("schemas.state")).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&again);
}
