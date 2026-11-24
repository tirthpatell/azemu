//! `Microsoft.Insights/components` and `Microsoft.OperationalInsights/workspaces` made through Resource Manager: the connection string
//! of a component leads to the monitor service, and the telemetry sent with it can be read back.

mod common;

use common::{full, SUB};
use serde_json::{json, Value};

fn component(name: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/ai/providers/Microsoft.Insights/components/{name}")
}

fn workspace(name: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/ai/providers/Microsoft.OperationalInsights/workspaces/{name}")
}

fn is_guid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok() && s.len() == 36
}

/// `key=value;key=value` into a lookup.
fn part<'a>(connection_string: &'a str, name: &str) -> &'a str {
    connection_string
        .split(';')
        .find_map(|p| p.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {connection_string}"))
}

#[tokio::test]
async fn the_connection_string_of_a_component_sends_telemetry_to_the_monitor_service() {
    let a = full().await;
    a.group("ai").await;
    let ws = workspace("logs01");
    let r = a.put(&ws, json!({"location": "eastus2", "properties": {"sku": {"name": "pergb2018"}, "retentionInDays": 30}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let w = r.json();
    assert_eq!(w["type"], "Microsoft.OperationalInsights/workspaces");
    assert_eq!(w["properties"]["sku"]["name"], "PerGB2018", "the name is written with Azure's capitals");
    assert_eq!(w["properties"]["provisioningState"], "Succeeded");
    assert!(is_guid(w["properties"]["customerId"].as_str().unwrap()));
    assert_eq!(w["properties"]["features"]["searchVersion"], 1);
    assert_eq!(a.get(&ws).await.json()["properties"]["customerId"], w["properties"]["customerId"], "the customer id is stable");
    let keys = a.post(&format!("{ws}/sharedKeys"), json!({})).await;
    assert_eq!(keys.status, 200, "{}", keys.text());
    let k1 = keys.json();
    assert_eq!(k1["primarySharedKey"].as_str().map(str::len), Some(88));
    assert_eq!(a.post(&format!("{ws}/sharedKeys"), json!({})).await.json(), k1, "the keys do not change");
    assert_ne!(k1["primarySharedKey"], k1["secondarySharedKey"]);

    let c = component("app01");
    let r = a
        .put(
            &c,
            json!({
                "location": "eastus2", "kind": "web", "tags": {"team": "a"},
                "properties": {"Application_Type": "web", "WorkspaceResourceId": ws, "RetentionInDays": 90}
            }),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let j = r.json();
    // the shape of the recorded answer
    assert_eq!(j["type"], "microsoft.insights/components");
    assert_eq!(j["kind"], "web");
    assert!(j["etag"].as_str().unwrap().starts_with('"'));
    let p = &j["properties"];
    let (ikey, app_id) = (p["InstrumentationKey"].as_str().unwrap(), p["AppId"].as_str().unwrap());
    assert!(is_guid(ikey) && is_guid(app_id) && ikey != app_id);
    assert_eq!((p["Name"].as_str(), p["ApplicationId"].as_str()), (Some("app01"), Some("app01")));
    assert_eq!(
        (p["IngestionMode"].as_str(), p["Retention"].as_str(), p["RetentionInDays"].as_u64()),
        (Some("LogAnalytics"), Some("P90D"), Some(90))
    );
    assert_eq!((p["Ver"].as_str(), p["provisioningState"].as_str()), (Some("v2"), Some("Succeeded")));
    assert_eq!(p["WorkspaceResourceId"], json!(ws));
    assert_eq!(p["SamplingPercentage"].as_f64(), Some(100.0));
    assert!(p["CreationDate"].as_str().unwrap().ends_with("+00:00"));

    // the connection string names the emulator's own endpoints
    let port = a.srv.port();
    let cs = p["ConnectionString"].as_str().unwrap();
    assert_eq!(part(cs, "InstrumentationKey"), ikey);
    assert_eq!(part(cs, "ApplicationId"), app_id);
    assert_eq!(part(cs, "IngestionEndpoint"), format!("http://eastus2.in.localhost:{port}/"));
    assert_eq!(part(cs, "LiveEndpoint"), format!("http://eastus2.livediagnostics.localhost:{port}/"));

    // a GET and a second PUT give the same keys
    let g = a.get(&c).await.json();
    assert_eq!(g["properties"]["InstrumentationKey"], json!(ikey));
    assert_eq!(g["properties"]["ConnectionString"], json!(cs));
    let again = a.put(&c, json!({"location": "eastus2", "properties": {"WorkspaceResourceId": ws}})).await.json();
    assert_eq!(again["properties"]["InstrumentationKey"], json!(ikey));
    assert_eq!(again["properties"]["AppId"], json!(app_id));

    // send one envelope to the endpoint of the connection string, read it back
    let now = azemu_core::time::iso8601_7(azemu_core::time::OffsetDateTime::now_utc());
    let envelope = json!({
        "ver": 1, "name": "Microsoft.ApplicationInsights.Event", "time": now, "iKey": ikey,
        "tags": {"ai.operation.id": "op-from-arm"},
        "data": {"baseType": "EventData", "baseData": {"ver": 2, "name": "hello from a component"}},
    });
    let sent = a
        .srv
        .send(
            "POST",
            &format!("{}v2/track", part(cs, "IngestionEndpoint")),
            &[("content-type", "application/json")],
            json!([envelope]).to_string(),
        )
        .await;
    assert_eq!(sent.status, 200, "{}", sent.text());
    assert_eq!(sent.json()["itemsAccepted"], 1);
    let back = a.srv.send("GET", &format!("{}/_azemu/monitor/telemetry?ikey={ikey}", a.srv.gateway_url()), &[], "").await.json();
    assert_eq!(back["count"], 1, "{back}");
    assert_eq!(back["items"][0]["name"], "hello from a component");
    assert_eq!(back["items"][0]["operationId"], "op-from-arm");
    // the application id lookup of the SDKs answers the id of the component
    let looked = a.srv.send("GET", &format!("{}api/profiles/{ikey}/appId", part(cs, "IngestionEndpoint")), &[], "").await;
    assert_eq!((looked.status, looked.text()), (200, app_id.to_string()));

    // list by group and by subscription, tags by PATCH
    let by_group = a.get(&format!("/subscriptions/{SUB}/resourceGroups/ai/providers/Microsoft.Insights/components")).await.json();
    assert_eq!(by_group["value"].as_array().map(Vec::len), Some(1));
    let by_sub = a.get(&format!("/subscriptions/{SUB}/providers/Microsoft.Insights/components")).await.json();
    assert_eq!(by_sub["value"][0]["name"], "app01");
    let patched = a.call("PATCH", &c, Some(json!({"tags": {"team": "b"}}))).await;
    assert_eq!(patched.status, 200, "{}", patched.text());
    let g = a.get(&c).await.json();
    assert_eq!(g["tags"], json!({"team": "b"}));
    assert_eq!(g["properties"]["InstrumentationKey"], json!(ikey));

    // a type of the namespace that is not built is kept as JSON, as before
    let ag = a
        .put(
            &format!("/subscriptions/{SUB}/resourceGroups/ai/providers/Microsoft.Insights/actionGroups/ag1"),
            json!({"location": "global", "properties": {"groupShortName": "ag"}}),
        )
        .await;
    assert_eq!(ag.status, 200, "{}", ag.text());
    assert_eq!(ag.header("x-azemu-stored-only"), Some("true"));

    assert_eq!(a.delete(&c).await.status, 200);
    assert_eq!(a.get(&c).await.status, 404);
    assert_eq!(a.delete(&ws).await.status, 200);
    assert_eq!(a.get(&ws).await.status, 404);
}

#[tokio::test]
async fn a_component_without_a_workspace_and_the_names_that_are_refused() {
    let a = full().await;
    a.group("ai").await;
    let r = a.put(&component("plain"), json!({"location": "eastus", "kind": "other", "properties": {"Application_Type": "other"}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let j: Value = r.json();
    assert_eq!(j["kind"], "other");
    assert_eq!(j["properties"]["IngestionMode"], "ApplicationInsights");
    assert!(j["properties"]["ConnectionString"].as_str().unwrap().contains("eastus.in.localhost"));
    // two components of different groups have different keys
    a.group("ai2").await;
    let other = a
        .put(
            &format!("/subscriptions/{SUB}/resourceGroups/ai2/providers/Microsoft.Insights/components/plain"),
            json!({"location": "eastus", "properties": {}}),
        )
        .await
        .json();
    assert_ne!(other["properties"]["InstrumentationKey"], j["properties"]["InstrumentationKey"]);
    // names and values Azure refuses
    assert_eq!(a.put(&workspace("a"), json!({"location": "eastus"})).await.status, 400);
    assert_eq!(a.put(&workspace("-bad-"), json!({"location": "eastus"})).await.status, 400);
    let r = a.put(&workspace("goodname"), json!({"location": "eastus", "properties": {"sku": {"name": "Nope"}}})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    let r = a.put(&component("old"), json!({"location": "eastus", "properties": {"RetentionInDays": 3}})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    // no location: refused as for every top-level resource
    assert_eq!(a.put(&component("nowhere"), json!({"properties": {}})).await.status, 400);
}
