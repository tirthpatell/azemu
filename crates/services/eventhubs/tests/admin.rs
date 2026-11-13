mod common;

use common::*;
use serde_json::{json, Value};

fn url(env: &Env, path: &str) -> String {
    format!("{}/_azemu/eventhubs{path}", env.srv.gateway_url())
}

async fn call(env: &Env, method: &str, path: &str, body: &str) -> (u16, Value) {
    let r = env.srv.send(method, &url(env, path), &[], body.to_string()).await;
    let v = if r.body.is_empty() { Value::Null } else { serde_json::from_slice(&r.body).unwrap_or(Value::Null) };
    (r.status, v)
}

#[tokio::test]
async fn create_get_list_and_delete_a_hub() {
    let env = env().await;
    let (st, v) = call(&env, "PUT", "/myns/hubs/orders", r#"{"partitionCount":3,"retentionDays":2,"consumerGroups":["a","B"]}"#).await;
    assert_eq!(st, 201, "{v}");
    assert_eq!(v["name"], "orders");
    assert_eq!(v["partitionCount"], 3);
    assert_eq!(v["retentionSeconds"], 172_800.0);
    assert_eq!(v["partitionIds"], json!(["0", "1", "2"]));
    assert_eq!(v["consumerGroups"], json!(["$Default", "a", "B"]), "sorted by lower-case name, spelled as created");

    let (st, v) = call(&env, "GET", "/myns/hubs/orders", "").await;
    assert_eq!(st, 200);
    assert_eq!(v["partitions"].as_array().unwrap().len(), 3);
    assert_eq!(v["partitions"][0]["isEmpty"], true);
    assert_eq!(v["partitions"][0]["lastEnqueuedSequenceNumber"], -1);

    let (st, v) = call(&env, "GET", "/myns/hubs", "").await;
    assert_eq!(st, 200);
    assert_eq!(v["eventHubs"].as_array().unwrap().len(), 1);
    let (_, v) = call(&env, "GET", "/namespaces", "").await;
    assert_eq!(v["namespaces"][0], json!({"name": "myns", "eventHubs": 1}));

    // defaults: 4 partitions, one day
    let (st, v) = call(&env, "PUT", "/myns/hubs/plain", "").await;
    assert_eq!((st, v["partitionCount"].clone(), v["retentionSeconds"].clone()), (201, json!(4), json!(86_400.0)));

    let (st, _) = call(&env, "DELETE", "/myns/hubs/orders", "").await;
    assert_eq!(st, 204);
    let (st, v) = call(&env, "GET", "/myns/hubs/orders", "").await;
    assert_eq!(st, 404);
    assert_eq!(v["error"]["code"], "NotFound");
    assert_eq!(call(&env, "DELETE", "/myns/hubs/orders", "").await.0, 404);
}

#[tokio::test]
async fn put_again_updates_but_cannot_change_the_partition_count() {
    let env = env().await;
    call(&env, "PUT", "/n/hubs/h", r#"{"partitionCount":2}"#).await;
    let (st, v) = call(&env, "PUT", "/n/hubs/h", r#"{"partitionCount":2,"retentionSeconds":90,"consumerGroups":["x"]}"#).await;
    assert_eq!(st, 200);
    assert_eq!(v["retentionSeconds"], 90.0);
    assert!(v["consumerGroups"].as_array().unwrap().iter().any(|g| g == "x"));
    let (st, v) = call(&env, "PUT", "/n/hubs/h", r#"{"partitionCount":8}"#).await;
    assert_eq!(st, 400, "{v}");
    let (st, _) = call(&env, "PUT", "/n/hubs/bad", r#"{"partitionCount":0}"#).await;
    assert_eq!(st, 400);
    let (st, _) = call(&env, "PUT", "/n/hubs/bad", "{nope").await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn consumer_group_operations() {
    let env = env().await;
    call(&env, "PUT", "/n/hubs/h", "").await;
    assert_eq!(call(&env, "PUT", "/n/hubs/h/consumergroups/cg1", "").await.0, 201);
    assert_eq!(call(&env, "PUT", "/n/hubs/h/consumergroups/cg1", "").await.0, 200);
    let (st, v) = call(&env, "GET", "/n/hubs/h/consumergroups/CG1", "").await;
    assert_eq!((st, v["name"].clone()), (200, json!("cg1")));
    let (_, v) = call(&env, "GET", "/n/hubs/h/consumergroups", "").await;
    assert_eq!(v["consumerGroups"], json!(["$Default", "cg1"]));
    assert_eq!(call(&env, "DELETE", "/n/hubs/h/consumergroups/$Default", "").await.0, 400);
    assert_eq!(call(&env, "DELETE", "/n/hubs/h/consumergroups/cg1", "").await.0, 204);
    assert_eq!(call(&env, "GET", "/n/hubs/h/consumergroups/cg1", "").await.0, 404);
    assert_eq!(call(&env, "PUT", "/n/hubs/nohub/consumergroups/x", "").await.0, 404);
}

#[tokio::test]
async fn partition_counters_follow_traffic() {
    let env = env().await;
    call(&env, "PUT", "/myns/hubs/h", r#"{"partitionCount":2}"#).await;
    let mut c = env.client().await;
    let s = attach_sender(&mut c, "h/Partitions/1").await.unwrap();
    for i in 0..3 {
        send_one(&mut c, s, &event(&format!("e{i}"))).await;
    }
    let (st, v) = call(&env, "GET", "/myns/hubs/h/partitions/1", "").await;
    assert_eq!(st, 200);
    assert_eq!(v["eventCount"], 3);
    assert_eq!(v["lastEnqueuedSequenceNumber"], 2);
    assert_eq!(v["beginSequenceNumber"], 0);
    assert_eq!(v["isEmpty"], false);
    assert!(v["sizeBytes"].as_u64().unwrap() > 0);
    assert!(v["lastEnqueuedTimeUtc"].as_str().unwrap().ends_with('Z'));
    let (_, v) = call(&env, "GET", "/myns/hubs/h/partitions", "").await;
    assert_eq!(v["partitions"][0]["eventCount"], 0);
    assert_eq!(v["partitions"][1]["totalEventsReceived"], 3);
    let (_, v) = call(&env, "GET", "/myns/hubs/h", "").await;
    assert_eq!(v["receivers"], 0);
    assert_eq!(call(&env, "GET", "/myns/hubs/h/partitions/5", "").await.0, 404);
}

#[tokio::test]
async fn hubs_made_over_http_are_visible_to_amqp_and_the_other_way_round() {
    let env = env().await;
    call(&env, "PUT", "/myns/hubs/viahttp", r#"{"partitionCount":2}"#).await;
    let mut c = env.client().await;
    attach_sender(&mut c, "viahttp/Partitions/1").await.unwrap();
    assert_eq!(condition(attach_sender(&mut c, "viahttp/Partitions/2").await.unwrap_err()), "com.microsoft:argument-out-of-range");
    attach_sender(&mut c, "viaamqp").await.unwrap();
    let (_, v) = call(&env, "GET", "/myns/hubs/viaamqp", "").await;
    assert_eq!(v["autoCreated"], true);
    assert_eq!(v["partitionCount"], 4);
}

#[tokio::test]
async fn unbuilt_routes_answer_501_and_the_atom_api_is_named() {
    let env = env().await;
    let (st, v) = call(&env, "POST", "/myns/hubs/h/events", "").await;
    assert_eq!(st, 501);
    assert_eq!(v["error"]["code"], "NotImplemented");
    assert!(v["error"]["message"].as_str().unwrap().contains("POST /_azemu/eventhubs/myns/hubs/h/events"));
    let r = env.srv.send("GET", &format!("{}/orders?api-version=2017-04", env.srv.url("eventhub", "myns")), &[], "").await;
    assert_eq!(r.status, 501);
    assert!(r.text().contains("not built"));
}
