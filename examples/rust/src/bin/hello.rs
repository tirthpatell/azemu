//! Against an emulator you started yourself:
//!   azemu --cert-dir ~/.azemu            # prints: azemu ready gateway=http://127.0.0.1:4577 ca=...
//!   cat /etc/ssl/certs/ca-certificates.crt ~/.azemu/ca.pem > ~/.azemu/bundle.pem
//!   SSL_CERT_FILE=~/.azemu/bundle.pem AZURE_COSMOS_BINARY_ENCODING_ENABLED=false COSMOS_DISABLE_IMDS=1 \
//!   IDENTITY_ENDPOINT=http://localhost:4577/_azemu/identity/msi/token IDENTITY_HEADER=azemu cargo run --bin hello
use azemu_rust_example::*;

#[tokio::main]
async fn main() -> Result<()> {
    let port = std::env::var("AZEMU_PORT").unwrap_or_else(|_| "4577".into());
    let cfg = Config {
        blob_url: format!("https://myacct.blob.core.localhost:{port}/"),
        queue_url: format!("https://myacct.queue.core.localhost:{port}/"),
        vault_url: format!("https://myvault.vault.localhost:{port}/"),
        cosmos_url: format!("https://myacct.documents.localhost:{port}/"),
        cosmos_key: "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==".into(),
        // The same credential type an App Service or Container App uses; IDENTITY_ENDPOINT points it at azemu.
        credential: azure_identity::ManagedIdentityCredential::new(None)?,
        client_options: Default::default(),
    };
    println!("blob:   {:?}", String::from_utf8(blob_round_trip(&cfg, "demo", "hello.txt", b"hello").await?)?);
    println!("queue:  {:?}", queue_round_trip(&cfg, "jobs", "hello").await?);
    println!("secret: {:?}", secret_round_trip(&cfg, "greeting", "hello").await?);
    let (alice, all, lost) = cosmos_orders(&cfg).await?;
    println!("cosmos: alice>20 {:?}; all>20 {}; stale write refused {lost}", alice.iter().map(|o| &o.id).collect::<Vec<_>>(), all.len());
    Ok(())
}
