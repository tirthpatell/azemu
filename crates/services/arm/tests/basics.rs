mod common;

use common::{standalone, SUB};
use serde_json::json;

#[tokio::test]
async fn api_version_is_required_and_auth_too() {
    let a = standalone().await;
    let r = a.srv.send("GET", &format!("{}/subscriptions", a.base()), &[("Authorization", "Bearer x")], "").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "MissingApiVersionParameter");
    let r = a.srv.send("GET", &format!("{}/subscriptions?api-version=2022-12-01", a.base()), &[], "").await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["error"]["code"], "AuthenticationFailed");
    assert!(r.header("www-authenticate").is_some());
}

#[tokio::test]
async fn headers_on_every_answer() {
    let a = standalone().await;
    let r = a.get("/subscriptions").await;
    assert_eq!(r.status, 200);
    assert!(r.header("x-ms-request-id").is_some());
    assert!(r.header("x-ms-correlation-request-id").is_some());
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    assert!(r.header("content-type").unwrap().starts_with("application/json"));
}

#[tokio::test]
async fn subscriptions_tenants_locations_providers() {
    let a = standalone().await;
    let r = a.get("/subscriptions").await.json();
    assert!(!r["value"].as_array().unwrap().is_empty());
    let r = a.get(&format!("/subscriptions/{SUB}")).await.json();
    assert_eq!(r["subscriptionId"], SUB);
    assert_eq!(r["id"], format!("/subscriptions/{SUB}"));
    assert_eq!(r["state"], "Enabled");
    let r = a.get("/tenants").await.json();
    assert!(r["value"][0]["tenantId"].is_string());
    let r = a.get(&format!("/subscriptions/{SUB}/locations")).await.json();
    assert!(r["value"].as_array().unwrap().iter().any(|l| l["name"] == "eastus"));
    let r = a.get(&format!("/subscriptions/{SUB}/providers")).await.json();
    assert!(r["value"].as_array().unwrap().iter().any(|p| p["namespace"] == "Microsoft.Storage"));
    let r = a.get(&format!("/subscriptions/{SUB}/providers/microsoft.storage")).await;
    assert_eq!(r.json()["namespace"], "Microsoft.Storage");
    assert_eq!(r.json()["registrationState"], "Registered");
    let r = a.post(&format!("/subscriptions/{SUB}/providers/Microsoft.Storage/register"), json!({})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["registrationState"], "Registered");
    // an unused subscription id shows up once it has been used
    let r = a.get("/subscriptions").await.json();
    assert!(r["value"].as_array().unwrap().iter().any(|s| s["subscriptionId"] == SUB));
}

#[tokio::test]
async fn resource_group_lifecycle() {
    let a = standalone().await;
    let p = format!("/subscriptions/{SUB}/resourcegroups/my-rg");
    assert_eq!(a.call("HEAD", &p, None).await.status, 404);
    let r = a.get(&p).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["code"], "ResourceGroupNotFound");
    let r = a.put(&p, json!({})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "LocationRequired");
    let r = a.put(&p, json!({"location": "East US", "tags": {"a": "1"}})).await;
    assert_eq!(r.status, 201);
    let j = r.json();
    assert_eq!(j["id"], format!("/subscriptions/{SUB}/resourceGroups/my-rg"));
    assert_eq!(j["name"], "my-rg");
    assert_eq!(j["type"], "Microsoft.Resources/resourceGroups");
    assert_eq!(j["location"], "eastus");
    assert_eq!(j["properties"]["provisioningState"], "Succeeded");
    assert_eq!(j["tags"]["a"], "1");
    // second PUT is an update
    assert_eq!(a.put(&p, json!({"location": "eastus", "tags": {"b": "2"}})).await.status, 200);
    let r = a.put(&p, json!({"location": "westus"})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json()["error"]["code"], "InvalidResourceGroupLocation");
    assert_eq!(a.call("HEAD", &p, None).await.status, 204);
    let r = a.call("PATCH", &p, Some(json!({"tags": {"c": "3"}}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["tags"], json!({"c": "3"}));
    let r = a.get(&format!("/subscriptions/{SUB}/resourcegroups")).await.json();
    assert_eq!(r["value"].as_array().unwrap().len(), 1);
    let r = a.get(&format!("/subscriptions/{SUB}/resourcegroups?$filter=tagName%20eq%20%27c%27")).await.json();
    assert_eq!(r["value"].as_array().unwrap().len(), 1);
    let r = a.get(&format!("/subscriptions/{SUB}/resourcegroups?$filter=tagName%20eq%20%27zzz%27")).await.json();
    assert_eq!(r["value"].as_array().unwrap().len(), 0);
    let r = a.delete(&p).await;
    assert_eq!(r.status, 202);
    let loc = r.header("location").unwrap().to_string();
    assert!(loc.contains("/operationresults/"));
    let r = a.srv.send("GET", &loc, &[("Authorization", "Bearer x")], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(a.get(&p).await.status, 404);
    assert_eq!(a.delete(&p).await.status, 404);
}

#[tokio::test]
async fn invalid_group_name() {
    let a = standalone().await;
    let r = a.put(&format!("/subscriptions/{SUB}/resourcegroups/bad%20name"), json!({"location": "eastus"})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "InvalidResourceGroup");
}

#[tokio::test]
async fn generic_resources_and_listing() {
    let a = standalone().await;
    a.group("rg1").await;
    let base = format!("/subscriptions/{SUB}/resourceGroups/rg1/providers/Microsoft.Compute/availabilitySets");
    let r = a
        .put(&format!("{base}/as1"), json!({"location": "eastus", "tags": {"k": "v"}, "properties": {"platformFaultDomainCount": 3}}))
        .await;
    assert_eq!(r.status, 201);
    let j = r.json();
    assert_eq!(j["name"], "as1");
    assert_eq!(j["type"], "Microsoft.Compute/availabilitySets");
    assert_eq!(j["properties"]["provisioningState"], "Succeeded");
    assert_eq!(j["properties"]["platformFaultDomainCount"], 3);
    assert_eq!(
        a.put(&format!("{base}/as1"), json!({"location": "eastus", "tags": {"k": "v"}, "properties": {"platformFaultDomainCount": 3}}))
            .await
            .status,
        200
    );
    // double slash, as in Microsoft's own recordings
    let r = a.get(&format!("/subscriptions/{SUB}/resourceGroups/rg1/providers/Microsoft.Compute//availabilitySets/as1")).await;
    assert_eq!(r.status, 200);
    let r = a.get(&base).await.json();
    assert_eq!(r["value"].as_array().unwrap().len(), 1);
    let r = a.get(&format!("/subscriptions/{SUB}/resourceGroups/rg1/resources")).await.json();
    assert_eq!(r["value"][0]["name"], "as1");
    assert_eq!(r["value"][0]["tags"]["k"], "v");
    let r =
        a.get(&format!("/subscriptions/{SUB}/resources?$filter=resourceType%20eq%20%27Microsoft.Compute/availabilitySets%27")).await.json();
    assert_eq!(r["value"].as_array().unwrap().len(), 1);
    let r =
        a.get(&format!("/subscriptions/{SUB}/resources?$filter=resourceType%20eq%20%27Microsoft.Network/virtualNetworks%27")).await.json();
    assert_eq!(r["value"].as_array().unwrap().len(), 0);
    let r = a.call("PATCH", &format!("{base}/as1"), Some(json!({"tags": {"z": "1"}}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["tags"], json!({"z": "1"}));
    assert_eq!(r.json()["properties"]["platformFaultDomainCount"], 3);
    assert_eq!(a.delete(&format!("{base}/as1")).await.status, 200);
    let r = a.get(&format!("{base}/as1")).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["code"], "ResourceNotFound");
    assert_eq!(a.delete(&format!("{base}/as1")).await.status, 204);
    // no group
    let r = a
        .put(
            &format!("/subscriptions/{SUB}/resourceGroups/nope/providers/Microsoft.Compute/availabilitySets/x"),
            json!({"location": "eastus"}),
        )
        .await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["code"], "ResourceGroupNotFound");
}

#[tokio::test]
async fn deleting_a_group_removes_its_resources() {
    let a = standalone().await;
    a.group("rg2").await;
    let base = format!("/subscriptions/{SUB}/resourceGroups/rg2/providers/Microsoft.Network/virtualNetworks/v1");
    assert_eq!(a.put(&base, json!({"location": "eastus"})).await.status, 201);
    assert_eq!(a.delete(&format!("/subscriptions/{SUB}/resourceGroups/rg2")).await.status, 202);
    a.group("rg2").await;
    assert_eq!(a.get(&base).await.status, 404);
    let r = a.get(&format!("/subscriptions/{SUB}/resources")).await.json();
    assert_eq!(r["value"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn strict_mode_refuses_unknown_types() {
    let a = common::strict().await;
    a.group("rg3").await;
    let r = a.put(&format!("/subscriptions/{SUB}/resourceGroups/rg3/providers/Microsoft.Foo/bars/b"), json!({"location": "eastus"})).await;
    assert_eq!(r.status, 501);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("Microsoft.Foo/bars"));
}

#[tokio::test]
async fn tags_at_scopes() {
    let a = standalone().await;
    a.group("rg4").await;
    let scope = format!("/subscriptions/{SUB}/resourceGroups/rg4/providers/Microsoft.Resources/tags/default");
    let r = a.put(&scope, json!({"properties": {"tags": {"x": "1"}}})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["properties"]["tags"]["x"], "1");
    let r = a.call("PATCH", &scope, Some(json!({"operation": "Merge", "properties": {"tags": {"y": "2"}}}))).await;
    assert_eq!(r.json()["properties"]["tags"], json!({"x": "1", "y": "2"}));
    let g = a.get(&format!("/subscriptions/{SUB}/resourcegroups/rg4")).await.json();
    assert_eq!(g["tags"], json!({"x": "1", "y": "2"}));
    let r = a.delete(&scope).await;
    assert_eq!(r.status, 200);
    let r = a.get(&scope).await.json();
    assert_eq!(r["properties"]["tags"], json!({}));
}

#[tokio::test]
async fn reset_empties_everything() {
    let a = standalone().await;
    a.group("rg5").await;
    a.srv.reset();
    assert_eq!(a.get(&format!("/subscriptions/{SUB}/resourcegroups/rg5")).await.status, 404);
}

/// `POST /_azemu/reset?service=arm&account=<subscription id>`: one subscription goes, the others stay.
#[tokio::test]
async fn reset_of_one_subscription_leaves_the_others() {
    let a = standalone().await;
    let other = "99999999-8888-7777-6666-555555555555";
    for sub in [SUB, other] {
        let g = format!("/subscriptions/{sub}/resourcegroups/rg");
        assert_eq!(a.put(&g, json!({"location": "eastus", "tags": {"t": "1"}})).await.status, 201);
        let r = a.put(&format!("{g}/providers/Microsoft.Network/virtualNetworks/v1"), json!({"location": "eastus"})).await;
        assert_eq!(r.status, 201, "{}", r.text());
        let r = a
            .put(&format!("/subscriptions/{sub}/providers/Microsoft.Resources/tags/default"), json!({"properties": {"tags": {"s": "1"}}}))
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
    }
    let url = format!("{}/_azemu/reset?service=arm&account={}", a.srv.gateway_url(), SUB.to_uppercase());
    let r = a.srv.send("POST", &url, &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["reset"], json!(["arm"]));
    // the subscription is empty: no group, no resource, no tags, and it is no longer listed
    assert_eq!(a.get(&format!("/subscriptions/{SUB}/resourcegroups/rg")).await.status, 404);
    assert_eq!(a.get(&format!("/subscriptions/{SUB}/resourcegroups")).await.json()["value"], json!([]));
    assert_eq!(a.get(&format!("/subscriptions/{SUB}/resources")).await.json()["value"], json!([]));
    assert_eq!(
        a.get(&format!("/subscriptions/{SUB}/providers/Microsoft.Resources/tags/default")).await.json()["properties"]["tags"],
        json!({})
    );
    // the other one is as it was
    assert_eq!(a.get(&format!("/subscriptions/{other}/resourcegroups/rg")).await.json()["tags"], json!({"t": "1"}));
    assert_eq!(
        a.get(&format!("/subscriptions/{other}/resourcegroups/rg/providers/Microsoft.Network/virtualNetworks/v1")).await.status,
        200
    );
    assert_eq!(
        a.get(&format!("/subscriptions/{other}/providers/Microsoft.Resources/tags/default")).await.json()["properties"]["tags"],
        json!({"s": "1"})
    );
    // the emptied subscription can be used again; a subscription never used is not an error
    assert_eq!(a.put(&format!("/subscriptions/{SUB}/resourcegroups/rg"), json!({"location": "eastus"})).await.status, 201);
    let url = format!("{}/_azemu/reset?service=arm&account=00000000-0000-0000-0000-000000000000", a.srv.gateway_url());
    assert_eq!(a.srv.send("POST", &url, &[], "").await.status, 200);
}
