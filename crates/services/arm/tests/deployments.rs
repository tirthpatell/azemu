mod common;

use common::{full, standalone, SUB};
use serde_json::{json, Value};

fn dep(rg: &str, name: &str) -> String {
    format!("/subscriptions/{SUB}/resourcegroups/{rg}/providers/Microsoft.Resources/deployments/{name}")
}

fn template() -> Value {
    json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "parameters": {
            "prefix": {"type": "string", "defaultValue": "tpl"},
            "queueCount": {"type": "int", "defaultValue": 2},
            "location": {"type": "string", "defaultValue": "[resourceGroup().location]"}
        },
        "variables": {
            "account": "[toLower(concat(parameters('prefix'), 'store', uniqueString(resourceGroup().id)))]",
            "ns": "[concat(parameters('prefix'), '-bus-ns')]"
        },
        "resources": [
            {
                "type": "Microsoft.Storage/storageAccounts",
                "apiVersion": "2023-01-01",
                "name": "[variables('account')]",
                "location": "[parameters('location')]",
                "sku": {"name": "Standard_LRS"},
                "kind": "StorageV2",
                "properties": {}
            },
            {
                "type": "Microsoft.Storage/storageAccounts/blobServices/containers",
                "apiVersion": "2023-01-01",
                "name": "[concat(variables('account'), '/default/data')]",
                "dependsOn": ["[resourceId('Microsoft.Storage/storageAccounts', variables('account'))]"],
                "properties": {"publicAccess": "None"}
            },
            {
                "type": "Microsoft.ServiceBus/namespaces",
                "apiVersion": "2022-10-01-preview",
                "name": "[variables('ns')]",
                "location": "[parameters('location')]",
                "sku": {"name": "Standard"},
                "resources": [
                    {"type": "queues", "apiVersion": "2022-10-01-preview", "name": "nested-queue", "dependsOn": ["[variables('ns')]"], "properties": {"maxDeliveryCount": 4}}
                ]
            },
            {
                "type": "Microsoft.ServiceBus/namespaces/queues",
                "apiVersion": "2022-10-01-preview",
                "name": "[concat(variables('ns'), '/q', copyIndex())]",
                "copy": {"name": "queues", "count": "[parameters('queueCount')]"},
                "dependsOn": ["[resourceId('Microsoft.ServiceBus/namespaces', variables('ns'))]"],
                "properties": {}
            },
            {
                "condition": false,
                "type": "Microsoft.ServiceBus/namespaces/queues",
                "apiVersion": "2022-10-01-preview",
                "name": "[concat(variables('ns'), '/skipped')]",
                "properties": {}
            }
        ],
        "outputs": {
            "accountName": {"type": "string", "value": "[variables('account')]"},
            "blobEndpoint": {"type": "string", "value": "[reference(variables('account')).primaryEndpoints.blob]"},
            "count": {"type": "int", "value": "[parameters('queueCount')]"}
        }
    })
}

#[tokio::test]
async fn template_creates_resources_in_the_data_plane() {
    let a = full().await;
    a.group("rg-t").await;
    let r = a
        .put(
            &dep("rg-t", "d1"),
            json!({"properties": {"mode": "Incremental", "template": template(), "parameters": {"prefix": {"value": "tpl"}}}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let j = r.json();
    assert_eq!(j["properties"]["provisioningState"], "Succeeded", "{j}");
    assert_eq!(j["type"], "Microsoft.Resources/deployments");
    assert_eq!(j["properties"]["parameters"]["prefix"]["value"], "tpl");
    let account = j["properties"]["outputs"]["accountName"]["value"].as_str().unwrap().to_string();
    assert!(account.starts_with("tplstore"));
    assert_eq!(account.len(), 8 + 13);
    assert_eq!(j["properties"]["outputs"]["blobEndpoint"]["value"], format!("http://{account}.blob.core.localhost:{}/", a.srv.port()));
    assert_eq!(j["properties"]["outputs"]["count"]["value"], 2);
    assert_eq!(j["properties"]["outputResources"].as_array().unwrap().len(), 6);
    // the container exists in the Blob service
    let url = a.dp("blob", &account);
    let r = a.dp_send("PUT", &format!("{url}/data/f.txt"), &[("x-ms-blob-type", "BlockBlob")], "hi").await;
    assert_eq!(r.status, 201, "{}", r.text());
    // queues exist in Service Bus: nested, copied; the false condition was skipped
    let sb = format!("http://tpl-bus-ns.servicebus.localhost:{}", a.srv.port());
    for q in ["nested-queue", "q0", "q1"] {
        let r = a.dp_send("GET", &format!("{sb}/{q}?api-version=2021-05"), &[], "").await;
        assert!(r.text().contains("<QueueDescription"), "{q}: {}", r.text());
    }
    let r = a.dp_send("GET", &format!("{sb}/skipped?api-version=2021-05"), &[], "").await;
    assert!(!r.text().contains("<QueueDescription"));
    let r = a
        .get(&format!("/subscriptions/{SUB}/resourceGroups/rg-t/providers/Microsoft.ServiceBus/namespaces/tpl-bus-ns/queues/nested-queue"))
        .await;
    assert_eq!(r.json()["properties"]["maxDeliveryCount"], 4);
    // GET, list, operations, export, check existence
    assert_eq!(a.get(&dep("rg-t", "d1")).await.status, 200);
    assert_eq!(a.call("HEAD", &dep("rg-t", "d1"), None).await.status, 204);
    assert_eq!(a.call("HEAD", &dep("rg-t", "nope"), None).await.status, 404);
    let l = a.get(&format!("/subscriptions/{SUB}/resourcegroups/rg-t/providers/Microsoft.Resources/deployments")).await.json();
    assert_eq!(l["value"].as_array().unwrap().len(), 1);
    let ops = a.get(&format!("{}/operations", dep("rg-t", "d1"))).await.json();
    assert_eq!(ops["value"].as_array().unwrap().len(), 6);
    assert_eq!(ops["value"][0]["properties"]["provisioningState"], "Succeeded");
    let ex = a.post(&format!("{}/exportTemplate", dep("rg-t", "d1")), json!({})).await.json();
    assert!(ex["template"]["resources"].is_array());
    // a second PUT redeploys (200)
    let r = a.put(&dep("rg-t", "d1"), json!({"properties": {"mode": "Incremental", "template": template()}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["properties"]["provisioningState"], "Succeeded");
    assert_eq!(a.delete(&dep("rg-t", "d1")).await.status, 204);
    assert_eq!(a.get(&dep("rg-t", "d1")).await.status, 404);
}

#[tokio::test]
async fn bad_templates_are_refused() {
    let a = standalone().await;
    a.group("rg-b").await;
    let t = |tpl: Value, params: Value| json!({"properties": {"mode": "Incremental", "template": tpl, "parameters": params}});
    // missing required parameter
    let r = a.put(&dep("rg-b", "d"), t(json!({"parameters": {"x": {"type": "string"}}, "resources": []}), json!({}))).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "InvalidTemplate");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("'x'"));
    // parameter not in the template
    let r = a.put(&dep("rg-b", "d"), t(json!({"resources": []}), json!({"y": {"value": 1}}))).await;
    assert_eq!(r.status, 400);
    // allowed values
    let tpl = json!({"parameters": {"sku": {"type": "string", "allowedValues": ["a", "b"]}}, "resources": []});
    assert_eq!(a.put(&dep("rg-b", "d"), t(tpl.clone(), json!({"sku": {"value": "z"}}))).await.status, 400);
    assert_eq!(a.put(&dep("rg-b", "d"), t(tpl, json!({"sku": {"value": "a"}}))).await.status, 201);
    // unknown function
    let r = a
        .put(
            &dep("rg-b", "d2"),
            t(json!({"resources": [{"type": "Microsoft.Compute/disks", "name": "[bogus()]", "location": "eastus"}]}), json!({})),
        )
        .await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("bogus"));
    // templateLink and Complete mode are not built
    let r = a.put(&dep("rg-b", "d3"), json!({"properties": {"templateLink": {"uri": "https://example.invalid/t.json"}}})).await;
    assert_eq!(r.status, 501);
    let r = a.put(&dep("rg-b", "d4"), json!({"properties": {"mode": "Complete", "template": {"resources": []}}})).await;
    assert_eq!(r.status, 501);
    // the group must exist
    let r = a.put(&dep("nogroup", "d"), t(json!({"resources": []}), json!({}))).await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn validate_does_not_deploy() {
    let a = standalone().await;
    a.group("rg-v").await;
    let ok = a.post(&format!("{}/validate", dep("rg-v", "dv")), json!({"properties": {"mode": "Incremental", "template": {
        "resources": [{"type": "Microsoft.Network/virtualNetworks", "apiVersion": "2024-05-01", "name": "vn", "location": "eastus", "properties": {}}]}}})).await;
    assert_eq!(ok.status, 200, "{}", ok.text());
    assert_eq!(ok.json()["properties"]["provisioningState"], "Succeeded");
    assert_eq!(ok.json()["properties"]["validatedResources"].as_array().unwrap().len(), 1);
    assert_eq!(
        a.get(&format!("/subscriptions/{SUB}/resourceGroups/rg-v/providers/Microsoft.Network/virtualNetworks/vn")).await.status,
        404
    );
    assert_eq!(a.get(&dep("rg-v", "dv")).await.status, 404);
    let bad = a
        .post(
            &format!("{}/validate", dep("rg-v", "dv")),
            json!({"properties": {"mode": "Incremental", "template": {
        "resources": [{"type": "Microsoft.Network/virtualNetworks", "name": "[concat('a'", "properties": {"x": "[nothere()]"}}]}}}),
        )
        .await;
    assert_eq!(bad.status, 400);
    assert_eq!(bad.json()["error"]["code"], "InvalidTemplateDeployment");
    assert_eq!(bad.json()["error"]["details"][0]["code"], "InvalidTemplate");
}

#[tokio::test]
async fn failed_resource_marks_the_deployment_failed() {
    let a = full().await;
    a.group("rg-f").await;
    let tpl = json!({"resources": [
        {"type": "Microsoft.Storage/storageAccounts", "apiVersion": "2023-01-01", "name": "Bad_Name", "location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"}
    ]});
    let r = a.put(&dep("rg-f", "df"), json!({"properties": {"mode": "Incremental", "template": tpl}})).await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text());
    let j = r.json();
    assert_eq!(j["properties"]["provisioningState"], "Failed");
    assert_eq!(j["properties"]["error"]["code"], "DeploymentFailed");
    assert_eq!(j["properties"]["error"]["details"][0]["code"], "AccountNameInvalid");
    let ops = a.get(&format!("{}/operations", dep("rg-f", "df"))).await.json();
    assert_eq!(ops["value"][0]["properties"]["provisioningState"], "Failed");
}

#[tokio::test]
async fn dependencies_reorder_and_cycles_fail() {
    let a = standalone().await;
    a.group("rg-o").await;
    // "second" is listed first but depends on "first"; its body refers to first's properties
    let tpl = json!({"resources": [
        {"type": "Microsoft.Network/virtualNetworks", "name": "second", "location": "eastus", "dependsOn": ["first"], "properties": {"copyOf": "[reference('first').marker]"}},
        {"type": "Microsoft.Network/virtualNetworks", "name": "first", "location": "eastus", "properties": {"marker": "m1"}}
    ]});
    let r = a.put(&dep("rg-o", "d"), json!({"properties": {"mode": "Incremental", "template": tpl}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["provisioningState"], "Succeeded");
    let s = a.get(&format!("/subscriptions/{SUB}/resourceGroups/rg-o/providers/Microsoft.Network/virtualNetworks/second")).await.json();
    assert_eq!(s["properties"]["copyOf"], "m1");
    let cyc = json!({"resources": [
        {"type": "Microsoft.Network/virtualNetworks", "name": "a", "location": "eastus", "dependsOn": ["b"]},
        {"type": "Microsoft.Network/virtualNetworks", "name": "b", "location": "eastus", "dependsOn": ["a"]}
    ]});
    let r = a.put(&dep("rg-o", "cyc"), json!({"properties": {"mode": "Incremental", "template": cyc}})).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("Circular"));
}

#[tokio::test]
async fn subscription_scope_deployment_creates_a_group_and_nested_deployment() {
    let a = standalone().await;
    let tpl = json!({
        "parameters": {"name": {"type": "string"}},
        "resources": [
            {"type": "Microsoft.Resources/resourceGroups", "apiVersion": "2021-04-01", "name": "[parameters('name')]", "location": "westus"},
            {"type": "Microsoft.Resources/deployments", "apiVersion": "2021-04-01", "name": "inner", "resourceGroup": "[parameters('name')]",
             "dependsOn": ["[subscriptionResourceId('Microsoft.Resources/resourceGroups', parameters('name'))]"],
             "properties": {"mode": "Incremental", "template": {
                 "resources": [{"type": "Microsoft.Network/virtualNetworks", "apiVersion": "2024-05-01", "name": "inner-vnet", "location": "[resourceGroup().location]"}],
                 "outputs": {"where": {"type": "string", "value": "[resourceGroup().name]"}}}}}
        ],
        "outputs": {"innerWhere": {"type": "string", "value": "[reference('inner').outputs.where.value]"}}
    });
    let r = a.put(&format!("/subscriptions/{SUB}/providers/Microsoft.Resources/deployments/sub-dep"), json!({"location": "eastus", "properties": {"mode": "Incremental", "template": tpl, "parameters": {"name": {"value": "made-by-template"}}}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["provisioningState"], "Succeeded", "{}", r.text());
    assert_eq!(r.json()["properties"]["outputs"]["innerWhere"]["value"], "made-by-template");
    assert_eq!(a.get(&format!("/subscriptions/{SUB}/resourcegroups/made-by-template")).await.json()["location"], "westus");
    let v = a
        .get(&format!("/subscriptions/{SUB}/resourceGroups/made-by-template/providers/Microsoft.Network/virtualNetworks/inner-vnet"))
        .await
        .json();
    assert_eq!(v["location"], "westus");
    // listing at subscription scope
    let l = a.get(&format!("/subscriptions/{SUB}/providers/Microsoft.Resources/deployments")).await.json();
    assert_eq!(l["value"].as_array().unwrap().len(), 1);
}

// ---- limits on what a template can ask for ----

fn put_template(rg: &str, name: &str, tpl: Value) -> (String, Value) {
    (dep(rg, name), json!({"properties": {"mode": "Incremental", "template": tpl}}))
}

#[tokio::test]
async fn deeply_nested_expressions_answer_invalid_template_and_the_service_lives() {
    let a = standalone().await;
    a.group("rg-deep").await;
    for depth in [100usize, 2000, 100_000] {
        let expr = format!("[{}'a'{}]", "concat(".repeat(depth), ")".repeat(depth));
        // an output, a resource property, and a resource name
        let tpl = json!({"$schema": "x", "resources": [], "outputs": {"o": {"type": "string", "value": expr}}});
        let (path, body) = put_template("rg-deep", "d1", tpl);
        let r = a.put(&path, body).await;
        assert_eq!(r.status, 400, "depth {depth}: {}", r.text());
        assert_eq!(r.json()["error"]["code"], "InvalidTemplate");
        let tpl = json!({"resources": [{"type": "Microsoft.Network/virtualNetworks", "name": "vn", "location": "eastus", "properties": {"x": expr}}]});
        let (path, body) = put_template("rg-deep", "d2", tpl);
        let r = a.put(&path, body).await;
        assert_eq!(r.status, 400, "depth {depth}: {}", r.text());
        assert_eq!(r.json()["error"]["code"], "InvalidTemplate");
        let tpl = json!({"resources": [{"type": "Microsoft.Network/virtualNetworks", "name": expr, "location": "eastus"}]});
        let (path, body) = put_template("rg-deep", "d3", tpl);
        assert_eq!(a.put(&path, body).await.status, 400);
    }
    // 50 levels are fine and the service still answers
    let expr = format!("[{}'a'{}]", "concat(".repeat(50), ")".repeat(50));
    let (path, body) =
        put_template("rg-deep", "ok", json!({"$schema": "x", "resources": [], "outputs": {"o": {"type": "string", "value": expr}}}));
    let r = a.put(&path, body).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["outputs"]["o"]["value"], "a");
}

#[tokio::test]
async fn expression_errors_that_break_a_limit_are_invalid_template_in_resource_properties_too() {
    let a = standalone().await;
    a.group("rg-lim").await;
    for expr in [
        "[range(0, 2000000000)]",
        "[div(1, 0)]",
        "[add(9223372036854775807, 1)]",
        "[mul(9223372036854775807, 2)]",
        "[mod(1, 0)]",
        "[format('{0:D4000000000}', 1)]",
    ] {
        let tpl = json!({"resources": [{"type": "Microsoft.Network/virtualNetworks", "name": "vn", "location": "eastus", "properties": {"x": expr}}]});
        let (path, body) = put_template("rg-lim", "d", tpl);
        let r = a.put(&path, body).await;
        assert_eq!(r.status, 400, "{expr}: {}", r.text());
        assert_eq!(r.json()["error"]["code"], "InvalidTemplate", "{expr}");
        let tpl = json!({"resources": [], "outputs": {"o": {"type": "int", "value": expr}}});
        let (path, body) = put_template("rg-lim", "d", tpl);
        assert_eq!(a.put(&path, body).await.status, 400, "{expr}");
    }
}

#[tokio::test]
async fn copy_counts_and_resource_counts_are_limited() {
    let a = standalone().await;
    a.group("rg-copy").await;
    let one = |copy: Value| json!({"resources": [{"type": "Microsoft.Network/virtualNetworks", "name": "[concat('vn', copyIndex())]", "location": "eastus", "copy": copy}]});
    for count in [json!(2_000_000_000), json!(801), json!(-1)] {
        let (path, body) = put_template("rg-copy", "d", one(json!({"name": "c", "count": count})));
        let r = a.put(&path, body).await;
        assert_eq!(r.status, 400, "{}", r.text());
        assert!(r.json()["error"]["message"].as_str().unwrap().contains("copy count"));
    }
    // 800 is allowed
    let (path, body) = put_template("rg-copy", "d800", one(json!({"name": "c", "count": 800})));
    let r = a.put(&path, body).await;
    assert_eq!(r.status, 201, "{}", r.text());
    // two loops that together pass 800
    let tpl = json!({"resources": [
        {"type": "Microsoft.Network/virtualNetworks", "name": "[concat('a', copyIndex())]", "location": "eastus", "copy": {"name": "c", "count": 500}},
        {"type": "Microsoft.Network/virtualNetworks", "name": "[concat('b', copyIndex())]", "location": "eastus", "copy": {"name": "c", "count": 500}}
    ]});
    let (path, body) = put_template("rg-copy", "d1000", tpl);
    let r = a.put(&path, body).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("800 resources"));
    // a copy loop in properties
    let tpl = json!({"resources": [], "outputs": {"o": {"type": "array", "value": "[range(0, 10001)]"}}});
    let (path, body) = put_template("rg-copy", "dr", tpl);
    assert_eq!(a.put(&path, body).await.status, 400);
}

#[tokio::test]
async fn variables_are_evaluated_once_each() {
    let a = standalone().await;
    a.group("rg-var").await;
    // v_i = concat(v_(i-1), v_(i-1)): 2^30 evaluations without a cache. Now the string is too big after 22 levels, quickly.
    let mut vars = serde_json::Map::new();
    vars.insert("v0".into(), json!("x"));
    for i in 1..=30 {
        vars.insert(format!("v{i}"), json!(format!("[concat(variables('v{}'), variables('v{}'))]", i - 1, i - 1)));
    }
    let started = std::time::Instant::now();
    let tpl = json!({"$schema": "x", "variables": vars.clone(), "resources": [], "outputs": {"o": {"type": "int", "value": "[length(variables('v30'))]"}}});
    let (path, body) = put_template("rg-var", "big", tpl);
    let r = a.put(&path, body).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(started.elapsed().as_secs() < 10, "took {:?}", started.elapsed());
    // 14 levels: fine, and the answer is right
    let tpl = json!({"$schema": "x", "variables": vars, "resources": [], "outputs": {"o": {"type": "int", "value": "[length(variables('v14'))]"}}});
    let (path, body) = put_template("rg-var", "ok", tpl);
    let r = a.put(&path, body).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["outputs"]["o"]["value"], 16384);
}

#[tokio::test]
async fn nested_deployments_and_dependency_chains_are_bounded() {
    let a = standalone().await;
    a.group("rg-nest").await;
    // 12 deployments inside each other: refused beyond 10 levels, with a plain message
    let mut inner = json!({"resources": []});
    for _ in 0..12 {
        inner = json!({"resources": [{"type": "Microsoft.Resources/deployments", "apiVersion": "2022-09-01", "name": "n", "properties": {"mode": "Incremental", "template": inner}}]});
    }
    let (path, body) = put_template("rg-nest", "nest", inner);
    let r = a.put(&path, body).await;
    let text = r.text();
    assert!(text.contains("levels deep"), "{} {text}", r.status);
    // 6 levels are fine
    let mut inner = json!({"resources": []});
    for _ in 0..6 {
        inner = json!({"resources": [{"type": "Microsoft.Resources/deployments", "apiVersion": "2022-09-01", "name": "n", "properties": {"mode": "Incremental", "template": inner}}]});
    }
    let (path, body) = put_template("rg-nest", "nest6", inner);
    let r = a.put(&path, body).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["provisioningState"], "Succeeded", "{}", r.text());
    // 800 resources, each depending on all the others in front of it, listed in reverse: ordered, quickly
    let n = 800;
    let resources: Vec<Value> = (0..n)
        .map(|i| {
            let deps: Vec<String> = (0..i).map(|j| format!("vn{j}")).collect();
            json!({"type": "Microsoft.Network/virtualNetworks", "name": format!("vn{i}"), "location": "eastus", "dependsOn": deps})
        })
        .rev()
        .collect();
    let started = std::time::Instant::now();
    let (path, body) = put_template("rg-nest", "chain", json!({"resources": resources}));
    let r = a.put(&path, body).await;
    assert_eq!(r.status, 201, "{}", &r.text()[..200.min(r.text().len())]);
    assert_eq!(r.json()["properties"]["provisioningState"], "Succeeded");
    assert!(started.elapsed().as_secs() < 20, "took {:?}", started.elapsed());
    // a cycle is found
    let cyc = json!({"resources": [
        {"type": "Microsoft.Network/virtualNetworks", "name": "a", "location": "eastus", "dependsOn": ["b"]},
        {"type": "Microsoft.Network/virtualNetworks", "name": "b", "location": "eastus", "dependsOn": ["a"]}
    ]});
    let (path, body) = put_template("rg-nest", "cyc", cyc);
    let r = a.put(&path, body).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("Circular"));
}

#[tokio::test]
async fn too_many_parameters_variables_or_outputs_are_refused() {
    let a = standalone().await;
    a.group("rg-cnt").await;
    let many = |n: usize, v: Value| -> Value { Value::Object((0..n).map(|i| (format!("k{i}"), v.clone())).collect()) };
    let cases = [
        ("parameters", json!({"parameters": many(257, json!({"type": "string", "defaultValue": "x"})), "resources": []})),
        ("variables", json!({"variables": many(513, json!("x")), "resources": []})),
        ("outputs", json!({"outputs": many(65, json!({"type": "string", "value": "x"})), "resources": []})),
    ];
    for (what, tpl) in cases {
        let (path, body) = put_template("rg-cnt", "d", tpl);
        let r = a.put(&path, body).await;
        assert_eq!(r.status, 400, "{what}: {}", r.text());
        assert!(r.json()["error"]["message"].as_str().unwrap().contains(what), "{what}");
    }
    let (path, body) = put_template(
        "rg-cnt",
        "ok",
        json!({"parameters": many(256, json!({"type": "string", "defaultValue": "x"})), "variables": many(512, json!("x")), "resources": [], "outputs": many(64, json!({"type": "string", "value": "x"}))}),
    );
    assert_eq!(a.put(&path, body).await.status, 201);
}

// ---- deployment history is bounded ----------------------------------------------------------------------

fn empty_template() -> Value {
    json!({"$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#", "contentVersion": "1.0.0.0", "resources": [], "outputs": {}})
}

#[tokio::test]
async fn a_group_keeps_at_most_800_deployments_and_the_oldest_go() {
    let a = standalone().await;
    a.group("rg").await;
    for i in 0..812 {
        let r = a.put(&dep("rg", &format!("d{i:04}")), json!({"properties": {"mode": "Incremental", "template": empty_template()}})).await;
        assert_eq!(r.status, 201, "{i}: {}", r.text());
    }
    let list = a.get(&format!("/subscriptions/{SUB}/resourcegroups/rg/providers/Microsoft.Resources/deployments")).await.json();
    let names: Vec<&str> = list["value"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 800);
    assert!(!names.contains(&"d0011") && names.contains(&"d0012") && names.contains(&"d0811"), "the 12 oldest are gone");
    assert_eq!(a.get(&dep("rg", "d0000")).await.status, 404);
    assert_eq!(a.get(&dep("rg", "d0811")).await.status, 200);
    // another group has its own 800
    a.group("other").await;
    assert_eq!(
        a.put(&dep("other", "d0000"), json!({"properties": {"mode": "Incremental", "template": empty_template()}})).await.status,
        201
    );
    assert_eq!(a.get(&dep("rg", "d0012")).await.status, 200);
}
