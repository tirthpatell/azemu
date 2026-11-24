//! Strict authorisation (`docs/strict-auth.md`): the emulator is started with `AZEMU_STRICT_AUTH=1`, and a principal with no role is refused while one
//! with the role is accepted, for Blob and Key Vault. What a Rust program sees is an `azure_core::Error` with the HTTP status and the error code.

use azemu_sdk_rust_tests::{client_secret_cred, emu, http_call, uniq, Emu};
use azure_core::http::{RequestContent, StatusCode, Url};
use azure_core::Error;
use azure_security_keyvault_secrets::models::SetSecretParameters;
use azure_security_keyvault_secrets::SecretClient;
use azure_storage_blob::BlobServiceClient;

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const WITH_ROLE: &str = "aaaaaaaa-0000-4000-8000-000000000021";
const NO_ROLE: &str = "aaaaaaaa-0000-4000-8000-000000000022";

/// The emulator of this test binary, started with the mode on (the variable is set before the first call starts it).
fn strict() -> &'static Emu {
    std::env::set_var("AZEMU_STRICT_AUTH", "1");
    emu()
}

fn give(client: &str, role: &str, scope: &str) {
    let body = serde_json::json!({"clientId": client, "role": role, "scope": scope}).to_string();
    let (status, text) = http_call("PUT", "/_azemu/auth/assignments", &body);
    assert!(status == 200 || status == 201, "give {role} at {scope}: {status} {text}");
}

fn code(e: &Error) -> Option<String> {
    match e.kind() {
        azure_core::error::ErrorKind::HttpResponse { error_code, .. } => error_code.clone(),
        _ => None,
    }
}

#[tokio::test]
async fn blob_refuses_a_principal_without_the_role_and_accepts_one_with_it() {
    let e = strict();
    let account = uniq("rsauth");
    give(WITH_ROLE, "Storage Blob Data Contributor", &format!("storage/{account}"));
    let url = Url::parse(&e.https("blob", &account)).unwrap();
    let good = BlobServiceClient::new(url.clone(), Some(client_secret_cred(TENANT, WITH_ROLE, "x")), None).unwrap();
    let bad = BlobServiceClient::new(url, Some(client_secret_cred(TENANT, NO_ROLE, "x")), None).unwrap();

    let c = good.blob_container_client("data");
    c.create(None).await.expect("with the role");
    let b = c.blob_client("hello");
    b.upload(RequestContent::from(b"hi".to_vec()), None).await.expect("with the role");
    assert_eq!(b.download(None).await.unwrap().body.collect().await.unwrap().as_ref(), b"hi");

    // without the role: 403 and the code of Azure
    let err = bad.blob_container_client("data").blob_client("hello").download(None).await.err().expect("refused");
    assert_eq!(err.http_status(), Some(StatusCode::Forbidden), "{err}");
    assert_eq!(code(&err).as_deref(), Some("AuthorizationPermissionMismatch"), "{err}");
    let err = bad.blob_container_client("other").create(None).await.err().expect("refused");
    assert_eq!(err.http_status(), Some(StatusCode::Forbidden));
    assert_eq!(code(&err).as_deref(), Some("AuthorizationFailure"));
    // what a missing role did not change
    assert_eq!(b.download(None).await.unwrap().body.collect().await.unwrap().as_ref(), b"hi");
}

#[tokio::test]
async fn key_vault_is_forbidden_by_rbac_without_the_role() {
    let e = strict();
    let vault_name = uniq("rskv");
    give(WITH_ROLE, "Key Vault Secrets Officer", &format!("keyvault/{vault_name}"));
    let vault = format!("https://{vault_name}.vault.localhost:{}", e.port);
    let good = SecretClient::new(&vault, client_secret_cred(TENANT, WITH_ROLE, "x"), None).unwrap();
    let bad = SecretClient::new(&vault, client_secret_cred(TENANT, NO_ROLE, "x"), None).unwrap();
    let params = SetSecretParameters { value: Some("s3cret".into()), ..Default::default() };
    good.set_secret("db-password", RequestContent::try_from(params).unwrap(), None).await.expect("with the role");
    let got = good.get_secret("db-password", None).await.unwrap().into_model().unwrap();
    assert_eq!(got.value.as_deref(), Some("s3cret"));

    let err = bad.get_secret("db-password", None).await.err().expect("refused");
    assert_eq!(err.http_status(), Some(StatusCode::Forbidden), "{err}");
    // the Rust Key Vault crate puts the HTTP status in `error_code`; the body names `Forbidden` and `ForbiddenByRbac`
    let text = err.to_string();
    assert!(text.contains("\"code\":\"Forbidden\"") && text.contains("ForbiddenByRbac"), "{text}");
    assert!(text.contains("Caller is not authorized to perform action on resource."), "{text}");
    assert!(text.contains("Microsoft.KeyVault/vaults/secrets/getSecret/action"), "{text}");
    // a role that reads does not write
    give(NO_ROLE, "Key Vault Secrets User", &format!("keyvault/{vault_name}"));
    assert!(bad.get_secret("db-password", None).await.is_ok());
    let params = SetSecretParameters { value: Some("x".into()), ..Default::default() };
    let err = bad.set_secret("other", RequestContent::try_from(params).unwrap(), None).await.err().expect("refused");
    assert_eq!(err.http_status(), Some(StatusCode::Forbidden));
}

/// Event Hubs: the Rust SDK takes a token credential only, so this is the Entra side (shared access keys are the Go and Python tests). A principal
/// with no role passes `put-token` and is refused at the link attach; what a Rust program sees is an error whose text carries Azure's
/// the condition `UnauthorizedAccess` and the missing claim. A principal with `Azure Event Hubs Data Sender` sends.
#[tokio::test]
async fn event_hubs_refuses_a_send_without_the_role_and_accepts_one_with_it() {
    use azure_messaging_eventhubs::ProducerClient;
    let e = strict();
    let ns = uniq("rseh");
    let hub = uniq("hub");
    azemu_sdk_rust_tests::create_eventhub(&ns, &hub);
    give(WITH_ROLE, "Azure Event Hubs Data Sender", &format!("eventhubs/{ns}/eventhubs/{hub}"));
    let host = format!("{ns}.eventhub.localhost:{}", e.port);

    let good = ProducerClient::builder().open(&host, &hub, client_secret_cred(TENANT, WITH_ROLE, "x")).await.expect("open with the role");
    let event = azure_messaging_eventhubs::models::EventData::builder().with_body(b"hello".to_vec()).build();
    good.send_event(event, None).await.expect("a send with the role");

    // no role: the open does the token exchange (accepted), the send attaches a sender link and that is refused
    let bad = ProducerClient::builder().open(&host, &hub, client_secret_cred(TENANT, NO_ROLE, "x")).await;
    let err = match bad {
        Err(err) => err,
        Ok(p) => {
            let event = azure_messaging_eventhubs::models::EventData::builder().with_body(b"hello".to_vec()).build();
            let r = p.send_event(event, None).await;
            r.err().expect("a send without the role is refused")
        }
    };
    let text = format!("{err:?}");
    assert!(text.contains("UnauthorizedAccess") && text.contains("'Send' claim(s) are required"), "{text}");
}
