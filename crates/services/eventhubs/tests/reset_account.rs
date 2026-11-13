//! Resetting one namespace (`POST /_azemu/reset?account=<namespace>`): its event hubs, events and Kafka
//! groups go, receivers waiting there are released, the config file's hubs come back, other namespaces are untouched,
//! and persistence sees a change.

mod common;

use std::time::Duration;

use azemu_core::Service;
use azemu_eventhubs::kafka::groups::Committed;
use azemu_eventhubs::state::{now_ms, Shared};
use common::*;

fn hubs(e: &Env, ns: &str) -> Vec<String> {
    e.shared.state.lock().namespaces.get(ns).map(|n| n.hubs.keys().cloned().collect()).unwrap_or_default()
}

fn commit(e: &Env, ns: &str) {
    let c = Committed { offset: 7, metadata: None, leader_epoch: -1 };
    assert_eq!(e.shared.kafka.commit(ns, "g", -1, "", vec![("hub".into(), 0, c)]), 0);
}

fn committed(e: &Env, ns: &str) -> bool {
    e.shared.kafka.fetch_offset(ns, "g", "hub", 0).is_some()
}

async fn setup() -> (Env, std::sync::Arc<azemu_eventhubs::EventHubsService>) {
    let shared = Shared::new(true, None);
    let (svc, amqp) = azemu_eventhubs::pair_with(shared.clone());
    let srv = azemu_core::testing::TestServer::start_with(vec![svc.clone()], vec![amqp]).await;
    let e = Env { srv, shared };
    for ns in ["alpha", "beta"] {
        e.shared.state.lock().create_hub(ns, "hub", 2, 86_400_000, now_ms()).unwrap();
        commit(&e, ns);
    }
    (e, svc)
}

#[tokio::test]
async fn one_namespace_is_emptied_and_the_other_is_not() {
    let (e, svc) = setup().await;
    let before = svc.persist_generation();
    assert!(svc.reset_account("Alpha"));
    assert_ne!(svc.persist_generation(), before, "persistence must see the reset");
    assert!(hubs(&e, "alpha").is_empty());
    assert!(!committed(&e, "alpha"), "the Kafka groups of the namespace end");
    assert_eq!(hubs(&e, "beta"), vec!["hub"]);
    assert!(committed(&e, "beta"));
    assert!(svc.reset_account("never-used"));
    assert_eq!(hubs(&e, "beta"), vec!["hub"]);
}

#[tokio::test]
async fn a_receiver_waiting_in_the_namespace_is_released() {
    let (e, svc) = setup().await;
    let mut c = e.client_for("alpha.eventhub.localhost").await;
    let r = consume(&mut c, "hub", "$Default", 0, &from_start()).await.unwrap();
    expect_quiet(&mut c, r).await;
    let mut other = e.client_for("beta.eventhub.localhost").await;
    let r2 = consume(&mut other, "hub", "$Default", 0, &from_start()).await.unwrap();
    expect_quiet(&mut other, r2).await;

    assert!(svc.reset_account("alpha"));
    let err =
        tokio::time::timeout(Duration::from_secs(5), c.wait_detached(r)).await.expect("the waiting receiver is told its event hub is gone");
    assert!(err.is_some(), "detached with an error");
    // the receiver in the other namespace still gets events
    let mut s = e.client_for("beta.eventhub.localhost").await;
    let l = attach_sender(&mut s, "hub/Partitions/0").await.unwrap();
    send_one(&mut s, l, &event("still-here")).await;
    assert_eq!(take(&mut other, r2, 1).await, vec!["still-here"]);
}

#[tokio::test]
async fn the_config_file_hubs_of_the_namespace_come_back() {
    let dir = std::env::temp_dir().join(format!("azemu-eh-reset-account-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        r#"{"UserConfig":{"NamespaceConfig":[
            {"Type":"EventHub","Name":"alpha","Entities":[{"Name":"declared","PartitionCount":2}]},
            {"Type":"EventHub","Name":"beta","Entities":[{"Name":"declared-b","PartitionCount":2}]}]}}"#,
    )
    .unwrap();
    let shared = Shared::new(true, Some(path));
    shared.load_config();
    let (svc, amqp) = azemu_eventhubs::pair_with(shared.clone());
    let srv = azemu_core::testing::TestServer::start_with(vec![svc.clone()], vec![amqp]).await;
    let e = Env { srv, shared };
    e.shared.state.lock().create_hub("alpha", "extra", 1, 86_400_000, now_ms()).unwrap();
    assert!(svc.reset_account("alpha"));
    assert_eq!(hubs(&e, "alpha"), vec!["declared"], "what the file declares is back, what a client made is gone");
    assert_eq!(hubs(&e, "beta"), vec!["declared-b"]);
    let _ = std::fs::remove_dir_all(dir);
}
