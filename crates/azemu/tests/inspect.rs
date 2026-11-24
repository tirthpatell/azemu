//! The inspector against the whole stack (`docs/inspector.md`): every service holds a little data, the tree is walked from
//! `/_azemu/inspect` through every path it names, and what must never be shown is not.

use azemu_amqp::client::TestClient;
use azemu_amqp::message::Message;
use azemu_core::inspect::walk::{walk, Visited};
use azemu_core::serde_json::{json, Value};
use azemu_core::testing::{TestResponse, TestServer};

const SUB: &str = "11111111-2222-3333-4444-555555555555";
const SECRET: &str = "s3cr3t-value-that-must-not-be-listed";

async fn http(srv: &TestServer, method: &str, url: &str, headers: &[(&str, &str)], body: &str) -> TestResponse {
    srv.send(method, url, headers, body.to_string()).await
}

async fn amqp(srv: &TestServer, host: &str) -> TestClient {
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], srv.port()).into();
    let mut c = TestClient::connect_sasl(addr, "ANONYMOUS", None).await;
    c.open_session(host).await;
    c
}

async fn arm(srv: &TestServer, path: &str, body: Value) {
    let url = format!("http://management.localhost:{}{path}?api-version=2022-10-01-preview", srv.port());
    let r = http(srv, "PUT", &url, &[("authorization", "Bearer arm"), ("content-type", "application/json")], &body.to_string()).await;
    assert!(r.status < 300, "PUT {path}: {} {}", r.status, r.text());
}

/// A little data in every service.
async fn fill(srv: &TestServer) {
    let ok = |r: TestResponse, what: &str| assert!(r.status < 300, "{what}: {} {}", r.status, r.text());
    // Blob
    let b = srv.url("blob", "acct");
    let shared = [("Authorization", "SharedKey acct:c2ln")];
    ok(http(srv, "PUT", &format!("{b}/docs?restype=container"), &shared, "").await, "container");
    for name in ["a.txt", "dir/b.txt", "dir/c.txt"] {
        ok(http(srv, "PUT", &format!("{b}/docs/{name}"), &[shared[0], ("x-ms-blob-type", "BlockBlob")], "blob text").await, name);
    }
    // Queue
    let q = srv.url("queue", "acct");
    ok(http(srv, "PUT", &format!("{q}/jobs"), &shared, "").await, "queue");
    for m in ["one", "two", "three"] {
        ok(
            http(
                srv,
                "POST",
                &format!("{q}/jobs/messages"),
                &shared,
                &format!("<QueueMessage><MessageText>{m}</MessageText></QueueMessage>"),
            )
            .await,
            "message",
        );
    }
    // Table
    let t = srv.url("table", "acct");
    let json_h = [("Authorization", "Bearer x"), ("Content-Type", "application/json")];
    ok(http(srv, "POST", &format!("{t}/Tables"), &json_h, r#"{"TableName":"People"}"#).await, "table");
    for i in 0..3 {
        ok(
            http(srv, "POST", &format!("{t}/People"), &json_h, &format!(r#"{{"PartitionKey":"p","RowKey":"{i}","Name":"n{i}"}}"#)).await,
            "entity",
        );
    }
    // Key Vault: a secret, a key
    let kv = srv.url("vault", "vault1");
    let kv_h = [("Authorization", "Bearer x"), ("Content-Type", "application/json")];
    ok(
        http(srv, "PUT", &format!("{kv}/secrets/db-password?api-version=7.6"), &kv_h, &json!({"value": SECRET}).to_string()).await,
        "secret",
    );
    ok(
        http(
            srv,
            "POST",
            &format!("{kv}/keys/signing/create?api-version=7.6"),
            &kv_h,
            &json!({"kty": "RSA", "key_size": 2048}).to_string(),
        )
        .await,
        "key",
    );
    // App Configuration
    let ac = srv.url("azconfig", "store1");
    ok(http(srv, "PUT", &format!("{ac}/kv/app%3Acolor?api-version=2024-09-01"), &kv_h, r#"{"value":"blue"}"#).await, "kv");
    // Cosmos DB
    let cosmos = srv.url("documents", "acct");
    let ch = [
        ("authorization", "type%3Dmaster%26ver%3D1.0%26sig%3Dxyz"),
        ("x-ms-date", "Wed, 11 Nov 2026 12:00:00 GMT"),
        ("x-ms-version", "2020-07-15"),
        ("content-type", "application/json"),
    ];
    ok(http(srv, "POST", &format!("{cosmos}/dbs"), &ch, r#"{"id":"shop"}"#).await, "db");
    ok(
        http(
            srv,
            "POST",
            &format!("{cosmos}/dbs/shop/colls"),
            &ch,
            r#"{"id":"orders","partitionKey":{"paths":["/c"],"kind":"Hash","version":2}}"#,
        )
        .await,
        "coll",
    );
    let mut with_pk: Vec<(&str, &str)> = ch.to_vec();
    with_pk.push(("x-ms-documentdb-partitionkey", r#"["ann"]"#));
    ok(http(srv, "POST", &format!("{cosmos}/dbs/shop/colls/orders/docs"), &with_pk, r#"{"id":"o1","c":"ann"}"#).await, "doc");
    // Event Grid: a topic with a subscription (to a closed port: the delivery waits for a retry)
    let g = srv.gateway_url();
    ok(http(srv, "PUT", &format!("{g}/_azemu/eventgrid/topics/orders"), &[("content-type", "application/json")], "{}").await, "topic");
    ok(
        http(
            srv,
            "PUT",
            &format!("{g}/_azemu/eventgrid/topics/orders/subscriptions/hook"),
            &[("content-type", "application/json")],
            r#"{"webhookUrl":"http://127.0.0.1:1/x","skipValidation":true}"#,
        )
        .await,
        "subscription",
    );
    // Monitor and File
    let track = json!([{"ver": 1, "name": "Microsoft.ApplicationInsights.Request", "time": "2026-11-24T10:00:00Z", "iKey": "11111111-2222-3333-4444-555555555555",
        "data": {"baseType": "RequestData", "baseData": {"ver": 2, "id": "a", "name": "GET /", "duration": "0.00:00:00.012", "responseCode": "200", "success": true}}}]);
    ok(
        http(srv, "POST", &format!("{}/v2/track", srv.url("in", "eastus")), &[("content-type", "application/json")], &track.to_string())
            .await,
        "track",
    );
    let f = srv.url("file.core", "acct");
    ok(http(srv, "PUT", &format!("{f}/share1?restype=share"), &shared, "").await, "share");
    // an identity token
    let form = "grant_type=client_credentials&client_id=app&client_secret=x&scope=https%3A%2F%2Fstorage.azure.com%2F.default";
    ok(
        http(
            srv,
            "POST",
            &format!("https://login.localhost:{}/contoso/oauth2/v2.0/token", srv.port()),
            &[("content-type", "application/x-www-form-urlencoded")],
            form,
        )
        .await,
        "token",
    );
    // Resource Manager, Service Bus and Event Hubs
    let group = format!("/subscriptions/{SUB}/resourceGroups/rg");
    arm(srv, &group, json!({"location": "eastus"})).await;
    arm(
        srv,
        &format!("{group}/providers/Microsoft.Storage/storageAccounts/armstore"),
        json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2", "properties": {}}),
    )
    .await;
    arm(srv, &format!("{group}/providers/Microsoft.Web/sites/func"), json!({"location": "eastus", "properties": {}})).await;
    let sb = format!("{group}/providers/Microsoft.ServiceBus/namespaces/sbns01");
    arm(srv, &sb, json!({"location": "eastus", "sku": {"name": "Standard"}, "properties": {}})).await;
    arm(srv, &format!("{sb}/queues/q"), json!({"properties": {}})).await;
    arm(srv, &format!("{sb}/topics/t"), json!({"properties": {}})).await;
    arm(srv, &format!("{sb}/topics/t/subscriptions/s"), json!({"properties": {}})).await;
    let eh = format!("{group}/providers/Microsoft.EventHub/namespaces/ehns01");
    arm(srv, &eh, json!({"location": "eastus", "sku": {"name": "Standard"}, "properties": {}})).await;
    arm(srv, &format!("{eh}/eventhubs/hub"), json!({"properties": {"partitionCount": 2}})).await;
    let mut c = amqp(srv, "sbns01.servicebus.localhost").await;
    let h = c.attach_sender("q").await.expect("sender on q");
    for i in 0..3 {
        assert!(c.send_message_wait(h, &Message::data(format!("msg {i}").into_bytes())).await.is_some());
    }
    let mut c = amqp(srv, "ehns01.eventhub.localhost").await;
    let h = c.attach_sender("hub").await.expect("sender on hub");
    for i in 0..3 {
        assert!(c.send_message_wait(h, &Message::data(format!("event {i}").into_bytes())).await.is_some());
    }
}

fn kinds(nodes: &[Visited]) -> Vec<String> {
    nodes.iter().map(|n| n.node["kind"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn the_whole_tree_of_a_filled_emulator_can_be_walked_and_nothing_secret_is_in_it() {
    let azemu::Stack { services, raw } = azemu::full_stack();
    let srv = TestServer::start_with(services, raw).await;
    fill(&srv).await;
    for limit in [2, 50] {
        let nodes = walk(&srv, "", limit).await;
        let kinds = kinds(&nodes);
        // every service that has an inspector has something in the walk
        let root = &nodes[0].node;
        let services: Vec<&str> = root["children"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        for s in [
            "blob",
            "queue",
            "table",
            "keyvault",
            "appconfig",
            "cosmos",
            "eventgrid",
            "servicebus",
            "eventhubs",
            "arm",
            "identity",
            "monitor",
            "file",
            "auth",
        ] {
            assert!(services.contains(&s), "{s} has no inspector: {services:?}");
            assert!(nodes.iter().any(|n| n.requested.starts_with(&format!("/{s}"))), "the walk did not reach {s}");
        }
        for k in [
            "container",
            "blob",
            "queue",
            "messages",
            "table",
            "entities",
            "vault",
            "secret",
            "store",
            "keyvalues",
            "container",
            "documents",
            "host",
            "subscription",
            "namespace",
            "hub",
            "partition",
            "resource",
            "registry",
            "signer",
            "sink",
            "share",
            "emulator",
            "health",
            "config",
        ] {
            assert!(kinds.iter().any(|x| x == k), "no node of kind {k} in {kinds:?}");
        }
        // no secret value without ?reveal=1
        let text = nodes.iter().map(|n| n.node.to_string()).collect::<Vec<_>>().join("\n");
        assert!(!text.contains(SECRET), "a secret value is in the walk");
    }
    // the same data through its own path: the messages that were sent are there
    let g = srv.gateway_url();
    let sbq = http(&srv, "GET", &format!("{g}/_azemu/inspect/servicebus/sbns01/queues/q/active"), &[], "").await.json();
    assert_eq!(sbq["items"].as_array().unwrap().len(), 3);
    let eh = http(&srv, "GET", &format!("{g}/_azemu/inspect/eventhubs/ehns01/hubs/hub"), &[], "").await.json();
    assert_eq!(eh["properties"]["partitionCount"], 2);
    // a resource nothing is emulated for says so
    let res = http(&srv, "GET", &format!("{g}/_azemu/inspect/arm/{SUB}/resourcegroups/rg/resources?type=web"), &[], "").await.json();
    assert_eq!(res["items"][0]["storedOnly"], true);
}

#[tokio::test]
async fn a_secret_is_shown_for_one_secret_with_reveal_and_never_to_another_site() {
    let azemu::Stack { services, raw } = azemu::full_stack();
    let srv = TestServer::start_with(services, raw).await;
    fill(&srv).await;
    let g = srv.gateway_url();
    let url = format!("{g}/_azemu/inspect/keyvault/vault1/secrets/db-password?reveal=1");
    let r = http(&srv, "GET", &url, &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["properties"]["value"], SECRET);
    // no value without reveal, also in the list
    let plain = http(&srv, "GET", &format!("{g}/_azemu/inspect/keyvault/vault1/secrets/db-password"), &[], "").await;
    assert!(!plain.text().contains(SECRET));
    assert!(!http(&srv, "GET", &format!("{g}/_azemu/inspect/keyvault/vault1/secrets"), &[], "").await.text().contains(SECRET));
    // another web site is refused, whatever it says about itself
    for h in [("sec-fetch-site", "cross-site"), ("origin", "http://evil.example")] {
        let r = http(&srv, "GET", &url, &[h], "").await;
        assert_eq!(r.status, 403, "{h:?}");
        assert!(!r.text().contains(SECRET));
    }
    // the page is served, with its policy
    let page = http(&srv, "GET", &format!("{g}/_azemu/ui"), &[], "").await;
    assert_eq!(page.status, 200);
    assert!(page.header("content-security-policy").unwrap().starts_with("default-src 'none'"));
}

#[tokio::test]
async fn looking_at_a_path_that_names_a_missing_account_makes_none_in_any_service() {
    let azemu::Stack { services, raw } = azemu::full_stack();
    let srv = TestServer::start_with(services, raw).await;
    let g = srv.gateway_url();
    for s in ["blob", "queue", "table", "keyvault", "appconfig", "cosmos", "eventgrid", "servicebus", "eventhubs", "file"] {
        let r = http(&srv, "GET", &format!("{g}/_azemu/inspect/{s}/nobody-home"), &[], "").await;
        assert_eq!(r.status, 404, "{s}: {}", r.text());
        let svc = http(&srv, "GET", &format!("{g}/_azemu/inspect/{s}"), &[], "").await.json();
        assert_eq!(svc["properties"]["accounts"], 0, "{s} made an account because it was looked at");
    }
}
