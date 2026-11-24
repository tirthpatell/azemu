//! Template functions that need the whole deployment: `list*` (they run the provider action), `utcNow` in a default,
//! `environment()`, lambdas in a copy count, user-defined functions and `languageVersion` 2.0.
//!
//! The `.json` files in `bicep/` are what the Bicep compiler (`bicep build`) wrote for the `.bicep` file of the same name;
//! they are deployed here as they are.

mod common;

use common::{full, standalone, Arm, SUB};
use serde_json::{json, Value};

fn dep(rg: &str, name: &str) -> String {
    format!("/subscriptions/{SUB}/resourcegroups/{rg}/providers/Microsoft.Resources/deployments/{name}")
}

fn rid(rg: &str, ns: &str, rest: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/{rg}/providers/{ns}/{rest}")
}

fn bicep(name: &str) -> Value {
    let path = format!("{}/tests/bicep/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))).unwrap()
}

/// Deploy; returns the whole answer.
async fn deploy(a: &Arm, rg: &str, name: &str, template: Value, parameters: Value) -> azemu_core::testing::TestResponse {
    a.put(&dep(rg, name), json!({"properties": {"mode": "Incremental", "template": template, "parameters": parameters}})).await
}

/// Deploy and expect success; returns `properties.outputs` as `{name: value}`.
async fn outputs(a: &Arm, rg: &str, name: &str, template: Value, parameters: Value) -> serde_json::Map<String, Value> {
    let r = deploy(a, rg, name, template, parameters).await;
    assert!(r.status == 200 || r.status == 201, "{} {}", r.status, r.text());
    let j = r.json();
    assert_eq!(j["properties"]["provisioningState"], "Succeeded", "{}", j);
    j["properties"]["outputs"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v["value"].clone())).collect()
}

#[tokio::test]
async fn storage_keys_and_the_endpoint_suffix_from_bicep() {
    // storage_keys.bicep: `sa.listKeys().keys[0].value` and `environment().suffixes.storage` in an output
    let a = full().await;
    a.group("rg-keys").await;
    let o = outputs(&a, "rg-keys", "d", bicep("storage_keys"), json!({"name": {"value": "stgkeys01"}})).await;
    let direct = a.post(&rid("rg-keys", "Microsoft.Storage", "storageAccounts/stgkeys01/listKeys"), json!({})).await.json();
    let key1 = direct["keys"][0]["value"].as_str().unwrap();
    assert_eq!(o["key1Length"], json!(key1.len()));
    let port = a.srv.port();
    assert_eq!(
        o["connectionString"],
        json!(format!("DefaultEndpointsProtocol=https;AccountName=stgkeys01;AccountKey={key1};EndpointSuffix=core.localhost:{port}"))
    );
    // a host built from the suffix is the account's Blob host
    assert_eq!(o["blobEndpoint"], json!(format!("http://stgkeys01.blob.core.localhost:{port}/")));
}

#[tokio::test]
async fn lambdas_in_a_copy_count_and_in_outputs_from_bicep() {
    // servicebus_loop.bicep: map and filter make the list of queue names; sort is an output
    let a = full().await;
    a.group("rg-sb").await;
    let o = outputs(&a, "rg-sb", "d", bicep("servicebus_loop"), json!({"namespaceName": {"value": "sbloop01"}})).await;
    assert_eq!(o["created"], json!(["sbloop01/orders", "sbloop01/billing", "sbloop01/audit"]));
    assert_eq!(o["sorted"], json!(["audit", "billing", "orders"]));
    assert_eq!(o["endpoint"], json!(format!("https://sbloop01.servicebus.localhost:{}/", a.srv.port())));
    let list = a.get(&rid("rg-sb", "Microsoft.ServiceBus", "namespaces/sbloop01/queues")).await.json();
    let mut names: Vec<&str> = list["value"].as_array().unwrap().iter().map(|q| q["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["billing", "orders"]);
}

#[tokio::test]
async fn date_arithmetic_in_a_tag_from_bicep() {
    // keyvault_expiry.bicep: `utcNow('u')` is only the default of a parameter; dateTimeAdd makes the tag
    let a = full().await;
    a.group("rg-kv").await;
    let o = outputs(
        &a,
        "rg-kv",
        "d",
        bicep("keyvault_expiry"),
        json!({"vaultName": {"value": "kvdates01"}, "baseTime": {"value": "2024-03-01T00:00:00Z"}}),
    )
    .await;
    assert_eq!(o["reviewBy"], json!("2025-03-01"));
    assert_eq!(o["reviewEpoch"], json!(1_740_787_200));
    assert_eq!(o["fixedExpiry"], json!("2024-02-14T10:00:00Z"));
    let vault = a.get(&rid("rg-kv", "Microsoft.KeyVault", "vaults/kvdates01")).await.json();
    assert_eq!(vault["tags"]["reviewBy"], "2025-03-01");
    // with no value for baseTime, the default is utcNow('u') of the request
    let o = outputs(&a, "rg-kv", "d2", bicep("keyvault_expiry"), json!({"vaultName": {"value": "kvdates02"}})).await;
    let by = o["reviewBy"].as_str().unwrap();
    assert_eq!(by.len(), 10, "{by}");
    assert!(by.starts_with("20"), "{by}");
}

#[tokio::test]
async fn user_defined_function_from_bicep() {
    // user_function.bicep: `func queueName(...)` is a function of the namespace `__bicep`; the template is languageVersion 2.0
    let a = full().await;
    a.group("rg-fn").await;
    let o = outputs(&a, "rg-fn", "d", bicep("user_function"), json!({"namespaceName": {"value": "sbfn01"}})).await;
    assert_eq!(o["names"], json!(["orders-q001", "orders-q002"]));
    let list = a.get(&rid("rg-fn", "Microsoft.ServiceBus", "namespaces/sbfn01/queues")).await.json();
    let mut names: Vec<&str> = list["value"].as_array().unwrap().iter().map(|q| q["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["orders-q001", "orders-q002"]);
}

#[tokio::test]
async fn symbolic_names_types_and_existing_resources_from_bicep() {
    // v2_probe.bicep: user-defined type with a nullable member, `existing`, symbolic dependsOn, `listKeys('sa', ...)`,
    // `reference('bus')`, an output made with `copy`, and an output that has a `$ref` type
    let a = full().await;
    a.group("rg-v2").await;
    let o = outputs(&a, "rg-v2", "d", bicep("v2_probe"), json!({"ns": {"value": "probe01"}})).await;
    assert_eq!(o["saKeys"], json!("key1"));
    assert_eq!(o["endpoint"], json!(format!("https://probe01.servicebus.localhost:{}/", a.srv.port())));
    assert_eq!(o["exname"], json!("probe01"));
    assert_eq!(
        o["qids"],
        json!([
            rid("rg-v2", "Microsoft.ServiceBus", "namespaces/probe01/queues/a"),
            rid("rg-v2", "Microsoft.ServiceBus", "namespaces/probe01/queues/b")
        ])
    );
    assert_eq!(o["st"], json!([{"name": "a", "maxDelivery": 3}, {"name": "b"}]));
    // `s.?maxDelivery ?? 10`: a has 3, b falls back to 10
    let qa = a.get(&rid("rg-v2", "Microsoft.ServiceBus", "namespaces/probe01/queues/a")).await.json();
    let qb = a.get(&rid("rg-v2", "Microsoft.ServiceBus", "namespaces/probe01/queues/b")).await.json();
    assert_eq!(qa["properties"]["maxDeliveryCount"], 3);
    assert_eq!(qb["properties"]["maxDeliveryCount"], 10);
    // a value that does not fit the type is refused with the place named
    let r = deploy(
        &a,
        "rg-v2",
        "bad",
        bicep("v2_probe"),
        json!({"ns": {"value": "probe02"}, "settings": {"value": [{"name": "a", "maxDelivery": 0}]}}),
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("[0].maxDelivery"), "{}", r.text());
    // and the SKU list of allowedValues
    let r = deploy(&a, "rg-v2", "bad2", bicep("v2_probe"), json!({"skuName": {"value": "Basic"}})).await;
    assert_eq!(r.status, 400, "{}", r.text());
}

fn list_template(extra_outputs: Value) -> Value {
    json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [
            // listed before the resources it uses, with no dependsOn: a list call waits for the resource it names
            {"type": "Microsoft.Storage/storageAccounts", "apiVersion": "2023-01-01", "name": "stglater01", "location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2",
             "tags": {"bus": "[listKeys(resourceId('Microsoft.ServiceBus/namespaces/authorizationRules', 'sblist01', 'RootManageSharedAccessKey'), '2022-10-01-preview').primaryKey]"}},
            {"type": "Microsoft.ServiceBus/namespaces", "apiVersion": "2022-10-01-preview", "name": "sblist01", "location": "eastus", "sku": {"name": "Standard"}},
            {"type": "Microsoft.EventHub/namespaces", "apiVersion": "2022-10-01-preview", "name": "ehlist01", "location": "eastus", "sku": {"name": "Standard"}},
            {"type": "Microsoft.AppConfiguration/configurationStores", "apiVersion": "2023-03-01", "name": "cfglist01", "location": "eastus", "sku": {"name": "free"}},
            {"type": "Microsoft.DocumentDB/databaseAccounts", "apiVersion": "2023-04-15", "name": "cosmoslist01", "location": "eastus", "properties": {}},
            {"type": "Microsoft.EventGrid/topics", "apiVersion": "2022-06-15", "name": "toplist01", "location": "eastus"}
        ],
        "outputs": extra_outputs
    })
}

#[tokio::test]
async fn a_list_call_waits_for_its_resource_and_equals_the_direct_post() {
    let a = full().await;
    a.group("rg-list").await;
    let outs = json!({
        "storage": {"type": "object", "value": "[listKeys(resourceId('Microsoft.Storage/storageAccounts', 'stglater01'), '2023-01-01')]"},
        "storageByName": {"type": "object", "value": "[listKeys('stglater01', '2023-01-01')]"},
        "bus": {"type": "object", "value": "[listKeys(resourceId('Microsoft.ServiceBus/namespaces/authorizationRules', 'sblist01', 'RootManageSharedAccessKey'), '2022-10-01-preview')]"},
        "hub": {"type": "object", "value": "[listKeys(resourceId('Microsoft.EventHub/namespaces/authorizationRules', 'ehlist01', 'RootManageSharedAccessKey'), '2022-10-01-preview')]"},
        "config": {"type": "object", "value": "[listKeys(resourceId('Microsoft.AppConfiguration/configurationStores', 'cfglist01'), '2023-03-01')]"},
        "cosmosKeys": {"type": "object", "value": "[listKeys(resourceId('Microsoft.DocumentDB/databaseAccounts', 'cosmoslist01'), '2023-04-15')]"},
        "cosmosConnections": {"type": "object", "value": "[listConnectionStrings(resourceId('Microsoft.DocumentDB/databaseAccounts', 'cosmoslist01'), '2023-04-15')]"},
        "topic": {"type": "object", "value": "[listKeys(resourceId('Microsoft.EventGrid/topics', 'toplist01'), '2022-06-15')]"},
        "topicShared": {"type": "object", "value": "[listSharedAccessKeys(resourceId('Microsoft.EventGrid/topics', 'toplist01'), '2022-06-15')]"}
    });
    let o = outputs(&a, "rg-list", "d", list_template(outs), json!({})).await;
    async fn post(a: &Arm, (ns, rest): (&str, &str)) -> Value {
        a.post(&rid("rg-list", ns, rest), json!({})).await.json()
    }
    // every answer is the answer of the POST action of the same name
    assert_eq!(o["storage"], post(&a, ("Microsoft.Storage", "storageAccounts/stglater01/listKeys")).await);
    assert_eq!(o["storage"], o["storageByName"]);
    assert_eq!(
        o["bus"],
        post(&a, ("Microsoft.ServiceBus", "namespaces/sblist01/authorizationRules/RootManageSharedAccessKey/listKeys")).await
    );
    assert_eq!(
        o["hub"],
        post(&a, ("Microsoft.EventHub", "namespaces/ehlist01/authorizationRules/RootManageSharedAccessKey/listKeys")).await
    );
    assert_eq!(o["config"], post(&a, ("Microsoft.AppConfiguration", "configurationStores/cfglist01/listKeys")).await);
    assert_eq!(o["cosmosKeys"], post(&a, ("Microsoft.DocumentDB", "databaseAccounts/cosmoslist01/listKeys")).await);
    assert_eq!(o["cosmosConnections"], post(&a, ("Microsoft.DocumentDB", "databaseAccounts/cosmoslist01/listConnectionStrings")).await);
    assert_eq!(o["topic"], post(&a, ("Microsoft.EventGrid", "topics/toplist01/listKeys")).await);
    assert_eq!(o["topicShared"], o["topic"]);
    assert_eq!(o["storage"]["keys"][0]["keyName"], "key1");
    // the tag was made from a key of a namespace that was deployed after the storage account in the template
    let tag = a.get(&rid("rg-list", "Microsoft.Storage", "storageAccounts/stglater01")).await.json()["tags"]["bus"].clone();
    assert_eq!(tag, o["bus"]["primaryKey"]);
}

#[tokio::test]
async fn list_account_sas_and_list_service_sas_make_tokens_the_data_plane_accepts() {
    let a = full().await;
    a.group("rg-sas").await;
    let tpl = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "parameters": {"expiry": {"type": "string", "defaultValue": "[dateTimeAdd(utcNow(), 'P1D')]"}},
        "resources": [
            {"type": "Microsoft.Storage/storageAccounts", "apiVersion": "2023-01-01", "name": "stgsas01", "location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"}
        ],
        "outputs": {
            "account": {"type": "string", "value": "[listAccountSas('stgsas01', '2023-01-01', createObject('signedServices', 'b', 'signedResourceTypes', 'sco', 'signedPermission', 'rwdlc', 'signedExpiry', parameters('expiry'))).accountSasToken]"},
            "service": {"type": "string", "value": "[listServiceSas('stgsas01', '2023-01-01', createObject('canonicalizedResource', '/blob/stgsas01/sasc', 'signedResource', 'c', 'signedPermission', 'rl', 'signedExpiry', parameters('expiry'))).serviceSasToken]"}
        }
    });
    let o = outputs(&a, "rg-sas", "d", tpl, json!({"expiry": {"value": "2099-01-01T00:00:00Z"}})).await;
    let (account, service) = (o["account"].as_str().unwrap(), o["service"].as_str().unwrap());
    let host = a.dp("blob", "stgsas01");
    // the account token creates a container; the container token lists it
    let r = a.srv.send("PUT", &format!("{host}/sasc?restype=container&{account}"), &[], String::new()).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = a.srv.send("GET", &format!("{host}/sasc?restype=container&comp=list&{service}"), &[], String::new()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // the data plane does not compare the signature of an account other than the development one (its keys are the
    // emulator's own), but it reads what the token says: a token with no read right does not list
    let r = a
        .srv
        .send("GET", &format!("{host}/sasc?restype=container&comp=list&{}", service.replace("sp=rl", "sp=w")), &[], String::new())
        .await;
    assert_eq!(r.status, 403, "{}", r.text());
    // the service token has only r and l: it does not allow a write
    let r = a.srv.send("PUT", &format!("{host}/sasc/x.txt?{service}"), &[("x-ms-blob-type", "BlockBlob")], "hello".to_string()).await;
    assert_eq!(r.status, 403, "{}", r.text());
    // the direct POST action answers the same shape
    let r = a
        .post(
            &rid("rg-sas", "Microsoft.Storage", "storageAccounts/stgsas01/listAccountSas"),
            json!({"signedServices": "b", "signedResourceTypes": "sco", "signedPermission": "rl", "signedExpiry": "2099-01-01T00:00:00Z"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.json()["accountSasToken"].as_str().unwrap().contains("&sig="));
}

#[tokio::test]
async fn a_list_call_that_cannot_work_is_an_error_that_names_it() {
    let a = full().await;
    a.group("rg-lerr").await;
    let mk = |value: &str| {
        json!({
            "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
            "contentVersion": "1.0.0.0",
            "resources": [
                {"type": "Microsoft.Network/virtualNetworks", "apiVersion": "2023-05-01", "name": "vnet1", "location": "eastus", "properties": {}},
                {"type": "Microsoft.ServiceBus/namespaces", "apiVersion": "2022-10-01-preview", "name": "sblerr01", "location": "eastus", "sku": {"name": "Standard"}}
            ],
            "outputs": {"x": {"type": "object", "value": value}}
        })
    };
    // A list call that cannot work starts the deployment and fails it with what the POST answered (a real deployment does the
    // same for `listKeys` on a resource group and on an account that does not exist).
    let failed = |r: &azemu_core::testing::TestResponse| {
        assert!(r.status == 200 || r.status == 201, "{}", r.text());
        let j = r.json();
        assert_eq!(j["properties"]["provisioningState"], "Failed", "{j}");
        assert_eq!(j["properties"]["error"]["code"], "DeploymentFailed", "{j}");
        j["properties"]["error"]["details"][0].clone()
    };
    // a type with no such action: the service says there is no HTTP resource at that address
    let r =
        deploy(&a, "rg-lerr", "d1", mk("[listKeys(resourceId('Microsoft.Network/virtualNetworks', 'vnet1'), '2023-05-01')]"), json!({}))
            .await;
    let d = failed(&r);
    assert_eq!(d["code"], "NotFound");
    assert!(d["message"].as_str().unwrap().contains("No HTTP resource was found that matches the request URI"), "{d}");
    assert!(d["message"].as_str().unwrap().contains("virtualNetworks/vnet1/listKeys?api-version=2023-05-01"), "{d}");
    assert!(d["target"].as_str().unwrap().ends_with("/virtualNetworks/vnet1"), "{d}");
    // the same on a resource group (the probe of a real deployment: `listKeys(resourceGroup().id, ...)`)
    let r = deploy(&a, "rg-lerr", "d1b", mk("[listKeys(resourceGroup().id, '2021-04-01')]"), json!({})).await;
    let d = failed(&r);
    assert_eq!(d["code"], "NotFound");
    assert!(
        d["message"].as_str().unwrap().contains("/providers/Microsoft.Resources/resourceGroups/rg-lerr/listKeys?api-version=2021-04-01"),
        "{d}"
    );
    // a type with actions, but not this one
    let r = deploy(
        &a,
        "rg-lerr",
        "d2",
        mk("[listSecrets(resourceId('Microsoft.ServiceBus/namespaces', 'sblerr01'), '2022-10-01-preview')]"),
        json!({}),
    )
    .await;
    assert_eq!(failed(&r)["code"], "NotFound");
    // a resource that is not in the template and does not exist: ResourceNotFound, in the words of the service
    let r = deploy(
        &a,
        "rg-lerr",
        "d3",
        mk("[listKeys(resourceId('Microsoft.Storage/storageAccounts', 'nosuchaccount'), '2023-01-01')]"),
        json!({}),
    )
    .await;
    let d = failed(&r);
    assert_eq!(d["code"], "ResourceNotFound");
    assert_eq!(
        d["message"],
        "The Resource 'Microsoft.Storage/storageAccounts/nosuchaccount' under resource group 'rg-lerr' was not found. For more details please go to https://aka.ms/ARMResourceNotFoundFix"
    );
    // fewer arguments
    let r = deploy(&a, "rg-lerr", "d4", mk("[listKeys('vnet1')]"), json!({})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("at least 2"), "{}", r.text());
}

#[tokio::test]
async fn utc_now_and_new_guid_only_in_a_default() {
    let a = standalone().await;
    a.group("rg-now").await;
    let t = |param_default: &str, output: &str| {
        json!({
            "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
            "contentVersion": "1.0.0.0",
            "parameters": {"p": {"type": "string", "defaultValue": param_default}},
            "resources": [],
            "outputs": {"o": {"type": "string", "value": output}}
        })
    };
    let o = outputs(&a, "rg-now", "ok", t("[utcNow('yyyy-MM-dd')]", "[parameters('p')]"), json!({})).await;
    assert_eq!(o["o"].as_str().unwrap().len(), 10);
    let o = outputs(&a, "rg-now", "ok2", t("[newGuid()]", "[parameters('p')]"), json!({})).await;
    assert_eq!(o["o"].as_str().unwrap().len(), 36);
    // in an output the deployment starts and fails while it evaluates the outputs (Azure's words, spelling included)
    for (default, out, named) in
        [("x", "[utcNow()]", "utcNow"), ("x", "[newGuid()]", "newGuid"), ("[concat('a', 'b')]", "[concat(utcNow('d'), 'x')]", "utcNow")]
    {
        let r = deploy(&a, "rg-now", "bad", t(default, out), json!({})).await;
        assert!(r.status == 200 || r.status == 201, "{}", r.text());
        let e = r.json()["properties"]["error"].clone();
        assert_eq!(e["code"], "DeploymentOutputEvaluationFailed", "{e}");
        assert_eq!(e["details"][0]["target"], "o");
        let m = e["details"][0]["message"].as_str().unwrap();
        assert!(
            m.starts_with(&format!("The template output 'o' is not valid: The template function '{named}' is not expected at this location. The function can only be used with parameter default value expresions."))
                && m.ends_with("usage details.."),
            "{m}"
        );
    }
    // in a variable the template is refused when it is validated, before anything is deployed
    for (var, named) in
        [("[newGuid()]", "newGuid"), ("[concat('a', utcNow('d'))]", "utcNow"), ("[listKeys('x', '2023-01-01')]", "listKeys")]
    {
        let tpl = json!({"contentVersion": "1.0.0.0", "resources": [], "variables": {"g": var}, "outputs": {"o": {"type": "string", "value": "a"}}});
        let r = deploy(&a, "rg-now", "bad-var", tpl, json!({})).await;
        assert_eq!(r.status, 400, "{}", r.text());
        assert_eq!(r.json()["error"]["code"], "InvalidTemplate");
        assert!(
            r.text().contains(&format!(
                "The template variable 'g' is not valid: The template function '{named}' is not expected at this location."
            )),
            "{}",
            r.text()
        );
    }
    // in a resource property it fails the resource
    let tpl = json!({"contentVersion": "1.0.0.0", "resources": [{"type": "Microsoft.Storage/storageAccounts", "apiVersion": "2023-01-01", "name": "stgnow01", "location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2", "tags": {"t": "[utcNow()]"}}]});
    let r = deploy(&a, "rg-now", "bad2", tpl, json!({})).await;
    assert!(r.text().contains("utcNow") && r.text().contains("Failed"), "{}", r.text());
}

#[tokio::test]
async fn environment_points_at_the_emulator() {
    let a = standalone().await;
    a.group("rg-env").await;
    let tpl = json!({
        "contentVersion": "1.0.0.0", "resources": [],
        "outputs": {
            "all": {"type": "object", "value": "[environment()]"},
            "blob": {"type": "string", "value": "[format('{0}.blob.{1}', 'acct', environment().suffixes.storage)]"}
        }
    });
    let o = outputs(&a, "rg-env", "d", tpl, json!({})).await;
    let port = a.srv.port();
    assert_eq!(o["blob"], json!(format!("acct.blob.core.localhost:{port}")));
    assert_eq!(o["all"]["resourceManager"], json!(format!("https://management.localhost:{port}/")));
    assert_eq!(o["all"]["suffixes"]["keyvaultDns"], json!(format!(".vault.localhost:{port}")));
    assert_eq!(o["all"]["name"], "AzureCloud");
}

#[tokio::test]
async fn lambda_work_is_limited_per_request() {
    let a = standalone().await;
    a.group("rg-work").await;
    let tpl = json!({
        "contentVersion": "1.0.0.0", "resources": [],
        "outputs": {"o": {"type": "int", "value": "[length(map(range(0, 10000), lambda('i', length(map(range(0, 10000), lambda('j', add(lambdaVariables('i'), lambdaVariables('j'))))))))]"}}
    });
    let r = deploy(&a, "rg-work", "d", tpl, json!({})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("too complex"), "{}", r.text());
}

#[tokio::test]
async fn user_functions_and_their_limits() {
    let a = standalone().await;
    a.group("rg-uf").await;
    let tpl = |body: &str, call: &str| {
        json!({
            "contentVersion": "1.0.0.0",
            "functions": [{"namespace": "contoso", "members": {
                "greet": {"parameters": [{"name": "who", "type": "string"}, {"name": "n", "type": "int"}], "output": {"type": "string", "value": body}},
                "other": {"parameters": [], "output": {"type": "string", "value": "x"}}
            }}],
            "variables": {"v": "from a variable"},
            "parameters": {"top": {"type": "string", "defaultValue": "from a parameter"}},
            "resources": [],
            "outputs": {"o": {"type": "string", "value": call}}
        })
    };
    let o = outputs(
        &a,
        "rg-uf",
        "ok",
        tpl("[concat('Hello ', parameters('who'), string(parameters('n')))]", "[contoso.greet('Bob', 3)]"),
        json!({}),
    )
    .await;
    assert_eq!(o["o"], "Hello Bob3");
    for (body, call, what) in [
        ("[variables('v')]", "[contoso.greet('a', 1)]", "variables"),
        ("[parameters('top')]", "[contoso.greet('a', 1)]", "top"),
        ("[contoso.other()]", "[contoso.greet('a', 1)]", "another one"),
        ("[concat(parameters('who'))]", "[contoso.greet('a')]", "takes 2"),
        ("[concat(parameters('who'))]", "[contoso.greet('a', 'x')]", "not valid"),
        ("[concat(parameters('who'))]", "[contoso.missing('a')]", "not defined"),
    ] {
        let r = deploy(&a, "rg-uf", "bad", tpl(body, call), json!({})).await;
        assert_eq!(r.status, 400, "{what}: {}", r.text());
        assert!(r.text().contains(what), "{what}: {}", r.text());
    }
}

#[tokio::test]
async fn symbolic_names_loops_references_and_existing() {
    let a = full().await;
    a.group("rg-sym").await;
    let tpl = |existing_name: &str| {
        json!({
            "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
            "languageVersion": "2.0",
            "contentVersion": "1.0.0.0",
            "resources": {
                "bus": {"type": "Microsoft.ServiceBus/namespaces", "apiVersion": "2022-10-01-preview", "name": "sbsym01", "location": "eastus", "sku": {"name": "Standard"}},
                "queues": {
                    "copy": {"name": "queues", "count": 3},
                    "condition": "[not(equals(copyIndex(), 1))]",
                    "type": "Microsoft.ServiceBus/namespaces/queues", "apiVersion": "2022-10-01-preview",
                    "name": "[format('sbsym01/q{0}', copyIndex())]",
                    "dependsOn": ["bus"]
                },
                "seen": {"existing": true, "type": "Microsoft.ServiceBus/namespaces", "apiVersion": "2022-10-01-preview", "name": existing_name, "dependsOn": ["queues"]}
            },
            "outputs": {
                "names": {"type": "array", "value": "[map(references('queues'), lambda('q', 'x', 'y', 'z'))]"},
                "ids": {"type": "array", "value": "[references('queues', 'Full')]"},
                "endpoint": {"type": "string", "value": "[reference('seen').serviceBusEndpoint]"},
                "full": {"type": "string", "value": "[reference('seen', '2022-10-01-preview', 'Full').name]"}
            }
        })
    };
    // a lambda with the wrong number of variables is an error that says so
    let r = deploy(&a, "rg-sym", "bad", tpl("sbsym01"), json!({})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("1 or 2 variable"), "{}", r.text());
    let mut good = tpl("sbsym01");
    good["outputs"]["names"]["value"] = json!("[map(references('queues', 'Full'), lambda('q', lambdaVariables('q').name))]");
    let o = outputs(&a, "rg-sym", "good", good.clone(), json!({})).await;
    // the copy with index 1 has a false condition: it is not deployed and not in the answer
    assert_eq!(o["names"], json!(["q0", "q2"]), "{o:?}");
    assert_eq!(o["ids"].as_array().unwrap().len(), 2);
    assert_eq!(o["endpoint"], json!(format!("https://sbsym01.servicebus.localhost:{}/", a.srv.port())));
    assert_eq!(o["full"], json!("sbsym01"));
    let list = a.get(&rid("rg-sym", "Microsoft.ServiceBus", "namespaces/sbsym01/queues")).await.json();
    assert_eq!(list["value"].as_array().unwrap().len(), 2);
    // an existing resource that does not exist
    let mut missing = tpl("nosuchns");
    missing["outputs"]["names"]["value"] = json!("[map(references('queues', 'Full'), lambda('q', lambdaVariables('q').name))]");
    let r = deploy(&a, "rg-sym", "missing", missing, json!({})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("nosuchns"), "{}", r.text());
    // 'resources' as an object needs languageVersion 2.0, and an array is refused in 2.0
    let mut no_version = good.clone();
    no_version.as_object_mut().unwrap().remove("languageVersion");
    assert_eq!(deploy(&a, "rg-sym", "v1", no_version, json!({})).await.status, 400);
    let array_in_v2 = json!({"languageVersion": "2.0", "contentVersion": "1.0.0.0", "resources": [], "outputs": {}});
    let r = deploy(&a, "rg-sym", "v2a", array_in_v2, json!({})).await;
    assert_eq!(r.status, 400, "{}", r.text());
}
