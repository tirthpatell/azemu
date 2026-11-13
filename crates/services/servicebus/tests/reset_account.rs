//! Resetting one namespace (`POST /_azemu/reset?account=<namespace>`): that namespace is emptied, the
//! others are not, receivers waiting there are released, the config file's entities come back, and persistence
//! sees a change.

mod common;

use std::sync::Arc;
use std::time::Duration;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_servicebus::state::Shared;
use common::*;

async fn two_namespaces() -> (Env, Arc<azemu_servicebus::ServiceBusService>) {
    let shared = Shared::new(true, None);
    let (svc, amqp) = azemu_servicebus::pair_with(shared.clone());
    let srv = TestServer::start_with(vec![svc.clone()], vec![amqp]).await;
    let e = Env { srv, shared };
    for ns in ["alpha", "beta"] {
        e.state(|st| {
            let n = st.ns(ns);
            n.create_queue("jobs", azemu_servicebus::model::Settings::queue_default(azemu_servicebus::state::now_ms())).unwrap();
            n.create_queue("idle", azemu_servicebus::model::Settings::queue_default(azemu_servicebus::state::now_ms())).unwrap();
            n.create_topic("events", azemu_servicebus::model::Settings::topic_default(azemu_servicebus::state::now_ms())).unwrap();
        });
        e.inject_ns(ns, "jobs");
        e.inject_ns(ns, "jobs");
    }
    (e, svc)
}

fn messages(e: &Env, ns: &str, queue: &str) -> Option<usize> {
    e.state(|st| st.namespaces.get(ns).and_then(|n| n.queues.get(queue)).map(|q| q.core.main.msgs.len()))
}

#[tokio::test]
async fn one_namespace_is_emptied_and_the_other_is_not() {
    let (e, svc) = two_namespaces().await;
    let before = svc.persist_generation();
    assert!(svc.reset_account("Alpha"));
    assert_ne!(svc.persist_generation(), before, "persistence must see the reset");
    assert_eq!(messages(&e, "alpha", "jobs"), None);
    e.state(|st| assert!(st.namespaces.get("alpha").is_none_or(|n| n.queues.is_empty() && n.topics.is_empty())));
    assert_eq!(messages(&e, "beta", "jobs"), Some(2));
    assert!(e.state(|st| st.namespaces["beta"].topics.contains_key("events")));
    // a namespace that was never used: nothing to do, and still `true`
    assert!(svc.reset_account("never-used"));
    assert_eq!(messages(&e, "beta", "jobs"), Some(2));
}

#[tokio::test]
async fn a_receiver_waiting_in_the_namespace_is_released() {
    let (e, svc) = two_namespaces().await;
    let mut c = e.client_for("alpha.servicebus.localhost").await;
    let r = receiver(&mut c, "idle", 1).await;
    no_delivery(&mut c, r, 100).await;
    let mut other = e.client_for("beta.servicebus.localhost").await;
    let r2 = receiver(&mut other, "idle", 1).await;
    no_delivery(&mut other, r2, 100).await;

    assert!(svc.reset_account("alpha"));
    let err =
        tokio::time::timeout(Duration::from_secs(5), c.wait_detached(r)).await.expect("the waiting receiver is told its queue is gone");
    assert!(err.is_some(), "detached with an error");
    // the receiver in the other namespace still waits and still gets a message
    e.inject_ns("beta", "idle");
    let got = other.recv_for(r2, Duration::from_secs(3)).await;
    assert!(got.is_some(), "the other namespace is untouched");
}

#[tokio::test]
async fn the_config_file_entities_of_the_namespace_come_back() {
    let dir = std::env::temp_dir().join(format!("azemu-sb-reset-account-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        r#"{"UserConfig":{"Namespaces":[
            {"Name":"alpha","Queues":[{"Name":"declared"}]},
            {"Name":"beta","Queues":[{"Name":"declared-b"}]}]}}"#,
    )
    .unwrap();
    let shared = Shared::new(true, Some(path));
    shared.load_config();
    let (svc, _amqp) = azemu_servicebus::pair_with(shared.clone());
    let e = Env { srv: TestServer::start_with(vec![svc.clone()], vec![]).await, shared };
    e.state(|st| {
        let n = st.ns("alpha");
        n.create_queue("extra", azemu_servicebus::model::Settings::queue_default(azemu_servicebus::state::now_ms())).unwrap();
    });
    assert!(svc.reset_account("alpha"));
    e.state(|st| {
        let a: Vec<_> = st.namespaces["alpha"].queues.keys().cloned().collect();
        assert_eq!(a, vec!["declared"], "what the file declares is back, what a client made is gone");
        assert!(st.namespaces["beta"].queues.contains_key("declared-b"));
    });
    let _ = std::fs::remove_dir_all(dir);
}
