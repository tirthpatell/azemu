mod common;

use common::{full, SUB};
use serde_json::json;

fn rid(rg: &str, ns: &str, rest: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/{rg}/providers/{ns}/{rest}")
}

#[tokio::test]
async fn service_bus_entities_exist_in_the_data_plane() {
    let a = full().await;
    a.group("sb").await;
    let r = a
        .put(
            &rid("sb", "Microsoft.ServiceBus", "namespaces/sbns01"),
            json!({"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text()); // the first PUT of a namespace answers 200 in the recordings of azure-mgmt-servicebus
    let j = r.json();
    assert_eq!(j["type"], "Microsoft.ServiceBus/Namespaces");
    assert_eq!(j["properties"]["serviceBusEndpoint"], format!("https://sbns01.servicebus.localhost:{}/", a.srv.port()));
    // the default rule exists
    let r = a.get(&rid("sb", "Microsoft.ServiceBus", "namespaces/sbns01/authorizationRules")).await.json();
    assert_eq!(r["value"][0]["name"], "RootManageSharedAccessKey");
    let r = a
        .post(&rid("sb", "Microsoft.ServiceBus", "namespaces/sbns01/authorizationRules/RootManageSharedAccessKey/listKeys"), json!({}))
        .await
        .json();
    assert!(r["primaryConnectionString"].as_str().unwrap().contains("SharedAccessKeyName=RootManageSharedAccessKey"));
    assert_eq!(r["primaryKey"].as_str().unwrap().len(), 44);
    let r2 = a
        .post(
            &rid("sb", "Microsoft.ServiceBus", "namespaces/sbns01/authorizationRules/RootManageSharedAccessKey/regenerateKeys"),
            json!({"keyType": "PrimaryKey"}),
        )
        .await
        .json();
    assert_ne!(r2["primaryKey"], r["primaryKey"]);
    assert_eq!(r2["secondaryKey"], r["secondaryKey"]);

    let q = rid("sb", "Microsoft.ServiceBus", "namespaces/sbns01/queues/orders");
    let r = a.put(&q, json!({"properties": {"lockDuration": "PT30S", "maxDeliveryCount": 3, "requiresSession": true}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["properties"]["maxDeliveryCount"], 3);
    assert_eq!(r.json()["type"], "Microsoft.ServiceBus/namespaces/queues");
    // the Service Bus management API sees the queue with the same settings
    let sb = format!("http://sbns01.servicebus.localhost:{}", a.srv.port());
    let r = a.dp_send("GET", &format!("{sb}/orders?api-version=2021-05"), &[], "").await;
    let body = r.text();
    assert!(body.contains("<QueueDescription"), "{body}");
    assert!(body.contains("<LockDuration>PT30S</LockDuration>"), "{body}");
    assert!(body.contains("<MaxDeliveryCount>3</MaxDeliveryCount>"), "{body}");
    assert!(body.contains("<RequiresSession>true</RequiresSession>"), "{body}");

    // topic, subscription, rule
    assert_eq!(a.put(&rid("sb", "Microsoft.ServiceBus", "namespaces/sbns01/topics/events"), json!({"properties": {}})).await.status, 200);
    let s = rid("sb", "Microsoft.ServiceBus", "namespaces/sbns01/topics/events/subscriptions/s1");
    assert_eq!(a.put(&s, json!({"properties": {"maxDeliveryCount": 5}})).await.status, 200);
    let rules = a.get(&format!("{s}/rules")).await.json();
    assert_eq!(rules["value"][0]["name"], "$Default");
    let r = a
        .put(&format!("{s}/rules/red"), json!({"properties": {"filterType": "SqlFilter", "sqlFilter": {"sqlExpression": "color = 'red'"}}}))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = a
        .put(&format!("{s}/rules/bad"), json!({"properties": {"filterType": "SqlFilter", "sqlFilter": {"sqlExpression": "color = = "}}}))
        .await;
    assert_eq!(r.status, 400);
    let r = a.dp_send("GET", &format!("{sb}/events/Subscriptions/s1/Rules?api-version=2021-05"), &[], "").await;
    assert!(r.text().contains("color = 'red'"), "{}", r.text());
    assert_eq!(
        a.get(&format!("/subscriptions/{SUB}/resourceGroups/sb/providers/Microsoft.ServiceBus/namespaces")).await.json()["value"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // delete the queue through ARM
    assert_eq!(a.delete(&q).await.status, 200);
    let r = a.dp_send("GET", &format!("{sb}/orders?api-version=2021-05"), &[], "").await;
    assert!(!r.text().contains("<QueueDescription"));
    // delete the namespace
    assert_eq!(a.delete(&rid("sb", "Microsoft.ServiceBus", "namespaces/sbns01")).await.status, 200);
    let r = a.dp_send("GET", &format!("{sb}/events?api-version=2021-05"), &[], "").await;
    assert!(!r.text().contains("<TopicDescription"));
    // name check
    let r =
        a.post(&format!("/subscriptions/{SUB}/providers/Microsoft.ServiceBus/checkNameAvailability"), json!({"name": "ab"})).await.json();
    assert_eq!(r["nameAvailable"], false);
    assert_eq!(r["reason"], "InvalidName");
}

#[tokio::test]
async fn event_hubs_exist_in_the_data_plane() {
    let a = full().await;
    a.group("eh").await;
    let r = a.put(&rid("eh", "Microsoft.EventHub", "namespaces/ehns01"), json!({"location": "eastus", "sku": {"name": "Standard"}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["type"], "Microsoft.EventHub/Namespaces");
    let h = rid("eh", "Microsoft.EventHub", "namespaces/ehns01/eventhubs/hub1");
    let r = a.put(&h, json!({"properties": {"partitionCount": 2, "messageRetentionInDays": 2}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["properties"]["partitionIds"], json!(["0", "1"]));
    let r = a.dp_send("GET", &format!("http://localhost:{}/_azemu/eventhubs/ehns01/hubs/hub1", a.srv.port()), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["partitionCount"], 2);
    assert_eq!(r.json()["retentionSeconds"], 172800.0);
    let r = a.put(&format!("{h}/consumergroups/cg1"), json!({"properties": {"userMetadata": "x"}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = a.get(&format!("{h}/consumergroups")).await.json();
    assert_eq!(r["value"].as_array().unwrap().len(), 2);
    let r = a
        .dp_send("GET", &format!("http://localhost:{}/_azemu/eventhubs/ehns01/hubs/hub1/consumergroups", a.srv.port()), &[], "")
        .await
        .json();
    assert!(r["consumerGroups"].as_array().unwrap().iter().any(|c| c == "cg1"));
    // partition count cannot change
    let r = a.put(&h, json!({"properties": {"partitionCount": 4}})).await;
    assert_eq!(r.status, 400);
    let r = a
        .post(&rid("eh", "Microsoft.EventHub", "namespaces/ehns01/authorizationRules/RootManageSharedAccessKey/listKeys"), json!({}))
        .await
        .json();
    assert!(r["primaryConnectionString"].as_str().unwrap().contains("ehns01.eventhub.localhost"));
    assert_eq!(a.delete(&h).await.status, 200);
    let r = a.dp_send("GET", &format!("http://localhost:{}/_azemu/eventhubs/ehns01/hubs/hub1", a.srv.port()), &[], "").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn key_vault_and_app_configuration() {
    let a = full().await;
    a.group("kv").await;
    let r = a.put(&rid("kv", "Microsoft.KeyVault", "vaults/vault-one"), json!({"location": "eastus", "properties": {"sku": {"family": "A", "name": "standard"}, "tenantId": "t", "accessPolicies": []}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["properties"]["vaultUri"], format!("https://vault-one.vault.localhost:{}/", a.srv.port()));
    // a secret in the data plane, then delete the vault through ARM
    let url = format!("http://vault-one.vault.localhost:{}", a.srv.port());
    let r =
        a.dp_send("PUT", &format!("{url}/secrets/s1?api-version=7.5"), &[("Content-Type", "application/json")], r#"{"value":"v"}"#).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = a
        .put(
            &rid("kv", "Microsoft.KeyVault", "vaults/vault-one/accessPolicies/add"),
            json!({"properties": {"accessPolicies": [{"tenantId": "t", "objectId": "o1", "permissions": {"secrets": ["get"]}}]}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = a.get(&rid("kv", "Microsoft.KeyVault", "vaults/vault-one")).await.json();
    assert_eq!(r["properties"]["accessPolicies"][0]["objectId"], "o1");
    assert_eq!(a.delete(&rid("kv", "Microsoft.KeyVault", "vaults/vault-one")).await.status, 200);
    let r = a.dp_send("GET", &format!("{url}/secrets/s1?api-version=7.5"), &[], "").await;
    assert_eq!(r.status, 404);
    let r = a.put(&rid("kv", "Microsoft.KeyVault", "vaults/bad_name"), json!({"location": "eastus"})).await;
    assert_eq!(r.json()["error"]["code"], "VaultNameNotValid");

    // app configuration
    let r = a
        .put(
            &rid("kv", "Microsoft.AppConfiguration", "configurationStores/appcfg-one"),
            json!({"location": "eastus", "sku": {"name": "standard"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["endpoint"], format!("https://appcfg-one.azconfig.localhost:{}", a.srv.port()));
    let keys = a.post(&rid("kv", "Microsoft.AppConfiguration", "configurationStores/appcfg-one/listKeys"), json!({})).await.json();
    assert_eq!(keys["value"].as_array().unwrap().len(), 4);
    assert!(keys["value"][0]["connectionString"].as_str().unwrap().starts_with("Endpoint=https://appcfg-one.azconfig.localhost"));
    let id = keys["value"][0]["id"].as_str().unwrap().to_string();
    let k2 = a.post(&rid("kv", "Microsoft.AppConfiguration", "configurationStores/appcfg-one/regenerateKey"), json!({"id": id})).await;
    assert_eq!(k2.status, 200, "{}", k2.text());
    assert_ne!(k2.json()["value"], keys["value"][0]["value"]);
    let url = format!("http://appcfg-one.azconfig.localhost:{}", a.srv.port());
    let r =
        a.dp_send("PUT", &format!("{url}/kv/k1?api-version=2023-11-01"), &[("Content-Type", "application/json")], r#"{"value":"v"}"#).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(a.delete(&rid("kv", "Microsoft.AppConfiguration", "configurationStores/appcfg-one")).await.status, 200);
    let r = a.dp_send("GET", &format!("{url}/kv/k1?api-version=2023-11-01"), &[], "").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn cosmos_database_and_container() {
    let a = full().await;
    a.group("cs").await;
    let acct = rid("cs", "Microsoft.DocumentDB", "databaseAccounts/cosmos-one");
    let r = a.put(&acct, json!({"location": "eastus", "kind": "GlobalDocumentDB", "properties": {"databaseAccountOfferType": "Standard", "locations": [{"locationName": "East US", "failoverPriority": 0}]}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["properties"]["documentEndpoint"], format!("https://cosmos-one.documents.localhost:{}/", a.srv.port()));
    let r = a
        .put(&format!("{acct}/sqlDatabases/db1"), json!({"properties": {"resource": {"id": "db1"}, "options": {"throughput": 400}}}))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["properties"]["resource"]["id"], "db1");
    assert!(r.json()["properties"]["resource"]["_rid"].is_string());
    let r = a
        .put(
            &format!("{acct}/sqlDatabases/db1/containers/c1"),
            json!({"properties": {"resource": {"id": "c1", "partitionKey": {"paths": ["/pk"], "kind": "Hash"}}}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["properties"]["resource"]["partitionKey"]["paths"][0], "/pk");
    let t = a.get(&format!("{acct}/sqlDatabases/db1/throughputSettings/default")).await;
    assert_eq!(t.status, 200);
    assert_eq!(t.json()["properties"]["resource"]["throughput"], 400);
    // the data plane has them
    let url = format!("http://cosmos-one.documents.localhost:{}", a.srv.port());
    let h = [("x-ms-version", "2018-12-31"), ("Content-Type", "application/json")];
    let r = a.dp_send("GET", &format!("{url}/dbs/db1/colls/c1"), &h, "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = a
        .dp_send(
            "POST",
            &format!("{url}/dbs/db1/colls/c1/docs"),
            &[("x-ms-version", "2018-12-31"), ("x-ms-documentdb-partitionkey", "[\"a\"]"), ("Content-Type", "application/json")],
            r#"{"id":"d1","pk":"a"}"#,
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let keys = a.post(&format!("{acct}/listKeys"), json!({})).await.json();
    assert_eq!(keys["primaryMasterKey"].as_str().unwrap().len(), 88);
    let ro = a.get(&format!("{acct}/readonlykeys")).await;
    assert_eq!(ro.status, 200);
    assert!(ro.json()["primaryReadonlyMasterKey"].is_string());
    let cs = a.post(&format!("{acct}/listConnectionStrings"), json!({})).await.json();
    assert!(cs["connectionStrings"][0]["connectionString"]
        .as_str()
        .unwrap()
        .starts_with("AccountEndpoint=https://cosmos-one.documents.localhost"));
    assert_eq!(a.delete(&format!("{acct}/sqlDatabases/db1/containers/c1")).await.status, 204);
    let r = a.dp_send("GET", &format!("{url}/dbs/db1/colls/c1"), &h, "").await;
    assert_eq!(r.status, 404);
    assert_eq!(a.delete(&acct).await.status, 204);
    let r = a.dp_send("GET", &format!("{url}/dbs/db1"), &h, "").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn event_grid_topic() {
    let a = full().await;
    a.group("eg").await;
    let t = rid("eg", "Microsoft.EventGrid", "topics/topic-one");
    let r = a.put(&t, json!({"location": "eastus", "properties": {"inputSchema": "CloudEventSchemaV1_0"}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["endpoint"], format!("https://topic-one.eventgrid.localhost:{}/api/events", a.srv.port()));
    assert_eq!(r.json()["properties"]["inputSchema"], "CloudEventSchemaV1_0");
    let r = a.dp_send("GET", &format!("http://localhost:{}/_azemu/eventgrid/topics/topic-one", a.srv.port()), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let keys = a.post(&format!("{t}/listKeys"), json!({})).await.json();
    assert!(keys["key1"].is_string());
    let k2 = a.post(&format!("{t}/regenerateKey"), json!({"keyName": "key1"})).await.json();
    assert_ne!(k2["key1"], keys["key1"]);
    assert_eq!(k2["key2"], keys["key2"]);
    // an event subscription on the topic (the extension form) lives in the Event Grid service and goes with the topic
    let sub = format!("{t}/providers/Microsoft.EventGrid/eventSubscriptions/to-queue");
    let dest = json!({"endpointType": "StorageQueue", "properties": {"resourceId": rid("eg", "Microsoft.Storage", "storageAccounts/acct01"), "queueName": "q"}});
    let r = a.put(&sub, json!({"properties": {"destination": dest}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["id"], sub);
    assert_eq!(r.json()["properties"]["topic"], t);
    let r = a
        .dp_send("GET", &format!("http://localhost:{}/_azemu/eventgrid/topics/topic-one/subscriptions/to-queue", a.srv.port()), &[], "")
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    // a subscription needs a destination and a valid name
    assert_eq!(a.put(&format!("{t}/providers/Microsoft.EventGrid/eventSubscriptions/s1"), json!({"properties": {}})).await.status, 400);
    assert_eq!(
        a.put(&format!("{t}/providers/Microsoft.EventGrid/eventSubscriptions/no-dest"), json!({"properties": {}})).await.status,
        400
    );
    assert_eq!(a.delete(&t).await.status, 204);
    assert_eq!(a.get(&sub).await.status, 404);
    let r = a.dp_send("GET", &format!("http://localhost:{}/_azemu/eventgrid/topics/topic-one", a.srv.port()), &[], "").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn deleting_a_group_removes_every_provider_from_the_data_plane() {
    let a = full().await;
    a.group("all").await;
    let port = a.srv.port();
    a.put(&rid("all", "Microsoft.ServiceBus", "namespaces/sbgone01"), json!({"location": "eastus"})).await;
    a.put(&rid("all", "Microsoft.ServiceBus", "namespaces/sbgone01/queues/q1"), json!({"properties": {}})).await;
    a.put(&rid("all", "Microsoft.EventHub", "namespaces/ehgone01"), json!({"location": "eastus"})).await;
    a.put(&rid("all", "Microsoft.EventHub", "namespaces/ehgone01/eventhubs/h1"), json!({"properties": {}})).await;
    a.put(&rid("all", "Microsoft.KeyVault", "vaults/vgone"), json!({"location": "eastus", "properties": {}})).await;
    a.put(&rid("all", "Microsoft.AppConfiguration", "configurationStores/cfggone"), json!({"location": "eastus", "sku": {"name": "free"}}))
        .await;
    a.put(&rid("all", "Microsoft.DocumentDB", "databaseAccounts/cosgone"), json!({"location": "eastus", "properties": {}})).await;
    a.put(
        &rid("all", "Microsoft.DocumentDB", "databaseAccounts/cosgone/sqlDatabases/d1"),
        json!({"properties": {"resource": {"id": "d1"}}}),
    )
    .await;
    a.put(&rid("all", "Microsoft.EventGrid", "topics/topgone"), json!({"location": "eastus", "properties": {}})).await;
    let h = [("x-ms-version", "2018-12-31"), ("Content-Type", "application/json")];
    let kv = format!("http://vgone.vault.localhost:{port}/secrets/s?api-version=7.5");
    assert_eq!(a.dp_send("PUT", &kv, &[("Content-Type", "application/json")], r#"{"value":"v"}"#).await.status, 200);
    let cfg = format!("http://cfggone.azconfig.localhost:{port}/kv/k?api-version=2023-11-01");
    assert_eq!(a.dp_send("PUT", &cfg, &[("Content-Type", "application/json")], r#"{"value":"v"}"#).await.status, 200);
    let cos = format!("http://cosgone.documents.localhost:{port}/dbs/d1");
    assert_eq!(a.dp_send("GET", &cos, &h, "").await.status, 200);

    assert_eq!(a.delete(&format!("/subscriptions/{SUB}/resourceGroups/all")).await.status, 202);

    let sb = format!("http://sbgone01.servicebus.localhost:{port}/q1?api-version=2021-05");
    assert!(!a.dp_send("GET", &sb, &[], "").await.text().contains("<QueueDescription"));
    let eh = format!("http://localhost:{port}/_azemu/eventhubs/ehgone01/hubs/h1");
    assert_eq!(a.dp_send("GET", &eh, &[], "").await.status, 404);
    assert_eq!(a.dp_send("GET", &kv, &[], "").await.status, 404);
    assert_eq!(a.dp_send("GET", &cfg, &[], "").await.status, 404);
    assert_eq!(a.dp_send("GET", &cos, &h, "").await.status, 404);
    let eg = format!("http://localhost:{port}/_azemu/eventgrid/topics/topgone");
    assert_eq!(a.dp_send("GET", &eg, &[], "").await.status, 404);
}

#[tokio::test]
async fn a_bad_duration_in_a_service_bus_entity_is_a_400_that_names_it() {
    let a = full().await;
    a.group("sb").await;
    let ns = rid("sb", "Microsoft.ServiceBus", "namespaces/sbns02");
    assert_eq!(a.put(&ns, json!({"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}})).await.status, 200);
    let q = format!("{ns}/queues/orders");
    for (prop, value) in [
        ("lockDuration", json!("abc")),
        ("defaultMessageTimeToLive", json!(60)),
        ("duplicateDetectionHistoryTimeWindow", json!("1 hour")),
        ("autoDeleteOnIdle", json!("PTx")),
    ] {
        let r = a.put(&q, json!({"properties": {prop: value}})).await;
        assert_eq!(r.status, 400, "{prop}: {}", r.text());
        let e = &r.json()["error"];
        assert_eq!(e["code"], "InvalidRequestContent", "a guess; see the comment in providers/servicebus.rs");
        assert!(e["message"].as_str().unwrap().contains(prop), "{}", e["message"]);
    }
    // nothing was created, here or in the data plane
    assert_eq!(a.get(&q).await.status, 404);
    // a topic and a subscription go through the same check
    assert_eq!(a.put(&format!("{ns}/topics/t"), json!({"properties": {"defaultMessageTimeToLive": "nope"}})).await.status, 400);
    assert_eq!(a.put(&format!("{ns}/topics/t"), json!({"properties": {}})).await.status, 200);
    assert_eq!(a.put(&format!("{ns}/topics/t/subscriptions/s"), json!({"properties": {"lockDuration": "x"}})).await.status, 400);
    // good durations still work
    assert_eq!(a.put(&q, json!({"properties": {"lockDuration": "PT1M", "autoDeleteOnIdle": "P1D"}})).await.status, 200);
}

/// Recording `test_cli_mgmt_servicebus_queue.pyTestMgmtServiceBustest_queue.json`, entry 7: a queue made with `enablePartitioning: true` and no size
/// has `maxSizeInMegabytes` 16384; one without partitioning has 1024.
#[tokio::test]
async fn a_partitioned_queue_has_sixteen_times_the_default_size() {
    let a = full().await;
    a.group("sb").await;
    a.put(
        &rid("sb", "Microsoft.ServiceBus", "namespaces/sbns02"),
        json!({"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}}),
    )
    .await;
    let r = a
        .put(&rid("sb", "Microsoft.ServiceBus", "namespaces/sbns02/queues/part"), json!({"properties": {"enablePartitioning": true}}))
        .await;
    assert_eq!(r.json()["properties"]["maxSizeInMegabytes"], 16384, "{}", r.text());
    let r = a.put(&rid("sb", "Microsoft.ServiceBus", "namespaces/sbns02/queues/plain"), json!({"properties": {}})).await;
    assert_eq!(r.json()["properties"]["maxSizeInMegabytes"], 1024);
    let r = a
        .put(
            &rid("sb", "Microsoft.ServiceBus", "namespaces/sbns02/queues/sized"),
            json!({"properties": {"enablePartitioning": true, "maxSizeInMegabytes": 2048}}),
        )
        .await;
    assert_eq!(r.json()["properties"]["maxSizeInMegabytes"], 2048, "a size the client gave stays");
}

/// Recording `test_domain.pyTestMgmtEventGridtest_domain.json`, entry 1: the status of an Event Grid operation has the address of the status
/// as its `id` and the id in lower case as its `name`.
#[tokio::test]
async fn an_event_grid_operation_status_names_itself() {
    let a = full().await;
    let path = format!(
        "/subscriptions/{SUB}/providers/Microsoft.EventGrid/locations/eastus2/operationsStatus/EABF51A4-E274-44D0-AE40-5439DA4EED85"
    );
    let r = a.get(&format!("{path}?api-version=2023-12-15-preview")).await;
    assert_eq!(r.status, 200);
    let j = r.json();
    assert_eq!(j["id"], format!("https://management.azure.com{path}?api-version=2023-12-15-preview"));
    assert_eq!(j["name"], "eabf51a4-e274-44d0-ae40-5439da4eed85");
    assert_eq!(j["status"], "Succeeded");
}
