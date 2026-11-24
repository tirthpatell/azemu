//! `Microsoft.EventHub/namespaces/schemagroups`: the groups of the Schema Registry, made through Resource Manager, as on Azure.

mod common;

use common::{full, SUB};
use serde_json::json;

fn rid(rest: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/eh/providers/Microsoft.EventHub/{rest}")
}

#[tokio::test]
async fn a_schema_group_made_through_resource_manager_serves_the_registry() {
    let a = full().await;
    a.group("eh").await;
    let r = a.put(&rid("namespaces/ehns02"), json!({"location": "eastus", "sku": {"name": "Standard"}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let g = rid("namespaces/ehns02/schemagroups/orders");
    let r = a
        .put(&g, json!({"properties": {"schemaType": "Avro", "schemaCompatibility": "Forward", "groupProperties": {"owner": "team"}}}))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let j = r.json();
    assert_eq!(j["type"], "Microsoft.EventHub/Namespaces/Schemagroups");
    assert_eq!((j["properties"]["schemaType"].as_str(), j["properties"]["schemaCompatibility"].as_str()), (Some("Avro"), Some("Forward")));
    assert_eq!(a.get(&g).await.json()["properties"]["groupProperties"], json!({"owner": "team"}));
    assert_eq!(a.get(&rid("namespaces/ehns02/schemagroups")).await.json()["value"].as_array().map(Vec::len), Some(1));

    // the registry on the namespace host has the group, with its mode
    let base = format!("http://ehns02.eventhub.localhost:{}", a.srv.port());
    let list = a.dp_send("GET", &format!("{base}/$schemaGroups?api-version=2023-07-01"), &[], "").await;
    assert_eq!(list.json()["Value"], json!(["orders"]));
    let put = a
        .dp_send(
            "PUT",
            &format!("{base}/$schemaGroups/orders/schemas/s?api-version=2023-07-01"),
            &[("Content-Type", "application/json; serialization=Avro")],
            r#"{"type":"record","name":"R","fields":[{"name":"a","type":"string"}]}"#,
        )
        .await;
    assert_eq!((put.status, put.header("schema-version")), (204, Some("1")));

    // a bad value is a 400; the group goes with the ARM resource, its schemas with it
    let r = a.put(&rid("namespaces/ehns02/schemagroups/bad"), json!({"properties": {"schemaCompatibility": "Sideways"}})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    let d = a.delete(&g).await;
    assert!(d.status == 200 || d.status == 204, "{}", d.text());
    let list = a.dp_send("GET", &format!("{base}/$schemaGroups?api-version=2023-07-01"), &[], "").await;
    assert_eq!(list.json()["Value"], json!([]));
}
