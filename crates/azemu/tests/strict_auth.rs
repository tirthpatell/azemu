//! Strict authorisation, end to end (`docs/strict-auth.md`): real tokens from the identity service, role assignments made through Resource Manager and
//! the admin API, data planes that check them, and the answers to "why was I refused".

use azemu_core::auth::Auth;
use azemu_core::serde_json::{json, Value};
use azemu_core::testing::{TestResponse, TestServer};

const SUB: &str = "11111111-2222-3333-4444-555555555555";
const OWNER: &str = "b7e6dc6d-f1e8-4753-8033-0f276bb0955b";
const READER: &str = "2a2b9908-6ea1-4ae2-8e65-a410df84e7d1";
const MAIN: &str = "aaaaaaaa-0000-0000-0000-000000000001";
const READER_ID: &str = "aaaaaaaa-0000-0000-0000-000000000002";
const NOROLE: &str = "aaaaaaaa-0000-0000-0000-000000000003";

struct Env {
    srv: TestServer,
}

async fn start() -> Env {
    let stack = azemu::full_stack_with(Auth::strict());
    Env { srv: TestServer::start_with(stack.services, stack.raw).await }
}

impl Env {
    fn gw(&self) -> String {
        self.srv.gateway_url()
    }

    async fn token(&self, client: &str, resource: &str) -> String {
        let r = self.srv.send("GET", &format!("{}/_azemu/identity/token?resource={resource}&client_id={client}", self.gw()), &[], "").await;
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()["access_token"].as_str().unwrap().to_string()
    }

    async fn oid(&self, client: &str) -> String {
        let r = self.srv.send("GET", &format!("{}/_azemu/identity/principal?client_id={client}", self.gw()), &[], "").await;
        r.json()["objectId"].as_str().unwrap().to_string()
    }

    async fn arm(&self, method: &str, path: &str, body: Value) -> TestResponse {
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = format!("http://management.localhost:{}{path}{sep}api-version=2022-04-01", self.srv.port());
        let t = self.token(MAIN, "https://management.azure.com").await;
        self.srv
            .send(method, &url, &[("authorization", &format!("Bearer {t}")), ("content-type", "application/json")], body.to_string())
            .await
    }

    async fn blob(&self, client: &str, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> TestResponse {
        let t = self.token(client, "https://storage.azure.com").await;
        let mut h = vec![("authorization", format!("Bearer {t}")), ("x-ms-version", "2024-11-04".to_string())];
        h.extend(extra.iter().map(|(k, v)| (*k, v.to_string())));
        let hs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.srv.send(method, &format!("http://stracct.blob.core.localhost:{}{path}", self.srv.port()), &hs, body.to_string()).await
    }

    async fn assign(&self, scope: &str, name: &str, role: &str, principal: &str) -> TestResponse {
        self.arm(
            "PUT",
            &format!("{scope}/providers/Microsoft.Authorization/roleAssignments/{name}"),
            json!({"properties": {"roleDefinitionId": format!("/subscriptions/{SUB}/providers/Microsoft.Authorization/roleDefinitions/{role}"), "principalId": principal, "principalType": "ServicePrincipal"}}),
        )
        .await
    }
}

const GROUP: &str = "/subscriptions/11111111-2222-3333-4444-555555555555/resourceGroups/rg";
const ACCT: &str =
    "/subscriptions/11111111-2222-3333-4444-555555555555/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/stracct";

async fn world(e: &Env) -> (String, String, String) {
    assert!(matches!(e.arm("PUT", GROUP, json!({"location": "eastus"})).await.status, 200 | 201));
    let r = e.arm("PUT", ACCT, json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    (e.oid(MAIN).await, e.oid(READER_ID).await, e.oid(NOROLE).await)
}

#[tokio::test]
async fn a_role_given_through_resource_manager_decides_what_the_data_plane_allows() {
    let e = start().await;
    let (main, reader, _norole) = world(&e).await;
    // the start: nobody has a role, so even the creator of the account is refused data calls
    let r = e.blob(MAIN, "PUT", "/photos?restype=container", &[], "").await;
    assert_eq!(r.status, 403, "{}", r.text());
    // Terraform's way: look the role up by name, then create the assignment under a GUID
    let r = e.arm("GET", &format!("/subscriptions/{SUB}/providers/Microsoft.Authorization/roleDefinitions?$filter=roleName%20eq%20%27Storage%20Blob%20Data%20Owner%27"), json!(null)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["value"][0]["name"], OWNER);
    assert_eq!(
        r.json()["value"][0]["properties"]["permissions"][0]["dataActions"][0],
        "Microsoft.Storage/storageAccounts/blobServices/containers/blobs/*"
    );
    let name = "0f8f2bcb-1a1b-4c1a-8c0e-111111111111";
    let r = e.assign(GROUP, name, OWNER, &main).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let j = r.json();
    assert_eq!(j["name"], name);
    assert_eq!(j["id"], format!("{GROUP}/providers/Microsoft.Authorization/roleAssignments/{name}"));
    assert_eq!(j["properties"]["scope"], GROUP);
    assert_eq!(j["properties"]["principalId"], main);
    assert_eq!(
        j["properties"]["roleDefinitionId"],
        format!("/subscriptions/{SUB}/providers/Microsoft.Authorization/roleDefinitions/{OWNER}")
    );
    // read back, the same again is not an error
    assert_eq!(
        e.arm("GET", &format!("{GROUP}/providers/Microsoft.Authorization/roleAssignments/{name}"), json!(null)).await.json()["properties"]
            ["principalId"],
        main
    );
    assert_eq!(e.assign(GROUP, name, OWNER, &main).await.status, 200);
    // the same principal, role and scope under another name
    let r = e.assign(GROUP, "0f8f2bcb-1a1b-4c1a-8c0e-222222222222", OWNER, &main).await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str().unwrap().to_string()), (409, "RoleAssignmentExists".to_string()));
    // the data plane now works, for the owner and not for the others
    assert_eq!(e.blob(MAIN, "PUT", "/photos?restype=container", &[], "").await.status, 201);
    assert_eq!(e.blob(MAIN, "PUT", "/photos/a.txt", &[("x-ms-blob-type", "BlockBlob")], "hello").await.status, 201);
    assert_eq!(e.blob(READER_ID, "GET", "/photos/a.txt", &[], "").await.status, 403);
    // a reader, with the role given at the account
    assert_eq!(e.assign(ACCT, "0f8f2bcb-1a1b-4c1a-8c0e-333333333333", READER, &reader).await.status, 201);
    let r = e.blob(READER_ID, "GET", "/photos/a.txt", &[], "").await;
    assert_eq!((r.status, r.text().as_str()), (200, "hello"));
    let r = e.blob(READER_ID, "PUT", "/photos/b.txt", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (403, Some("AuthorizationPermissionMismatch")));
    // lists: at the scope and below, at or above, for a principal
    let all = e.arm("GET", &format!("/subscriptions/{SUB}/providers/Microsoft.Authorization/roleAssignments"), json!(null)).await.json();
    assert_eq!(all["value"].as_array().unwrap().len(), 2);
    let at = e.arm("GET", &format!("{ACCT}/providers/Microsoft.Authorization/roleAssignments?$filter=atScope()"), json!(null)).await.json();
    assert_eq!(at["value"].as_array().unwrap().len(), 2, "the group's assignment applies above the account");
    let one = e
        .arm(
            "GET",
            &format!("/subscriptions/{SUB}/providers/Microsoft.Authorization/roleAssignments?$filter=principalId%20eq%20%27{reader}%27"),
            json!(null),
        )
        .await
        .json();
    assert_eq!(one["value"].as_array().unwrap().len(), 1);
    // removed: the rights go
    assert_eq!(
        e.arm(
            "DELETE",
            &format!("{ACCT}/providers/Microsoft.Authorization/roleAssignments/0f8f2bcb-1a1b-4c1a-8c0e-333333333333"),
            json!(null)
        )
        .await
        .status,
        200
    );
    assert_eq!(e.blob(READER_ID, "GET", "/photos/a.txt", &[], "").await.status, 403);
    assert_eq!(
        e.arm(
            "DELETE",
            &format!("{ACCT}/providers/Microsoft.Authorization/roleAssignments/0f8f2bcb-1a1b-4c1a-8c0e-333333333333"),
            json!(null)
        )
        .await
        .status,
        204
    );
}

#[tokio::test]
async fn resource_manager_answers_the_mistakes_with_azures_codes() {
    let e = start().await;
    let (main, _, _) = world(&e).await;
    let n = "0f8f2bcb-1a1b-4c1a-8c0e-444444444444";
    // an unknown role definition
    let r = e.assign(GROUP, n, "99999999-9999-9999-9999-999999999999", &main).await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str().unwrap().to_string()), (400, "RoleDefinitionDoesNotExist".to_string()));
    // a principal id that is not a GUID
    let r = e.assign(GROUP, n, OWNER, "not-a-guid").await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str().unwrap().to_string()), (400, "InvalidRequestContent".to_string()));
    // a scope that does not exist
    let r = e.assign(&format!("/subscriptions/{SUB}/resourceGroups/nothere"), n, OWNER, &main).await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str().unwrap().to_string()), (404, "ResourceGroupNotFound".to_string()));
    let r = e.assign(&format!("{GROUP}/providers/Microsoft.Storage/storageAccounts/nothere"), n, OWNER, &main).await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str().unwrap().to_string()), (404, "ResourceNotFound".to_string()));
    // a name that is not a GUID
    assert_eq!(e.assign(GROUP, "name", OWNER, &main).await.status, 400);
    // the name of another principal's assignment
    assert_eq!(e.assign(GROUP, n, OWNER, &main).await.status, 201);
    let r = e.assign(GROUP, n, OWNER, "bbbbbbbb-0000-0000-0000-000000000009").await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str().unwrap().to_string()), (409, "RoleAssignmentUpdateNotPermitted".to_string()));
    // custom roles: made, found by name, used, and not deleted while used
    let id = "cccccccc-0000-0000-0000-000000000001";
    let r = e
        .arm(
            "PUT",
            &format!("/subscriptions/{SUB}/providers/Microsoft.Authorization/roleDefinitions/{id}"),
            json!({"properties": {"roleName": "Blob lister", "description": "d", "type": "CustomRole", "assignableScopes": [format!("/subscriptions/{SUB}")], "permissions": [{"actions": [], "notActions": [], "dataActions": ["Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read"], "notDataActions": []}]}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["type"], "CustomRole");
    let found = e
        .arm(
            "GET",
            &format!("/subscriptions/{SUB}/providers/Microsoft.Authorization/roleDefinitions?$filter=roleName%20eq%20%27Blob%20lister%27"),
            json!(null),
        )
        .await
        .json();
    assert_eq!(found["value"][0]["name"], id);
    assert_eq!(e.assign(ACCT, "0f8f2bcb-1a1b-4c1a-8c0e-555555555555", id, &main).await.status, 201);
    let r = e.arm("DELETE", &format!("/subscriptions/{SUB}/providers/Microsoft.Authorization/roleDefinitions/{id}"), json!(null)).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "RoleDefinitionHasAssignments");
    // a built-in role cannot be changed
    let r = e
        .arm(
            "PUT",
            &format!("/subscriptions/{SUB}/providers/Microsoft.Authorization/roleDefinitions/{OWNER}"),
            json!({"properties": {"roleName": "x"}}),
        )
        .await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn the_admin_api_gives_roles_to_accounts_resource_manager_does_not_know_and_says_why() {
    let e = start().await;
    let main = e.oid(MAIN).await;
    // an account nobody made through Resource Manager: no group, so only a scope that names it covers it
    let r = e
        .srv
        .send(
            "PUT",
            &format!("{}/_azemu/auth/assignments", e.gw()),
            &[("content-type", "application/json")],
            json!({"clientId": MAIN, "role": "Storage Blob Data Contributor", "scope": "storage/stracct/containers/data"}).to_string(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["properties"]["principalId"], main);
    assert!(r.json()["properties"]["scope"]
        .as_str()
        .unwrap()
        .ends_with("/providers/Microsoft.Storage/storageAccounts/stracct/blobServices/default/containers/data"));
    assert_eq!(e.blob(MAIN, "PUT", "/elsewhere?restype=container", &[], "").await.status, 403, "another container is not covered");
    assert_eq!(e.blob(MAIN, "PUT", "/data?restype=container", &[], "").await.status, 201);
    assert_eq!(e.blob(MAIN, "PUT", "/data/x", &[("x-ms-blob-type", "BlockBlob")], "1").await.status, 201);
    assert_eq!(e.blob(MAIN, "PUT", "/other/x", &[("x-ms-blob-type", "BlockBlob")], "1").await.status, 403);
    // one step to find out why
    let url = format!("{}/_azemu/auth/check?principal={main}&action=Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read&scope=storage/stracct/containers/other", e.gw());
    let r = e.srv.send("GET", &url, &[], "").await;
    let v = r.json();
    assert_eq!(v["allowed"], false, "{}", r.text());
    assert_eq!(v["lookedAt"][0]["applies"], false);
    assert!(v["reason"].as_str().unwrap().contains("none applies"), "{v}");
    let url = url.replace("containers/other", "containers/data");
    assert_eq!(e.srv.send("GET", &url, &[], "").await.json()["allowed"], true);
    // GET and DELETE
    let list = e.srv.send("GET", &format!("{}/_azemu/auth/assignments?clientId={MAIN}", e.gw()), &[], "").await.json();
    assert_eq!(list["value"].as_array().unwrap().len(), 1);
    let r = e.srv.send("DELETE", &format!("{}/_azemu/auth/assignments?clientId={MAIN}", e.gw()), &[], "").await;
    assert_eq!(r.json()["removed"].as_array().unwrap().len(), 1);
    assert_eq!(e.blob(MAIN, "PUT", "/data/y", &[("x-ms-blob-type", "BlockBlob")], "1").await.status, 403);
    // mistakes in the request
    for body in [
        json!({"clientId": MAIN, "role": "No such role", "scope": "storage/a"}),
        json!({"clientId": MAIN, "role": "Reader", "scope": "nonsense/a"}),
        json!({"role": "Reader", "scope": "storage/a"}),
    ] {
        let r = e
            .srv
            .send("PUT", &format!("{}/_azemu/auth/assignments", e.gw()), &[("content-type", "application/json")], body.to_string())
            .await;
        assert_eq!(r.status, 400, "{body}: {}", r.text());
    }
    // the mode shows
    let c = e.srv.send("GET", &format!("{}/_azemu/config", e.gw()), &[], "").await.json();
    assert_eq!(c["strictAuth"], true);
    let a = e.srv.send("GET", &format!("{}/_azemu/auth", e.gw()), &[], "").await.json();
    assert_eq!(a["strict"], true);
    let enforced: Vec<&str> = a["checksRoles"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    for s in ["blob", "dfs", "queue", "table", "keyvault", "appconfig", "cosmos", "eventgrid"] {
        assert!(enforced.contains(&s), "{s} is not listed: {enforced:?}");
    }
}

#[tokio::test]
async fn the_mode_off_changes_nothing_and_the_internal_calls_of_resource_manager_pass() {
    // off: any token, as before; roles are still stored and served
    let stack = azemu::full_stack_with(Auth::disabled());
    let e = Env { srv: TestServer::start_with(stack.services, stack.raw).await };
    let r = e
        .srv
        .send(
            "PUT",
            &format!("http://stracct.blob.core.localhost:{}/c1x?restype=container", e.srv.port()),
            &[("authorization", "Bearer anything"), ("x-ms-version", "2024-11-04")],
            "",
        )
        .await;
    assert_eq!(r.status, 201);
    assert!(matches!(e.arm("PUT", GROUP, json!({"location": "eastus"})).await.status, 200 | 201));
    assert_eq!(e.assign(GROUP, "0f8f2bcb-1a1b-4c1a-8c0e-666666666666", OWNER, "bbbbbbbb-0000-0000-0000-000000000009").await.status, 201);
    // on: a container made through Resource Manager (it calls the Blob service itself) works with nobody having a role
    let e = start().await;
    world(&e).await;
    let r = e.arm("PUT", &format!("{ACCT}/blobServices/default/containers/viaarm"), json!({"properties": {}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

/// The text the emulator's own services used to send to each other must mean nothing from the network: it is an ordinary bearer token there, and
/// not a valid one.
#[tokio::test]
async fn a_request_from_the_network_with_the_old_internal_token_text_is_an_ordinary_bad_token() {
    let e = start().await;
    world(&e).await;
    let port = e.srv.port();
    for text in ["azemu-arm", "azemu-seed"] {
        let auth = format!("Bearer {text}");
        let h: &[(&str, &str)] = &[("authorization", auth.as_str()), ("x-ms-version", "2024-11-04")];
        for url in [
            format!("http://stracct.blob.core.localhost:{port}/photos?restype=container&comp=list"),
            format!("http://stracct.blob.core.localhost:{port}/?comp=list"),
            format!("http://stracct.queue.core.localhost:{port}/?comp=list"),
            format!("http://stracct.table.core.localhost:{port}/Tables"),
        ] {
            let r = e.srv.send("GET", &url, h, "").await;
            assert_eq!(r.status, 401, "{url} with {text}: {}", r.text());
        }
        let r = e.srv.send("GET", &format!("http://v1.vault.localhost:{port}/secrets?api-version=7.4"), h, "").await;
        assert_eq!(r.status, 401, "key vault with {text}: {}", r.text());
        let r = e.srv.send("GET", &format!("http://ac1.azconfig.localhost:{port}/kv"), h, "").await;
        assert_eq!(r.status, 401, "app configuration with {text}: {}", r.text());
    }
}

#[tokio::test]
async fn key_vault_role_based_and_policy_vaults_made_through_resource_manager() {
    let e = start().await;
    let (main, reader, _) = world(&e).await;
    let vault = |name: &str, rbac: bool, policies: Value| json!({"location": "eastus", "properties": {"tenantId": "00000000-0000-0000-0000-000000000000", "sku": {"family": "A", "name": "standard"}, "enableRbacAuthorization": rbac, "accessPolicies": policies}, "name": name});
    let kv = |n: &str| format!("{GROUP}/providers/Microsoft.KeyVault/vaults/{n}");
    assert_eq!(e.arm("PUT", &kv("rbacv"), vault("rbacv", true, json!([]))).await.status, 200);
    let policies = json!([{"tenantId": "t", "objectId": main, "permissions": {"secrets": ["get", "set", "list"], "keys": [], "certificates": []}}, {"tenantId": "t", "objectId": reader, "permissions": {"secrets": ["get"], "keys": [], "certificates": []}}]);
    assert_eq!(e.arm("PUT", &kv("polv"), vault("polv", false, policies)).await.status, 200);
    let call = |client: &'static str, v: &'static str, method: &'static str, path: &'static str, body: &'static str| {
        let e = &e;
        async move {
            let t = e.token(client, "https://vault.azure.net").await;
            e.srv
                .send(
                    method,
                    &format!("http://{v}.vault.localhost:{}{path}", e.srv.port()),
                    &[("authorization", &format!("Bearer {t}")), ("content-type", "application/json")],
                    body.to_string(),
                )
                .await
        }
    };
    // the policy vault: main sets, the reader gets, the reader may not set; nobody else has an entry
    assert_eq!(call(MAIN, "polv", "PUT", "/secrets/s1?api-version=7.5", r#"{"value":"v"}"#).await.status, 200);
    assert_eq!(call(READER_ID, "polv", "GET", "/secrets/s1?api-version=7.5", "").await.status, 200);
    let r = call(READER_ID, "polv", "PUT", "/secrets/s2?api-version=7.5", r#"{"value":"v"}"#).await;
    assert_eq!(r.json()["error"]["innererror"]["code"], "ForbiddenByPolicy");
    let r = call(NOROLE, "polv", "GET", "/secrets/s1?api-version=7.5", "").await;
    assert_eq!(r.json()["error"]["innererror"]["code"], "AccessDenied");
    // the role-based vault: roles, as the id Resource Manager gave the vault shows in the answer
    let r = call(MAIN, "rbacv", "PUT", "/secrets/s1?api-version=7.5", r#"{"value":"v"}"#).await;
    assert_eq!(r.status, 403);
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&format!("Resource: '/subscriptions/{SUB}/resourcegroups/rg/providers/microsoft.keyvault/vaults/rbacv/secrets/s1'")),
        "{}",
        r.text()
    );
    assert_eq!(
        e.assign(&kv("rbacv"), "0f8f2bcb-1a1b-4c1a-8c0e-777777777777", "b86a8fe4-44ce-4948-aee5-eccb2c155cd7", &main).await.status,
        201
    );
    assert_eq!(call(MAIN, "rbacv", "PUT", "/secrets/s1?api-version=7.5", r#"{"value":"v"}"#).await.status, 200);
}

#[tokio::test]
async fn cosmos_roles_are_made_through_resource_manager() {
    let e = start().await;
    let (main, _, _) = world(&e).await;
    let acct = format!("{GROUP}/providers/Microsoft.DocumentDB/databaseAccounts/cosmosacct");
    let r = e.arm("PUT", &acct, json!({"location": "eastus", "kind": "GlobalDocumentDB", "properties": {"databaseAccountOfferType": "Standard", "locations": [{"locationName": "East US", "failoverPriority": 0}]}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // the two built-in definitions are there
    let r = e.arm("GET", &format!("{acct}/sqlRoleDefinitions/00000000-0000-0000-0000-000000000002"), json!(null)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["properties"]["roleName"], "Cosmos DB Built-in Data Contributor");
    let id = "dddddddd-0000-0000-0000-000000000001";
    let r = e.arm("PUT", &format!("{acct}/sqlRoleAssignments/{id}"), json!({"properties": {"roleDefinitionId": format!("{acct}/sqlRoleDefinitions/00000000-0000-0000-0000-000000000002"), "scope": acct, "principalId": main}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // the registry has it: asked for through the admin check
    let url = format!("{}/_azemu/auth/check?principal={main}&cosmosAccount=cosmosacct&action=Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/items/create&scope=/dbs/d1/colls/c1", e.gw());
    assert_eq!(e.srv.send("GET", &url, &[], "").await.json()["allowed"], true);
    assert_eq!(e.arm("DELETE", &format!("{acct}/sqlRoleAssignments/{id}"), json!(null)).await.status, 204);
    assert_eq!(e.srv.send("GET", &url, &[], "").await.json()["allowed"], false);
}

fn scratch(what: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "azemu-strict-{what}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test]
async fn assignments_survive_a_restart_and_the_config_file_survives_a_reset() {
    let dir = scratch("data");
    let cfg = scratch("cfg").join("auth.json");
    std::fs::write(
        &cfg,
        json!({"assignments": [{"clientId": NOROLE, "role": "Storage Blob Data Reader", "scope": "storage/fileacct/containers/data"}]})
            .to_string(),
    )
    .unwrap();
    let start_on = |dir: std::path::PathBuf, cfg: std::path::PathBuf| async move {
        let stack = azemu::full_stack_with(Auth::with(true, Some(cfg)));
        Env { srv: TestServer::try_start_persistent(stack.services, &dir, |b| b).await.expect("start") }
    };
    let e = start_on(dir.clone(), cfg.clone()).await;
    let (main, reader, _) = world(&e).await;
    assert_eq!(e.assign(GROUP, "0f8f2bcb-1a1b-4c1a-8c0e-888888888888", OWNER, &main).await.status, 201);
    assert_eq!(e.assign(ACCT, "0f8f2bcb-1a1b-4c1a-8c0e-999999999999", READER, &reader).await.status, 201);
    let r = e
        .srv
        .send(
            "PUT",
            &format!("{}/_azemu/auth/assignments", e.gw()),
            &[("content-type", "application/json")],
            json!({"principalId": "eeeeeeee-0000-0000-0000-000000000001", "role": "Reader", "scope": "storage/other"}).to_string(),
        )
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(e.blob(MAIN, "PUT", "/photos?restype=container", &[], "").await.status, 201);
    e.srv.shutdown().await.unwrap();

    // a new start on the same directory: Resource Manager's assignments and the API's are back, and the account Resource Manager made is
    // still known by its id (so the group-wide assignment still reaches it)
    let e = start_on(dir.clone(), cfg.clone()).await;
    let list = e.srv.send("GET", &format!("{}/_azemu/auth/assignments", e.gw()), &[], "").await.json();
    assert_eq!(list["value"].as_array().unwrap().len(), 4, "{list}");
    assert_eq!(e.blob(MAIN, "GET", "/photos?restype=container", &[], "").await.status, 200);
    assert_eq!(e.blob(READER_ID, "GET", "/photos?restype=container", &[], "").await.status, 200);
    // a reset drops all of it but puts the file's assignment back
    assert_eq!(e.srv.send("POST", &format!("{}/_azemu/reset", e.gw()), &[], "").await.status, 200);
    let list = e.srv.send("GET", &format!("{}/_azemu/auth/assignments", e.gw()), &[], "").await.json();
    assert_eq!(list["value"].as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list["value"][0]["properties"]["principalId"], e.oid(NOROLE).await);
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_config_file_with_a_mistake_stops_the_start_and_names_it() {
    let cfg = scratch("bad").join("auth.json");
    std::fs::write(&cfg, r#"{"assignments":[{"clientId":"x","role":"No such role","scope":"storage/a"}]}"#).unwrap();
    let stack = azemu::full_stack_with(Auth::with(true, Some(cfg.clone())));
    let dir = scratch("baddata");
    let err = TestServer::try_start_persistent(stack.services, &dir, |b| b).await.err().expect("must not start").to_string();
    assert!(err.contains("auth.json") && err.contains("assignments[0]") && err.contains("No such role"), "{err}");
}
