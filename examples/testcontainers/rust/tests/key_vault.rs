//! Key Vault over https with a token credential. The client trusts the CA of this container and no other: the CA is
//! given to this one client as its transport, so no variable and no system store is touched.
mod common;

use std::sync::Arc;

use azure_core::http::{ClientOptions, RetryOptions, Transport};
use azure_identity::ManagedIdentityCredential;
use azure_security_keyvault_secrets::models::SetSecretParameters;
use azure_security_keyvault_secrets::{SecretClient, SecretClientOptions};
use common::{Emulator, GATEWAY, KEY_VAULT};

#[tokio::test]
async fn key_vault_over_https() {
    let emulator = Emulator::start().await;
    let gateway = emulator.port(GATEWAY).await;

    // The credential of a service on Azure; these two variables send it to the emulator's token endpoint.
    // Safety: this test binary has one test, and nothing else reads the environment while it is set.
    unsafe {
        std::env::set_var("IDENTITY_ENDPOINT", format!("http://{}:{gateway}/_azemu/identity/msi/token", emulator.host));
        std::env::set_var("IDENTITY_HEADER", "azemu");
    }
    let credential = ManagedIdentityCredential::new(None).unwrap();

    // Trust this container's CA for this client only.
    let ca = reqwest::Certificate::from_pem(&emulator.ca_pem().await).unwrap();
    let http = reqwest::Client::builder().tls_certs_merge(vec![ca]).build().unwrap();
    let options = SecretClientOptions {
        // A host with one label ("localhost") fails the client's check of the Key Vault challenge: switch it off.
        verify_challenge_resource: Some(false),
        client_options: ClientOptions {
            transport: Some(Transport::new(Arc::new(http))),
            retry: RetryOptions::none(),
            ..Default::default()
        },
        ..Default::default()
    };
    let port = emulator.port(KEY_VAULT).await;
    let secrets = SecretClient::new(&format!("https://{}:{port}", emulator.host), credential, Some(options)).unwrap();

    // "db-password" comes from seed.json.
    let seeded = secrets.get_secret("db-password", None).await.unwrap().into_model().unwrap();
    assert_eq!(seeded.value.as_deref(), Some("local-only-password"));

    let new = SetSecretParameters { value: Some("hunter2".into()), ..Default::default() };
    secrets.set_secret("api-key", new.try_into().unwrap(), None).await.unwrap();
    let got = secrets.get_secret("api-key", None).await.unwrap().into_model().unwrap();
    assert_eq!(got.value.as_deref(), Some("hunter2"));

    // After a reset the secret that a test made is gone and the seeded one is back.
    emulator.reset().await;
    assert!(secrets.get_secret("api-key", None).await.is_err());
    assert!(secrets.get_secret("db-password", None).await.is_ok());
}
