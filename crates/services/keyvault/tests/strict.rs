//! Strict authorisation on Key Vault (`docs/strict-auth.md`, the answers of Azure in November 2026): a vault with role-based access and one
//! with access policies.

use std::sync::Arc;

use azemu_core::auth::{Assignment, Auth, Origin, TestAuthority, VaultAccess, VaultPolicy};
use azemu_core::testing::{TestResponse, TestServer};
use azemu_keyvault::KeyVaultService;

const SECRETS_OFFICER: &str = "b86a8fe4-44ce-4948-aee5-eccb2c155cd7";
const SECRETS_USER: &str = "4633458b-17de-408a-b874-0445c86b69e6";
const ADMIN: &str = "00482a5a-887f-4fb3-b363-3b7fe8e74483";
const VAULT_ID: &str = "/subscriptions/s1/resourceGroups/Grp/providers/Microsoft.KeyVault/vaults/rbacvault";

fn give(auth: &Auth, principal: &str, role: &str, scope: &str) {
    auth.registry()
        .put_assignment(Assignment {
            id: azemu_core::auth::config::derived_guid(&[principal, role, scope]),
            scope: scope.into(),
            role_id: role.into(),
            principal_id: principal.into(),
            principal_type: "ServicePrincipal".into(),
            condition: None,
            condition_version: None,
            description: None,
            created_on: String::new(),
            updated_on: String::new(),
            origin: Origin::Api,
        })
        .unwrap();
}

async fn start() -> (TestServer, Auth) {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.KeyVault/vaults", "rbacvault", VAULT_ID);
    let kv = KeyVaultService::new();
    kv.set_auth(auth.clone());
    give(&auth, "main", ADMIN, VAULT_ID);
    give(&auth, "reader", SECRETS_USER, VAULT_ID);
    give(&auth, "scoped", SECRETS_USER, &format!("{VAULT_ID}/secrets/mine"));
    // a vault with access policies
    auth.registry().set_vault_access(
        "policyvault",
        VaultAccess::Policies(
            vec![
                VaultPolicy {
                    object_id: "main".into(),
                    secrets: vec!["get".into(), "list".into(), "set".into(), "delete".into(), "purge".into(), "recover".into()],
                    ..Default::default()
                },
                VaultPolicy { object_id: "reader".into(), secrets: vec!["get".into(), "list".into()], ..Default::default() },
            ],
            Origin::Api,
        ),
    );
    (TestServer::start(vec![Arc::new(kv)]).await, auth)
}

async fn call(srv: &TestServer, vault: &str, who: &str, method: &str, path: &str, body: &str) -> TestResponse {
    let t = format!("Bearer {}", TestAuthority::token(who, "https://vault.azure.net"));
    let sep = if path.contains('?') { '&' } else { '?' };
    srv.send(
        method,
        &format!("{}{path}{sep}api-version=7.5", srv.url("vault", vault)),
        &[("authorization", &t), ("content-type", "application/json")],
        body.to_string(),
    )
    .await
}

const SET: &str = r#"{"value":"v"}"#;

#[tokio::test]
async fn a_role_based_vault_names_the_action_and_the_resource() {
    let (srv, _) = start().await;
    assert_eq!(call(&srv, "rbacvault", "main", "PUT", "/secrets/mine", SET).await.status, 200);
    assert_eq!(call(&srv, "rbacvault", "main", "PUT", "/secrets/other", SET).await.status, 200);
    // no role: the same answer for a missing secret as for an existing one
    for name in ["mine", "nothere"] {
        let r = call(&srv, "rbacvault", "norole", "GET", &format!("/secrets/{name}"), "").await;
        assert_eq!(r.status, 403);
        let v = r.json();
        assert_eq!(v["error"]["code"], "Forbidden");
        assert_eq!(v["error"]["innererror"]["code"], "ForbiddenByRbac");
        let m = v["error"]["message"].as_str().unwrap();
        assert!(m.starts_with("Caller is not authorized to perform action on resource.\r\nIf role assignments, deny assignments or role definitions were changed recently, please observe propagation time.\r\nCaller: appid=client-norole;oid=norole;iss=https://sts.windows.net/t-0/\r\n"), "{m}");
        assert!(m.contains(&format!("Action: 'Microsoft.KeyVault/vaults/secrets/getSecret/action'\r\nResource: '/subscriptions/s1/resourcegroups/grp/providers/microsoft.keyvault/vaults/rbacvault/secrets/{name}'\r\nAssignment: (not found)\r\nDenyAssignmentId: null\r\nDecisionReason: null \r\nVault: rbacvault;location=westus\r\n")), "{m}");
    }
    // a list names the vault
    let r = call(&srv, "rbacvault", "norole", "GET", "/secrets", "").await;
    let m = r.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(m.contains("Action: 'Microsoft.KeyVault/vaults/secrets/readMetadata/action'\r\nResource: '/subscriptions/s1/resourcegroups/grp/providers/microsoft.keyvault/vaults/rbacvault'\r\n"), "{m}");
    // delete has no `/action`, a key read has none either
    let m = call(&srv, "rbacvault", "norole", "DELETE", "/secrets/mine", "").await.text();
    assert!(m.contains("Action: 'Microsoft.KeyVault/vaults/secrets/delete'"), "{m}");
    let m = call(&srv, "rbacvault", "norole", "GET", "/keys/k", "").await.text();
    assert!(m.contains("Action: 'Microsoft.KeyVault/vaults/keys/read'"), "{m}");
    // a Secrets User reads, lists, reads deleted ones, and nothing else
    assert_eq!(call(&srv, "rbacvault", "reader", "GET", "/secrets/mine", "").await.status, 200);
    assert_eq!(call(&srv, "rbacvault", "reader", "GET", "/secrets", "").await.status, 200);
    assert_eq!(call(&srv, "rbacvault", "reader", "GET", "/secrets/mine/versions", "").await.status, 200);
    let r = call(&srv, "rbacvault", "reader", "GET", "/secrets/nothere", "").await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str().unwrap().to_string()), (404, "SecretNotFound".to_string()));
    assert_eq!(call(&srv, "rbacvault", "main", "DELETE", "/secrets/other", "").await.status, 200);
    assert_eq!(call(&srv, "rbacvault", "reader", "GET", "/deletedsecrets/other", "").await.status, 200);
    assert_eq!(call(&srv, "rbacvault", "reader", "GET", "/deletedsecrets", "").await.status, 200);
    for (m, p, b) in [
        ("PUT", "/secrets/mine", SET),
        ("PATCH", "/secrets/mine", "{}"),
        ("DELETE", "/secrets/mine", ""),
        ("POST", "/deletedsecrets/other/recover", ""),
        ("DELETE", "/deletedsecrets/other", ""),
        ("POST", "/secrets/mine/backup", ""),
        ("POST", "/secrets/restore", "{}"),
        ("GET", "/keys", ""),
        ("GET", "/certificates", ""),
    ] {
        let r = call(&srv, "rbacvault", "reader", m, p, b).await;
        assert_eq!(r.status, 403, "{m} {p}: {}", r.text());
    }
    // a role on one secret
    assert_eq!(call(&srv, "rbacvault", "scoped", "GET", "/secrets/mine", "").await.status, 200);
    assert_eq!(call(&srv, "rbacvault", "scoped", "GET", "/secrets/mine/versions", "").await.status, 200);
    assert_eq!(call(&srv, "rbacvault", "scoped", "GET", "/secrets/other", "").await.status, 403);
    assert_eq!(call(&srv, "rbacvault", "scoped", "GET", "/secrets", "").await.status, 403);
    let m = call(&srv, "rbacvault", "scoped", "GET", "/secrets/other/versions", "").await.text();
    assert!(m.contains("readMetadata/action") && m.contains("/secrets/other"), "{m}");
    // an Officer sets
    let (srv2, auth2) = start().await;
    give(&auth2, "officer", SECRETS_OFFICER, VAULT_ID);
    assert_eq!(call(&srv2, "rbacvault", "officer", "PUT", "/secrets/x1", SET).await.status, 200);
    assert_eq!(call(&srv2, "rbacvault", "officer", "POST", "/keys/k/create", "{\"kty\":\"RSA\"}").await.status, 403);
}

#[tokio::test]
async fn an_access_policy_vault_tells_a_missing_entry_from_a_missing_permission() {
    let (srv, _) = start().await;
    assert_eq!(call(&srv, "policyvault", "main", "PUT", "/secrets/s1", SET).await.status, 200);
    assert_eq!(call(&srv, "policyvault", "reader", "GET", "/secrets/s1", "").await.status, 200);
    assert_eq!(call(&srv, "policyvault", "reader", "GET", "/secrets", "").await.status, 200);
    // an entry without the permission
    let r = call(&srv, "policyvault", "reader", "PUT", "/secrets/s2", SET).await;
    assert_eq!(r.status, 403);
    let v = r.json();
    assert_eq!(v["error"]["innererror"]["code"], "ForbiddenByPolicy");
    assert_eq!(
        v["error"]["message"],
        "The user, group or application 'appid=client-reader;oid=reader;iss=https://sts.windows.net/t-0/' does not have secrets set permission on key vault 'policyvault;location=westus'. For help resolving this issue, please see https://go.microsoft.com/fwlink/?linkid=2125287"
    );
    // no entry at all
    let r = call(&srv, "policyvault", "norole", "GET", "/secrets/s1", "").await;
    assert_eq!(r.json()["error"]["innererror"]["code"], "AccessDenied");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("does not have secrets get permission"));
    // update needs `set`
    let r = call(&srv, "policyvault", "reader", "PATCH", "/secrets/s1", "{}").await;
    assert!(r.text().contains("secrets set permission"), "{}", r.text());
    // keys: main has no key permission here
    let r = call(&srv, "policyvault", "main", "GET", "/keys", "").await;
    assert!(
        r.text().contains("keys list permission") && r.text().contains("ForbiddenByPolicy") || r.text().contains("AccessDenied"),
        "{}",
        r.text()
    );
}

#[tokio::test]
async fn token_problems_are_401_with_the_challenge() {
    let (srv, _) = start().await;
    let url = format!("{}/secrets/s1?api-version=7.5", srv.url("vault", "rbacvault"));
    let check = |t: String, want: String| {
        let srv = &srv;
        let url = url.clone();
        async move {
            let r = srv.send("GET", &url, &[("authorization", &format!("Bearer {t}"))], "").await;
            assert_eq!(r.status, 401, "{}", r.text());
            assert!(r.header("www-authenticate").unwrap().starts_with(
                "Bearer authorization=\"https://login.microsoftonline.com/00000000-0000-0000-0000-000000000000\", resource=\""
            ));
            assert_eq!(r.json()["error"]["code"], "Unauthorized");
            assert_eq!(r.json()["error"]["message"], want);
        }
    };
    check(
        TestAuthority::token("main", "https://storage.azure.com"),
        "AKV10022: Invalid audience. Expected cfa8b339-82a2-471a-a3c9-0fc0be7a4093, found: https://storage.azure.com.".into(),
    )
    .await;
    check(format!("{}x", TestAuthority::token("main", "https://vault.azure.net")), "[Unknown] Error validating token: 'S2S12086'.".into())
        .await;
    check("garbage".into(), "[MalformedToken] Error validating token: 'S2S12086'.".into()).await;
    // the vault's own host is an audience
    let t = TestAuthority::token("main", "https://rbacvault.vault.azure.net");
    let r = srv.send("GET", &url, &[("authorization", &format!("Bearer {t}"))], "").await;
    assert_eq!(r.status, 404);
}
