mod common;

use std::sync::Arc;

use azemu_eventhubs::state::Shared;
use common::*;

fn config_file(name: &str, json: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("azemu-eh-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("Config.json");
    std::fs::write(&p, json).unwrap();
    p
}

const MS_CONFIG: &str = r#"{
  "UserConfig": {
    "NamespaceConfig": [
      { "Type": "EventHub", "Name": "emulatorns",
        "Entities": [
          { "Name": "eh1", "PartitionCount": "2", "ConsumerGroups": [ { "Name": "cg1" }, { "Name": "cg2" } ] },
          { "Name": "eh2", "PartitionCount": 3, "MessageRetentionInSeconds": 7 }
        ] }
    ],
    "LoggingConfig": { "Type": "File" }
  }
}"#;

fn shared_with_config(name: &str, autocreate: bool, json: &str) -> Arc<Shared> {
    let s = Shared::new(autocreate, Some(config_file(name, json)));
    s.load_config();
    s
}

#[tokio::test]
async fn config_file_creates_hubs_groups_and_the_default_alias() {
    let env = env_with(shared_with_config("a", false, MS_CONFIG)).await;
    {
        let st = env.shared.state.lock();
        let h = st.hub("emulatorns", "eh1").unwrap();
        assert_eq!(h.partitions.len(), 2);
        assert!(h.has_consumer_group("cg1") && h.has_consumer_group("CG2") && h.has_consumer_group("$Default"));
        let h2 = st.hub("emulatorns", "EH2").unwrap();
        assert_eq!(h2.partitions.len(), 3);
        assert_eq!(h2.retention_ms, 7000);
        assert_eq!(st.hub("emulatorns", "eh1").unwrap().retention_ms, 86_400_000, "one day by default");
    }
    // the one namespace is reachable under its own name and as `localhost` / `default`
    for host in ["emulatorns.eventhub.localhost", "localhost", "127.0.0.1", "default.eventhub.localhost"] {
        let mut c = env.client_for(host).await;
        let s = attach_sender(&mut c, "eh1/Partitions/1").await.unwrap_or_else(|e| panic!("{host}: {e:?}"));
        send_one(&mut c, s, &event("x")).await;
        let e = attach_sender(&mut c, "eh1/Partitions/2").await.unwrap_err();
        assert_eq!(condition(e), "com.microsoft:argument-out-of-range", "{host}");
    }
    // a configured consumer group exists; an unknown hub is not created (autocreate is off)
    let mut c = env.client_for("localhost").await;
    consume(&mut c, "eh1", "cg1", 0, &from_start()).await.unwrap();
    let e = attach_sender(&mut c, "nothere").await.unwrap_err();
    assert_eq!(condition(e), "amqp:not-found");
}

#[tokio::test]
async fn reset_drops_everything_and_reloads_the_config() {
    let shared = shared_with_config("b", true, MS_CONFIG);
    let env = env_with(shared.clone()).await;
    let mut c = env.client_for("emulatorns.eventhub.localhost").await;
    let s = attach_sender(&mut c, "eh1/Partitions/0").await.unwrap();
    send_one(&mut c, s, &event("before-reset")).await;
    let r = consume(&mut c, "eh1", "$Default", 0, &with_filter("amqp.annotation.x-opt-offset > '@latest'")).await.unwrap();
    // an extra hub made at run time
    shared.state.lock().create_hub("emulatorns", "extra", 1, 1000, 0).unwrap();

    env.srv.reset();

    // the receiver is told its hub is gone
    let e = c.wait_detached(r).await.expect("detach error");
    assert_eq!(e.condition, "amqp:not-found");
    let st = shared.state.lock();
    assert!(st.hub("emulatorns", "extra").is_none());
    let h = st.hub("emulatorns", "eh1").expect("config hub is back");
    assert!(h.partitions[0].is_empty(), "events are gone");
    assert_eq!(h.partitions[0].next_seq, 0);
}

#[tokio::test]
async fn namespaces_are_separate() {
    let env = env().await;
    let mut a = env.client_for("one.eventhub.localhost").await;
    let mut b = env.client_for("two.eventhub.localhost").await;
    let sa = attach_sender(&mut a, "h/Partitions/0").await.unwrap();
    send_one(&mut a, sa, &event("in-one")).await;
    let r = consume(&mut b, "h", "$Default", 0, &from_start()).await.unwrap();
    expect_quiet(&mut b, r).await;
    let st = env.shared.state.lock();
    assert!(st.hub("one", "h").is_some() && st.hub("two", "h").is_some());
    assert_eq!(st.hub("one", "h").unwrap().partitions[0].events.len(), 1);
}

#[tokio::test]
async fn autocreate_on_creates_hub_with_four_partitions_and_consumer_group() {
    let env = env().await;
    let mut c = env.client().await;
    attach_sender(&mut c, "auto").await.unwrap();
    {
        let st = env.shared.state.lock();
        let h = st.hub("myns", "auto").unwrap();
        assert_eq!(h.partitions.len(), 4);
        assert!(h.autocreated);
        assert!(!h.has_consumer_group("newgroup"));
    }
    consume(&mut c, "auto", "newgroup", 3, &from_start()).await.unwrap();
    assert!(env.shared.state.lock().hub("myns", "auto").unwrap().has_consumer_group("newgroup"));
    // names are case-insensitive
    attach_sender(&mut c, "AUTO/Partitions/1").await.unwrap();
    assert_eq!(env.shared.state.lock().namespaces["myns"].hubs.len(), 1);
}

#[tokio::test]
async fn autocreate_off_refuses_missing_hubs_and_groups_with_not_found() {
    let env = env_with(Shared::new(false, None)).await;
    env.hub("exists", 2);
    let mut c = env.client().await;

    let e = attach_sender(&mut c, "missing").await.unwrap_err().unwrap();
    assert_eq!(e.condition, "amqp:not-found");
    let d = e.description.unwrap();
    assert!(d.contains("sb://myns.servicebus.windows.net/missing") && d.contains("could not be found") && d.contains("TrackingId"), "{d}");

    let e = consume(&mut c, "missing", "$Default", 0, &from_start()).await.unwrap_err();
    assert_eq!(condition(e), "amqp:not-found");

    let e = consume(&mut c, "exists", "nogroup", 0, &from_start()).await.unwrap_err().unwrap();
    assert_eq!(e.condition, "amqp:not-found");
    assert!(e.description.unwrap().contains("exists/ConsumerGroups/nogroup"));

    // what exists works
    attach_sender(&mut c, "exists/Partitions/1").await.unwrap();
    consume(&mut c, "exists", "$Default", 1, &from_start()).await.unwrap();
}

#[tokio::test]
async fn deleting_a_hub_detaches_its_receivers() {
    let env = env().await;
    env.hub("h", 1);
    let mut c = env.client().await;
    let r = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    assert!(env.shared.state.lock().delete_hub("myns", "h"));
    let e = c.wait_detached(r).await.unwrap();
    assert_eq!(e.condition, "amqp:not-found");
}

#[test]
fn hostname_rules() {
    use azemu_eventhubs::amqp_conn::{hostname_says_eventhubs as says, namespace_from_hostname as ns};
    assert_eq!(ns(Some("myns.eventhub.localhost")), "myns");
    assert_eq!(ns(Some("myns.servicebus.windows.net")), "myns");
    assert_eq!(ns(Some("MyNs.EventHubs.core.windows.net:5671")), "myns");
    for plain in ["localhost", "127.0.0.1", "[::1]", "", "eventhub.localhost", "servicebus.localhost"] {
        assert_eq!(ns(Some(plain)), "default", "{plain}");
    }
    assert_eq!(ns(None), "default");
    assert!(says(Some("myns.eventhub.localhost")));
    assert!(says(Some("myns.eventhubs.core.windows.net")));
    assert!(says(Some("eventhub.localhost:5672")));
    assert!(!says(Some("myns.servicebus.localhost")));
    assert!(!says(Some("localhost")));
    assert!(!says(None));
}
