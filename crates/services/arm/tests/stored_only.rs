//! Resource types nothing emulates are stored as JSON and say so: the header `x-azemu-stored-only: true` on
//! every answer for them, a count in `GET /_azemu/unsupported`, and a name for the operation of every 501.

mod common;

use common::{standalone, strict, Arm, SUB};
use serde_json::{json, Value};

async fn unsupported(a: &Arm) -> Value {
    let r = a.srv.send("GET", &format!("{}/_azemu/unsupported", a.srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()
}

fn stored_only_count(v: &Value, ty: &str) -> Option<u64> {
    v["storedOnly"].as_array().unwrap().iter().find(|e| e["resourceType"] == ty).and_then(|e| e["count"].as_u64())
}

#[tokio::test]
async fn a_stored_only_resource_says_so_on_every_answer_and_is_listed() {
    let a = standalone().await;
    a.group("rg").await;
    let site = format!("/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.Web/sites/myfunc");
    let r = a.put(&site, json!({"location": "eastus", "kind": "functionapp"})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["provisioningState"], "Succeeded");
    assert_eq!(r.header("x-azemu-stored-only"), Some("true"));
    // read, list, patch, head, delete, and a read of one that is not there
    for (m, path, body) in [
        ("GET", site.clone(), None),
        ("HEAD", site.clone(), None),
        ("PATCH", site.clone(), Some(json!({"tags": {"a": "b"}}))),
        ("GET", format!("/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.Web/sites"), None),
        ("GET", format!("/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.Web/sites/other"), None),
        ("DELETE", site.clone(), None),
    ] {
        let r = a.call(m, &path, body).await;
        assert_eq!(r.header("x-azemu-stored-only"), Some("true"), "{m} {path}: {} {}", r.status, r.text());
    }
    // the group itself and a resource an emulated provider makes carry no such header
    let r = a.get(&format!("/subscriptions/{SUB}/resourceGroups/rg")).await;
    assert!(r.header("x-azemu-stored-only").is_none());
    let r = a
        .put(
            &format!("/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct1"),
            json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"}),
        )
        .await;
    assert!(r.header("x-azemu-stored-only").is_none(), "{} {}", r.status, r.text());

    // listed with the number of writes: one PUT and one PATCH
    let v = unsupported(&a).await;
    assert_eq!(stored_only_count(&v, "Microsoft.Web/sites"), Some(2), "{v}");
    assert_eq!(stored_only_count(&v, "Microsoft.Storage/storageAccounts"), None);
    // clearing the list between two runs
    let r = a.srv.send("DELETE", &format!("{}/_azemu/unsupported", a.srv.gateway_url()), &[], "").await;
    assert_eq!(r.status / 100, 2);
    assert_eq!(stored_only_count(&unsupported(&a).await, "Microsoft.Web/sites"), None);
}

#[tokio::test]
async fn a_deployment_with_a_stored_only_resource_shows_it() {
    let a = standalone().await;
    a.group("rg").await;
    let template = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [{"type": "Microsoft.Web/serverfarms", "apiVersion": "2022-03-01", "name": "plan1", "location": "eastus", "sku": {"name": "Y1"}}]
    });
    let r = a
        .put(
            &format!("/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.Resources/deployments/d1"),
            json!({"properties": {"mode": "Incremental", "template": template}}),
        )
        .await;
    assert!(r.status == 200 || r.status == 201, "{} {}", r.status, r.text());
    assert_eq!(r.header("x-azemu-stored-only"), Some("true"));
    assert_eq!(stored_only_count(&unsupported(&a).await, "Microsoft.Web/serverfarms"), Some(1));
    // a deployment of nothing special has no header
    let template = json!({"$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#", "contentVersion": "1.0.0.0", "resources": []});
    let r = a
        .put(
            &format!("/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.Resources/deployments/d2"),
            json!({"properties": {"mode": "Incremental", "template": template}}),
        )
        .await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text());
    assert!(r.header("x-azemu-stored-only").is_none());
}

#[tokio::test]
async fn strict_mode_stores_nothing_and_says_nothing_is_stored() {
    let a = strict().await;
    a.group("rg").await;
    let r = a.put(&format!("/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.Web/sites/x"), json!({"location": "eastus"})).await;
    assert_eq!(r.status, 501);
    assert!(r.header("x-azemu-stored-only").is_none());
    let v = unsupported(&a).await;
    assert_eq!(stored_only_count(&v, "Microsoft.Web/sites"), None);
    // the 501 is counted under a name that says what was asked
    assert!(
        v["notImplemented"].as_array().unwrap().iter().any(|e| e["service"] == "arm" && e["operation"] == "PUT Microsoft.Web/sites"),
        "{v}"
    );
}

#[tokio::test]
async fn a_501_names_its_operation_without_names_in_it() {
    let a = standalone().await;
    a.group("rg").await;
    // a type of a known provider that is not built, and an action on a type without provider
    let r = a
        .put(
            &format!(
                "/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct1/fileServices/default/shares/s1"
            ),
            json!({}),
        )
        .await;
    let r2 = a.post(&format!("/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.Web/sites/x/restart"), json!({})).await;
    assert_eq!(r2.status, 501, "{}", r2.text());
    let v = unsupported(&a).await;
    let ops: Vec<String> = v["notImplemented"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["service"] == "arm")
        .map(|e| e["operation"].as_str().unwrap().to_string())
        .collect();
    assert!(ops.contains(&"POST Microsoft.Web/sites/restart".to_string()), "{ops:?}");
    if r.status == 501 {
        assert!(ops.contains(&"PUT Microsoft.Storage/storageAccounts/fileServices/shares".to_string()), "{ops:?}");
    }
    assert!(ops.iter().all(|o| !o.contains("acct1") && !o.contains(SUB) && !o.contains("rg")), "{ops:?}");
}
