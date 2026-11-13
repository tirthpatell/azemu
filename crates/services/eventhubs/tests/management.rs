mod common;

use azemu_amqp::client::TestClient;
use azemu_amqp::message::Properties;
use azemu_amqp::types::Value;
use common::*;

fn hub_request(hub: &str) -> Vec<(&str, &str)> {
    vec![("name", hub), ("operation", "READ"), ("type", "com.microsoft:eventhub"), ("security_token", "SharedAccessSignature sr=x&sig=y")]
}

fn partition_request<'a>(hub: &'a str, pid: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![("name", hub), ("partition", pid), ("operation", "READ"), ("type", "com.microsoft:partition")]
}

#[tokio::test]
async fn cbs_put_token_is_accepted_with_202() {
    let env = env().await;
    let mut c = env.client().await;
    let resp = c.attach_receiver("$cbs", 10).await.unwrap();
    let req = attach_sender(&mut c, "$cbs").await.unwrap();
    let mut m = TestClient::value_message(
        Value::str("SharedAccessSignature sr=abc&sig=def&se=1&skn=k"),
        vec![
            ("operation", Value::str("put-token")),
            ("type", Value::str("servicebus.windows.net:sastoken")),
            ("name", Value::str("sb://myns.eventhub.localhost/h")),
        ],
    );
    m.properties = Some(Properties { message_id: Some(Value::Uuid([9; 16])), ..Default::default() });
    send_one(&mut c, req, &m).await;
    let r = c.recv(resp).await.message();
    assert_eq!(r.app_property("status-code").and_then(|v| v.as_i64()), Some(202));
    assert_eq!(r.app_property("status-description").and_then(|v| v.as_str()), Some("Accepted"));
    assert_eq!(r.properties.unwrap().correlation_id, Some(Value::Uuid([9; 16])));

    // anything but put-token is not built
    let mut m = TestClient::value_message(Value::Null, vec![("operation", Value::str("renew"))]);
    m.properties = Some(Properties { message_id: Some(Value::Uuid([8; 16])), ..Default::default() });
    send_one(&mut c, req, &m).await;
    let r = c.recv(resp).await.message();
    assert_eq!(r.app_property("status-code").and_then(|v| v.as_i64()), Some(501));
}

#[tokio::test]
async fn hub_properties() {
    let env = env().await;
    env.hub("h", 3);
    let mut c = env.client().await;
    let links = mgmt_links(&mut c).await;
    let (status, _, body) = mgmt(&mut c, links, hub_request("h")).await;
    assert_eq!(status, 200);
    assert_eq!(map_get(&body, "name").as_str(), Some("h"));
    assert!(matches!(map_get(&body, "created_at"), Value::Timestamp(t) if *t > 1_600_000_000_000));
    assert_eq!(map_get(&body, "partition_count").as_i64(), Some(3));
    let ids: Vec<&str> = map_get(&body, "partition_ids").as_list().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(ids, vec!["0", "1", "2"]);
    // keys the SDKs read: JavaScript `georeplication_factor` (`isGeoDrEnabled` is "> 1"), Rust none more than the above
    assert_eq!(map_get(&body, "georeplication_factor").as_i64(), Some(1));
    assert_eq!(map_get(&body, "type").as_str(), Some("com.microsoft:eventhub"));
}

#[tokio::test]
async fn partition_properties_follow_the_stream() {
    let env = env().await;
    env.hub("h", 2);
    let mut c = env.client().await;
    let links = mgmt_links(&mut c).await;
    let (status, _, empty) = mgmt(&mut c, links, partition_request("h", "1")).await;
    assert_eq!(status, 200);
    assert_eq!(map_get(&empty, "name").as_str(), Some("h"));
    assert_eq!(map_get(&empty, "partition").as_str(), Some("1"));
    assert_eq!(map_get(&empty, "is_partition_empty"), &Value::Bool(true));
    // the Rust SDK refuses a reply without these two keys
    assert_eq!(map_get(&empty, "type").as_str(), Some("com.microsoft:partition"));
    assert!(map_get(&empty, "last_enqueued_sequence_number_epoch").as_i64().is_some());
    assert_eq!(map_get(&empty, "last_enqueued_sequence_number").as_i64(), Some(-1));
    assert_eq!(map_get(&empty, "begin_sequence_number").as_i64(), Some(-1));
    assert_eq!(map_get(&empty, "last_enqueued_offset").as_str(), Some("-1"));
    assert_eq!(map_get(&empty, "last_enqueued_time_utc"), &Value::Timestamp(-62_135_596_800_000));

    let s = attach_sender(&mut c, "h/Partitions/1").await.unwrap();
    for i in 0..3 {
        send_one(&mut c, s, &event(&format!("e{i}"))).await;
    }
    let (_, _, p) = mgmt(&mut c, links, partition_request("h", "1")).await;
    assert_eq!(map_get(&p, "is_partition_empty"), &Value::Bool(false));
    assert_eq!(map_get(&p, "begin_sequence_number").as_i64(), Some(0));
    assert_eq!(map_get(&p, "last_enqueued_sequence_number").as_i64(), Some(2));
    // the offset matches what a receiver sees on the last event
    let r = consume(&mut c, "h", "$Default", 1, &from_start()).await.unwrap();
    let mut last = None;
    for _ in 0..3 {
        last = Some(c.recv(r).await.message());
    }
    assert_eq!(map_get(&p, "last_enqueued_offset").as_str(), Some(ann_str(&last.unwrap(), "x-opt-offset").as_str()));
    assert!(matches!(map_get(&p, "last_enqueued_time_utc"), Value::Timestamp(t) if *t > 1_600_000_000_000));
    // the other partition is untouched
    let (_, _, other) = mgmt(&mut c, links, partition_request("h", "0")).await;
    assert_eq!(map_get(&other, "is_partition_empty"), &Value::Bool(true));
}

#[tokio::test]
async fn bad_partition_unknown_type_and_unknown_operation() {
    let env = env().await;
    env.hub("h", 2);
    let mut c = env.client().await;
    let links = mgmt_links(&mut c).await;
    let (status, _, _) = mgmt(&mut c, links, partition_request("h", "7")).await;
    assert_eq!(status, 400);
    let (status, desc, _) = mgmt(&mut c, links, vec![("name", "h"), ("operation", "READ"), ("type", "com.microsoft:nope")]).await;
    assert_eq!(status, 501);
    assert!(desc.contains("management type 'com.microsoft:nope'"), "{desc}");
    let (status, desc, _) = mgmt(&mut c, links, vec![("name", "h"), ("operation", "DELETE"), ("type", "com.microsoft:eventhub")]).await;
    assert_eq!(status, 501);
    assert!(desc.contains("'DELETE'"), "{desc}");
    let (status, _, _) = mgmt(&mut c, links, vec![("operation", "READ"), ("type", "com.microsoft:eventhub")]).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn reading_properties_of_a_missing_hub_creates_it_or_404s() {
    let env = env().await;
    let mut c = env.client().await;
    let links = mgmt_links(&mut c).await;
    let (status, _, body) = mgmt(&mut c, links, hub_request("fresh")).await;
    assert_eq!(status, 200, "auto-creation is on");
    assert_eq!(map_get(&body, "partition_count").as_i64(), Some(4));

    let strict = env_with(azemu_eventhubs::state::Shared::new(false, None)).await;
    let mut c = strict.client().await;
    let links = mgmt_links(&mut c).await;
    let (status, desc, _) = mgmt(&mut c, links, hub_request("missing")).await;
    assert_eq!(status, 404);
    assert!(desc.contains("sb://myns.servicebus.windows.net/missing") && desc.contains("could not be found"), "{desc}");
}

#[tokio::test]
async fn two_management_pairs_on_one_connection_answer_on_their_own_links() {
    let env = env().await;
    env.hub("h", 2);
    let mut c = env.client().await;
    let a = mgmt_links(&mut c).await;
    let b = mgmt_links(&mut c).await;
    let (_, _, x) = mgmt(&mut c, a, hub_request("h")).await;
    let (_, _, y) = mgmt(&mut c, b, partition_request("h", "0")).await;
    assert!(x.map_get("partition_ids").is_some());
    assert!(y.map_get("begin_sequence_number").is_some());
    common::expect_quiet(&mut c, a.1).await;
    common::expect_quiet(&mut c, b.1).await;
}
