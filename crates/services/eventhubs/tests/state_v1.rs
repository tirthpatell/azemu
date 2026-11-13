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
