//! What ARM stores holds no port: after a restart on another port the endpoints in bodies and in deployment
//! outputs name the port of the new run.

mod common;

use common::{full_persistent, SUB};
use serde_json::json;

fn rid(rg: &str, ns: &str, rest: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/{rg}/providers/{ns}/{rest}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn endpoints_follow_the_port_of_the_run_that_answers() {
    let data = std::env::temp_dir().join(format!("azemu-arm-portfree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let (a, _arm, _all) = full_persistent(&data).await;
    let port_a = a.srv.port();
    a.group("rg").await;
    let sb = rid("rg", "Microsoft.ServiceBus", "namespaces/sbport");
    let storage = rid("rg", "Microsoft.Storage", "storageAccounts/storport");
    let cosmos = rid("rg", "Microsoft.DocumentDB", "databaseAccounts/cosport");
    let grid = rid("rg", "Microsoft.EventGrid", "topics/gridport");
    for (path, body) in [
        (&sb, json!({"location": "eastus", "sku": {"name": "Standard"}})),
        (&storage, json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}})),
        (
            &cosmos,
            json!({"location": "eastus", "kind": "GlobalDocumentDB", "properties": {"databaseAccountOfferType": "Standard", "locations": [{"locationName": "East US", "failoverPriority": 0}]}}),
        ),
        (&grid, json!({"location": "eastus"})),
    ] {
        let r = a.put(path, body).await;
        assert!(r.status == 200 || r.status == 201, "{path}: {} {}", r.status, r.text());
    }
    // a deployment whose outputs hold addresses
    let template = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [],
        "outputs": {
            "bus": {"type": "string", "value": "[reference(resourceId('Microsoft.ServiceBus/namespaces', 'sbport'), '2021-11-01').serviceBusEndpoint]"},
            "blob": {"type": "string", "value": "[reference(resourceId('Microsoft.Storage/storageAccounts', 'storport'), '2023-01-01').primaryEndpoints.blob]"},
        }
    });
    let dep = rid("rg", "Microsoft.Resources", "deployments/d1");
    let r = a.put(&dep, json!({"properties": {"mode": "Incremental", "template": template}})).await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text());
    let out = r.json()["properties"]["outputs"].clone();
    assert!(out["bus"]["value"].as_str().unwrap().contains(&format!("sbport.servicebus.localhost:{port_a}")), "{out}");
    assert!(out["blob"]["value"].as_str().unwrap().contains(&format!("storport.blob.core.localhost:{port_a}")), "{out}");
    // the same answers on a read in this run
    let g = a.get(&dep).await.json();
    assert!(g["properties"]["outputs"]["bus"]["value"].as_str().unwrap().contains(&format!(":{port_a}/")), "{g}");
    a.srv.shutdown().await.unwrap();

    let (b, _arm, _all) = full_persistent(&data).await;
    let port_b = b.srv.port();
    assert_ne!(port_a, port_b, "the second run takes another port");
    let has = |text: &str| text.contains(&format!(":{port_b}")) && !text.contains(&format!(":{port_a}/"));
    let r = b.get(&sb).await;
    assert!(
        has(&r.text())
            && r.json()["properties"]["serviceBusEndpoint"].as_str().unwrap().contains(&format!("sbport.servicebus.localhost:{port_b}/")),
        "{}",
        r.text()
    );
    let r = b.get(&storage).await;
    assert!(
        r.json()["properties"]["primaryEndpoints"]["blob"].as_str().unwrap().contains(&format!("storport.blob.core.localhost:{port_b}/")),
        "{}",
        r.text()
    );
    let r = b.get(&cosmos).await;
    let p = &r.json()["properties"];
    assert_eq!(p["documentEndpoint"], format!("https://cosport.documents.localhost:{port_b}/"), "{}", r.text());
    for list in ["locations", "writeLocations", "readLocations"] {
        assert_eq!(p[list][0]["documentEndpoint"], format!("https://cosport.documents.localhost:{port_b}/"), "{list}: {}", r.text());
    }
    let r = b.get(&grid).await;
    assert!(
        r.json()["properties"]["endpoint"].as_str().unwrap().contains(&format!("gridport.eventgrid.localhost:{port_b}/")),
        "{}",
        r.text()
    );
    let g = b.get(&dep).await;
    let out = &g.json()["properties"]["outputs"];
    assert!(out["bus"]["value"].as_str().unwrap().contains(&format!("sbport.servicebus.localhost:{port_b}/")), "{out}");
    assert!(out["blob"]["value"].as_str().unwrap().contains(&format!("storport.blob.core.localhost:{port_b}/")), "{out}");
    assert!(!g.text().contains("azemu-port"), "the placeholder never reaches a client: {}", g.text());
    // the files hold no port
    for e in std::fs::read_dir(data.join("arm")).unwrap() {
        let bytes = std::fs::read(e.unwrap().path()).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(&format!("localhost:{port_a}")), "a state file holds the old port");
    }
    b.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}
