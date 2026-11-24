//! The inspector of Resource Manager (`/_azemu/inspect/arm/...`).

mod common;

use azemu_core::inspect::walk::walk;
use common::{standalone, Arm, SUB};
use serde_json::json;

async fn fill(a: &Arm) {
    a.group("rg1").await;
    a.group("rg2").await;
    let rg = format!("/subscriptions/{SUB}/resourcegroups/rg1/providers");
    for name in ["acct1", "acct2", "acct3"] {
        let r = a
            .put(
                &format!("{rg}/Microsoft.Storage/storageAccounts/{name}"),
                json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2", "properties": {}}),
            )
            .await;
        assert!(r.status == 200 || r.status == 201, "{}", r.text());
    }
    let r = a.put(&format!("{rg}/Microsoft.Web/sites/myfunc"), json!({"location": "eastus", "properties": {"x": 1}})).await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text());
    // a deployment that works and one that fails
    let ok = json!({"properties": {"mode": "Incremental", "template": {
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#", "contentVersion": "1.0.0.0",
        "resources": [{"type": "Microsoft.Storage/storageAccounts", "apiVersion": "2023-01-01", "name": "fromtemplate", "location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2", "properties": {}}]}}});
    let r = a.put(&format!("{rg}/Microsoft.Resources/deployments/good"), ok).await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text());
    let bad = json!({"properties": {"mode": "Incremental", "template": {
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#", "contentVersion": "1.0.0.0",
        "resources": [{"type": "Microsoft.Storage/storageAccounts", "apiVersion": "2023-01-01", "name": "INVALID_NAME!", "location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2", "properties": {}}]}}});
    let _ = a.put(&format!("{rg}/Microsoft.Resources/deployments/broken"), bad).await;
    // a second subscription
    let other = "99999999-0000-0000-0000-000000000000";
    a.put(&format!("/subscriptions/{other}/resourcegroups/rgx"), json!({"location": "westus"})).await;
}

async fn insp(a: &Arm, p: &str) -> azemu_core::testing::TestResponse {
    a.srv.send("GET", &format!("{}/_azemu/inspect{p}", a.srv.gateway_url()), &[], "").await
}

#[tokio::test]
async fn the_whole_tree_can_be_walked_and_paging_ends() {
    let a = standalone().await;
    fill(&a).await;
    for limit in [1, 2, 50] {
        let nodes = walk(&a.srv, "/arm", limit).await;
        let kinds: Vec<&str> = nodes.iter().map(|n| n.node["kind"].as_str().unwrap()).collect();
        for k in [
            "service",
            "subscription",
            "resourcegroups",
            "resourcegroup",
            "resources",
            "resource",
            "deployments",
            "deployment",
            "operations",
        ] {
            assert!(kinds.contains(&k), "no {k} among {kinds:?}");
        }
    }
}

#[tokio::test]
async fn resources_show_the_stored_only_flag_and_deployments_their_operations_and_errors() {
    let a = standalone().await;
    fill(&a).await;
    let base = format!("/arm/{SUB}/resourcegroups/rg1");
    let rows = insp(&a, &format!("{base}/resources")).await.json();
    let by_name = |n: &str| rows["items"].as_array().unwrap().iter().find(|r| r["name"] == n).cloned().unwrap_or_else(|| panic!("no {n}"));
    assert_eq!(by_name("acct1")["storedOnly"], false);
    assert_eq!(by_name("myfunc")["storedOnly"], true);
    assert_eq!(by_name("fromtemplate")["type"], "Microsoft.Storage/storageAccounts");
    let only = insp(&a, &format!("{base}/resources?type=web")).await.json();
    assert_eq!(only["items"].as_array().unwrap().len(), 1);
    let one = insp(&a, by_name("acct2")["path"].as_str().unwrap()).await.json();
    assert_eq!(one["properties"]["resource"]["kind"], "StorageV2");
    let deps = insp(&a, &format!("{base}/deployments")).await.json();
    let state = |n: &str| deps["items"].as_array().unwrap().iter().find(|d| d["name"] == n).map(|d| d["provisioningState"].clone());
    assert_eq!(state("good"), Some(json!("Succeeded")));
    assert_eq!(state("broken"), Some(json!("Failed")));
    let ops = insp(&a, &format!("{base}/deployments/good/operations")).await.json();
    assert!(!ops["items"].as_array().unwrap().is_empty());
    let broken = insp(&a, &format!("{base}/deployments/broken")).await.json();
    assert!(broken["properties"]["error"].is_object(), "{broken}");
    // two subscriptions
    assert_eq!(insp(&a, "/arm").await.json()["properties"]["accounts"], 3);
}

#[tokio::test]
async fn looking_makes_no_subscription_or_group_and_changes_nothing() {
    let a = standalone().await;
    fill(&a).await;
    let view = || async {
        (a.get(&format!("/subscriptions/{SUB}/resourcegroups/rg1/resources")).await.text(), a.get("/subscriptions").await.text())
    };
    let before = view().await;
    walk(&a.srv, "", 1).await;
    assert_eq!(before, view().await);
    for p in [
        "/arm/12345678-0000-0000-0000-000000000000",
        &format!("/arm/{SUB}/resourcegroups/none"),
        &format!("/arm/{SUB}/resourcegroups/rg1/resources/nothing"),
        &format!("/arm/{SUB}/other"),
    ] {
        assert_eq!(insp(&a, p).await.status, 404, "{p}");
    }
    // a path that names a subscription nobody used did not create it
    assert_eq!(insp(&a, "/arm").await.json()["properties"]["accounts"], 3);
}
