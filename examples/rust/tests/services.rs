//! `cargo test`: azemu-testkit starts one emulator for this test binary (`azemu` on `PATH`, or `AZEMU_BIN`). Each test uses
//! its own account names, so the tests run in parallel, and each test empties its account at the end.
use azemu_rust_example::*;
use azemu_testkit::{COSMOS_EMULATOR_KEY, Service, shared, unique_name};
use azure_core::http::{ClientOptions, RetryOptions};
use azure_identity::ManagedIdentityCredential;

fn config(account: &str) -> Config {
    // The first call starts the emulator and sets SSL_CERT_FILE, IDENTITY_ENDPOINT and the Cosmos variables; do it
    // before building any Azure client.
    let emu = shared();
    Config {
        blob_url: emu.endpoint(Service::Blob, account),
        queue_url: emu.endpoint(Service::Queue, account),
        vault_url: emu.endpoint(Service::KeyVault, account),
        cosmos_url: emu.endpoint(Service::Cosmos, account),
        cosmos_key: COSMOS_EMULATOR_KEY.into(),
        // The credential type a service uses on Azure; the testkit's IDENTITY_ENDPOINT points it at the emulator.
        credential: ManagedIdentityCredential::new(None).unwrap(),
        // No retries: a mistake in the set-up fails in milliseconds, not after the SDK's default retries.
        client_options: ClientOptions { retry: RetryOptions::none(), ..Default::default() },
    }
}

#[tokio::test]
async fn blob() {
    let account = unique_name("t");
    assert_eq!(blob_round_trip(&config(&account), "box", "a.txt", b"hi").await.unwrap(), b"hi");
    shared().reset_account(&account);
}

#[tokio::test]
async fn queue() {
    let account = unique_name("t");
    assert_eq!(queue_round_trip(&config(&account), "jobs", "job-1").await.unwrap().as_deref(), Some("job-1"));
    shared().reset_account(&account);
}

#[tokio::test]
async fn key_vault() {
    let account = unique_name("t");
    assert_eq!(secret_round_trip(&config(&account), "db-password", "hunter2").await.unwrap(), "hunter2");
    shared().reset_account(&account);
}

#[tokio::test]
async fn cosmos() {
    let account = unique_name("t");
    let (alice, all, lost) = cosmos_orders(&config(&account)).await.unwrap();
    assert_eq!(alice.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), ["2"]);
    assert_eq!(all.len(), 2);
    assert!(lost, "a write with a stale ETag must fail with 412");
    shared().reset_account(&account);
}
