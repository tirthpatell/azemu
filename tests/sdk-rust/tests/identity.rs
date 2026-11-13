//! Identity: `ManagedIdentityCredential` (App Service style, `IDENTITY_ENDPOINT` + `IDENTITY_HEADER`),
//! `ClientSecretCredential` against the emulator's login endpoint, and the token used for Blob and Key Vault.
//!
//! The Rust `azure_identity` crate has no `DefaultAzureCredential` (it was replaced by
//! `DeveloperToolsCredential`, which only runs the `az` / `azd` command line tools), so managed
//! identity is the "no code change" path.

use azemu_sdk_rust_tests::{client_secret_cred, emu, jwt_claims, uniq};
use azure_core::credentials::{Secret, TokenCredential};
use azure_core::http::{RequestContent, Url};
use azure_identity::{ManagedIdentityCredential, ManagedIdentityCredentialOptions, UserAssignedId};
use azure_security_keyvault_secrets::models::SetSecretParameters;
use azure_security_keyvault_secrets::{SecretClient, SecretClientOptions};
use azure_storage_blob::BlobServiceClient;

const TENANT: &str = "72f988bf-86f1-41af-91ab-2d7cd011db47";

#[tokio::test]
async fn managed_identity_token_comes_from_the_emulator_for_the_requested_resource() {
    emu();
    let cred = ManagedIdentityCredential::new(None).unwrap();
    let t = cred.get_token(&["https://storage.azure.com/.default"], None).await.unwrap();
    let claims = jwt_claims(t.token.secret());
    assert_eq!(claims["aud"], "https://storage.azure.com");
    assert!(claims["iss"].as_str().unwrap().starts_with("https://sts.windows.net/"));
    let left = (t.expires_on - azure_core::time::OffsetDateTime::now_utc()).whole_minutes();
    assert!((50..=61).contains(&left), "token lifetime should be about 1 hour, got {left} minutes");
}

#[tokio::test]
async fn managed_identity_with_user_assigned_client_id_gets_a_token_for_that_client() {
    emu();
    let client_id = "11111111-2222-3333-4444-555555555555".to_string();
    let opts =
        ManagedIdentityCredentialOptions { user_assigned_id: Some(UserAssignedId::ClientId(client_id.clone())), ..Default::default() };
    let cred = ManagedIdentityCredential::new(Some(opts)).unwrap();
    let t = cred.get_token(&["https://vault.azure.net/.default"], None).await.unwrap();
    let claims = jwt_claims(t.token.secret());
    assert_eq!(claims["appid"], client_id.as_str());
    assert_eq!(claims["aud"], "https://vault.azure.net");
}

#[tokio::test]
async fn client_secret_credential_gets_a_token_from_the_login_endpoint() {
    emu();
    let client = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let cred = client_secret_cred(TENANT, client, "any-secret");
    let t = cred.get_token(&["https://management.azure.com/.default"], None).await.unwrap();
    let claims = jwt_claims(t.token.secret());
    assert_eq!(claims["appid"], client);
    assert_eq!(claims["tid"], TENANT);
    assert_eq!(claims["aud"], "https://management.azure.com");
}

#[tokio::test]
async fn client_secret_credential_caches_the_token_between_calls() {
    emu();
    let cred = client_secret_cred(TENANT, "cache-client", "s");
    let a = cred.get_token(&["https://storage.azure.com/.default"], None).await.unwrap();
    let b = cred.get_token(&["https://storage.azure.com/.default"], None).await.unwrap();
    assert_eq!(a.token.secret(), b.token.secret());
}

#[tokio::test]
async fn client_secret_credential_accepts_a_tenant_domain_name() {
    emu();
    let cred = client_secret_cred("contoso.onmicrosoft.com", "client-x", "s");
    let t = cred.get_token(&["https://storage.azure.com/.default"], None).await.unwrap();
    assert!(!jwt_claims(t.token.secret())["tid"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn managed_identity_token_authorizes_a_blob_call() {
    emu();
    let cred: std::sync::Arc<dyn TokenCredential> = ManagedIdentityCredential::new(None).unwrap();
    let url = Url::parse(&emu().https("blob", &uniq("mi"))).unwrap();
    let svc = BlobServiceClient::new(url, Some(cred), None).unwrap();
    let c = svc.blob_container_client("idc");
    c.create(None).await.unwrap();
    let b = c.blob_client("hello");
    b.upload(RequestContent::from(b"via managed identity".to_vec()), None).await.unwrap();
    assert_eq!(b.download(None).await.unwrap().body.collect().await.unwrap().as_ref(), b"via managed identity");
}

#[tokio::test]
async fn client_secret_token_authorizes_a_key_vault_call() {
    emu();
    let cred = client_secret_cred(TENANT, "kv-client", "s");
    let vault = format!("https://{}.vault.localhost:{}", uniq("idv"), emu().port);
    let client = SecretClient::new(&vault, cred, None).unwrap();
    let params = SetSecretParameters { value: Some("s3cret".into()), ..Default::default() };
    client.set_secret("db-password", RequestContent::try_from(params).unwrap(), None).await.unwrap();
    let got = client.get_secret("db-password", None).await.unwrap().into_model().unwrap();
    assert_eq!(got.value.as_deref(), Some("s3cret"));
}

#[tokio::test]
async fn managed_identity_token_authorizes_a_key_vault_call_with_resource_challenge_checked() {
    // Azure: the vault answers 401 with resource="https://vault.azure.net"; the SDK refuses a challenge whose resource does not
    // match the vault host unless told not to. The emulator's challenge names the domain of the request (vault.localhost:<port>),
    // so the default check passes, and the old switch still works.
    emu();
    let cred: std::sync::Arc<dyn TokenCredential> = ManagedIdentityCredential::new(None).unwrap();
    let vault = format!("https://{}.vault.localhost:{}", uniq("idv"), emu().port);
    let client = SecretClient::new(&vault, cred.clone(), None).unwrap();
    let e = client.get_secret("anything", None).await.unwrap_err();
    assert_eq!(e.http_status(), Some(azure_core::http::StatusCode::NotFound), "with the default check on, the call reaches the vault: {e}");
    let opts = SecretClientOptions { verify_challenge_resource: Some(false), ..Default::default() };
    let client = SecretClient::new(&vault, cred, Some(opts)).unwrap();
    let e = client.get_secret("anything", None).await.unwrap_err();
    assert_eq!(e.http_status(), Some(azure_core::http::StatusCode::NotFound));
}

#[allow(dead_code)]
fn _unused(_: Secret) {}
