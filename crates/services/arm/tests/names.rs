//! Names the providers accept (Microsoft Learn, "Naming rules and restrictions for Azure resources"), and limits on what is stored.

mod common;

use common::{full, SUB};
use serde_json::json;

fn ns(rg: &str, name: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/{rg}/providers/Microsoft.EventHub/namespaces/{name}")
}

#[tokio::test]
async fn event_hub_names_follow_the_naming_rules() {
    let a = full().await;
    a.group("rg").await;
    assert_eq!(a.put(&ns("rg", "ehns-names"), json!({"location": "eastus", "sku": {"name": "Standard"}})).await.status, 200);
    let hub = |n: &str| format!("{}/eventhubs/{n}", ns("rg", "ehns-names"));
    assert_eq!(a.put(&hub("orders.v1_a-b"), json!({"properties": {}})).await.status, 200);
    for bad in ["-x", "x-", "a%20b", "a%2Fb", "a$b", "..", "a%3Fb"] {
        let r = a.put(&hub(bad), json!({"properties": {}})).await;
        assert_eq!(r.status, 400, "{bad}: {}", r.text());
    }
    assert_eq!(a.put(&hub(&"h".repeat(257)), json!({"properties": {}})).await.status, 400);
    assert_eq!(a.put(&hub(&"h".repeat(256)), json!({"properties": {}})).await.status, 200);
    let cg = |n: &str| format!("{}/consumergroups/{n}", hub("orders.v1_a-b"));
    assert_eq!(a.put(&cg("workers"), json!({"properties": {}})).await.status, 200);
    assert_eq!(a.put(&cg(&"c".repeat(51)), json!({"properties": {}})).await.status, 400);
    assert_eq!(a.put(&cg("bad%20name"), json!({"properties": {}})).await.status, 400);
}

#[tokio::test]
async fn app_configuration_store_names_follow_the_naming_rules() {
    let a = full().await;
    a.group("rg").await;
    let store = |n: &str| format!("/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.AppConfiguration/configurationStores/{n}");
    let body = json!({"location": "eastus", "sku": {"name": "free"}});
    for good in ["abcde", "my-store-1", "a--b-cd"] {
        assert_eq!(a.put(&store(good), body.clone()).await.status, 201, "{good}");
    }
    for bad in ["abcd", "-abcde", "abcde-", "a---bcde", "my_store", "my.store", &"x".repeat(51)] {
        assert_eq!(a.put(&store(bad), body.clone()).await.status, 400, "{bad}");
    }
}

#[tokio::test]
async fn tags_are_limited_to_50_names_of_512_and_values_of_256_characters() {
    let a = full().await;
    let g = format!("/subscriptions/{SUB}/resourcegroups/tg");
    let tags = |n: usize| serde_json::Map::from_iter((0..n).map(|i| (format!("t{i}"), json!("v"))));
    assert_eq!(a.put(&g, json!({"location": "eastus", "tags": tags(50)})).await.status, 201);
    assert_eq!(a.put(&g, json!({"location": "eastus", "tags": tags(51)})).await.status, 400);
    assert_eq!(a.put(&g, json!({"location": "eastus", "tags": {"n": "x".repeat(257)}})).await.status, 400);
    assert_eq!(a.put(&g, json!({"location": "eastus", "tags": {"x".repeat(513): "v"}})).await.status, 400);
    let scope = format!("{g}/providers/Microsoft.Resources/tags/default");
    assert_eq!(a.put(&scope, json!({"properties": {"tags": tags(50)}})).await.status, 200);
    assert_eq!(a.put(&scope, json!({"properties": {"tags": tags(51)}})).await.status, 400);
    // a merge that goes over the limit is refused, and nothing changed
    let r = a.call("PATCH", &scope, Some(json!({"operation": "Merge", "properties": {"tags": {"one-too-many": "v"}}}))).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(a.get(&scope).await.json()["properties"]["tags"].as_object().unwrap().len(), 50);
}
