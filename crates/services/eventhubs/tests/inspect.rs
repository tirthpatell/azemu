//! The inspector of Event Hubs and its Kafka endpoint (`/_azemu/inspect/eventhubs/...`).

mod common;

use azemu_amqp::message::{Message, Properties};
use azemu_amqp::types::Value;
use azemu_core::inspect::walk::walk;
use azemu_eventhubs::kafka::groups::Committed;
use azemu_eventhubs::model::key;
use azemu_eventhubs::schema_registry::{Compatibility, SchemaEntry, SchemaGroup, SchemaType, SchemaVersion};
use azemu_eventhubs::state::{now_ms, SendTarget};
use common::*;

async fn insp(e: &Env, p: &str) -> azemu_core::testing::TestResponse {
    e.srv.send("GET", &format!("{}/_azemu/inspect{p}", e.srv.gateway_url()), &[], "").await
}

fn rich(body: &str) -> Message {
    let mut m = event(body);
    m.properties = Some(Properties {
        content_type: Some("text/plain".into()),
        message_id: Some(Value::str(format!("id-{body}"))),
        ..Default::default()
    });
    m.application_properties = Some(vec![(Value::str("kind"), Value::str("sample"))]);
    m
}

fn fill(e: &Env) {
    e.hub("orders", 2);
    e.hub("logs", 1);
    let mut st = e.shared.state.lock();
    for i in 0..5 {
        st.store("myns", "orders", SendTarget::Partition(0), vec![rich(&format!("e{i}"))], false, now_ms()).unwrap();
    }
    st.store("myns", "orders", SendTarget::Partition(1), vec![keyed("k", "customer-1")], false, now_ms()).unwrap();
    st.store("myns", "orders", SendTarget::Partition(1), vec![Message::data(vec![0, 159, 146, 150])], false, now_ms()).unwrap();
    let ns = st.namespaces.get_mut("myns").unwrap();
    let mut g = SchemaGroup::new("payments", SchemaType::Avro, Compatibility::Backward, now_ms());
    g.schemas.insert(
        "invoice".into(),
        SchemaEntry {
            name: "invoice".into(),
            versions: vec![
                SchemaVersion {
                    version: 1,
                    kind: SchemaType::Avro,
                    id: "a".repeat(32),
                    content: r#"{"type":"record","name":"invoice","fields":[]}"#.into(),
                    created_ms: now_ms(),
                },
                SchemaVersion {
                    version: 2,
                    kind: SchemaType::Avro,
                    id: "b".repeat(32),
                    content: r#"{"type":"record","name":"invoice","fields":[{"name":"n","type":"int"}]}"#.into(),
                    created_ms: now_ms(),
                },
            ],
        },
    );
    ns.schema_groups.insert(key("payments"), g);
    drop(st);
    e.shared.kafka.import_offsets(vec![(
        "myns".into(),
        "billing".into(),
        vec![("orders".into(), 0, Committed { offset: 3, metadata: Some("m".into()), leader_epoch: 0 })],
    )]);
}

#[tokio::test]
async fn the_whole_tree_can_be_walked_and_paging_ends() {
    let e = env().await;
    fill(&e);
    for limit in [1, 2, 50] {
        let nodes = walk(&e.srv, "/eventhubs", limit).await;
        let kinds: Vec<&str> = nodes.iter().map(|n| n.node["kind"].as_str().unwrap()).collect();
        for k in [
            "service",
            "namespace",
            "hubs",
            "hub",
            "partitions",
            "partition",
            "kafka-groups",
            "kafka-group",
            "schemagroups",
            "schemagroup",
            "schemas",
            "schema",
        ] {
            assert!(kinds.contains(&k), "no {k} among {kinds:?}");
        }
    }
}

#[tokio::test]
async fn partitions_show_numbers_and_events_come_from_a_sequence_number_with_properties_and_body() {
    let e = env().await;
    fill(&e);
    let parts = insp(&e, "/eventhubs/myns/hubs/orders/partitions").await.json();
    let p0 = &parts["items"][0];
    assert_eq!(
        (p0["beginSequenceNumber"].as_i64(), p0["lastSequenceNumber"].as_i64(), p0["retainedEvents"].as_i64()),
        (Some(0), Some(4), Some(5))
    );
    assert!(p0["lastOffset"].is_string() && p0["nextOffset"].as_i64().unwrap() > 0);
    let logs = insp(&e, "/eventhubs/myns/hubs/logs/partitions").await.json();
    assert_eq!(logs["items"][0]["beginSequenceNumber"], -1, "an empty partition");
    assert!(logs["items"][0]["lastEnqueuedTime"].is_null());
    let from = insp(&e, "/eventhubs/myns/hubs/orders/partitions/0?from=3").await.json();
    let rows = from["items"].as_array().unwrap();
    assert_eq!(rows.iter().map(|r| r["sequenceNumber"].as_i64().unwrap()).collect::<Vec<_>>(), [3, 4]);
    assert_eq!(rows[0]["body"]["text"], "e3");
    assert_eq!(rows[0]["contentType"], "text/plain");
    assert_eq!(rows[0]["applicationProperties"]["kind"], "sample");
    assert_eq!(from["properties"]["kafkaCommittedOffsets"], serde_json::json!([{"group": "billing", "offset": 3}]));
    let p1 = insp(&e, "/eventhubs/myns/hubs/orders/partitions/1").await.json();
    assert_eq!(p1["items"][0]["partitionKey"], "customer-1");
    assert!(p1["items"][1]["body"].get("base64").is_some());
    // paging by the marker follows the sequence numbers
    let first = insp(&e, "/eventhubs/myns/hubs/orders/partitions/0?limit=2").await.json();
    let marker = first["next"].as_str().unwrap();
    let second = insp(&e, &format!("/eventhubs/myns/hubs/orders/partitions/0?limit=2&marker={marker}")).await.json();
    assert_eq!(second["items"][0]["sequenceNumber"], 2);
    // Kafka and the schema registry
    let g = insp(&e, "/eventhubs/myns/kafka-groups/billing").await.json();
    assert_eq!(g["items"][0]["offset"], 3);
    let schema = insp(&e, "/eventhubs/myns/schemagroups/payments/schemas/invoice").await.json();
    assert_eq!(schema["items"].as_array().unwrap().len(), 2);
    assert_eq!(insp(&e, "/eventhubs/myns/schemagroups/payments").await.json()["properties"]["compatibility"], "Backward");
}

#[tokio::test]
async fn looking_changes_nothing_a_receiver_can_see() {
    let e = env().await;
    fill(&e);
    let view = || {
        let st = e.shared.state.lock();
        let h = st.hub("myns", "orders").unwrap();
        (
            h.partitions.iter().map(|p| (p.events.len(), p.next_seq, p.next_offset, p.total_events, p.rev)).collect::<Vec<_>>(),
            h.round_robin(),
            e.shared.kafka.export_offsets().len(),
        )
    };
    let before = view();
    walk(&e.srv, "", 1).await;
    walk(&e.srv, "/eventhubs", 3).await;
    assert_eq!(before, view());
    for p in [
        "/eventhubs/ghost",
        "/eventhubs/myns/hubs/none",
        "/eventhubs/myns/hubs/orders/partitions/9",
        "/eventhubs/myns/kafka-groups/none",
        "/eventhubs/myns/schemagroups/none",
    ] {
        assert_eq!(insp(&e, p).await.status, 404, "{p}");
    }
    assert!(!e.shared.state.lock().namespaces.contains_key("ghost"), "looking made a namespace");
}
