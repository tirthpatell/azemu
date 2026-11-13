//! Cosmos DB with the SDK's own defaults (no environment switches). One binary per default because the SDK reads
//! its environment once per process. See docs/sdk-notes.md.

use azemu_sdk_rust_tests::{emu, uniq};
use azure_core::credentials::Secret;
use azure_data_cosmos::{AccountEndpoint, AccountReference, CosmosClient, RoutingStrategy};

const KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

async fn client() -> azure_data_cosmos::Result<CosmosClient> {
    let endpoint: AccountEndpoint = emu().https("documents", &uniq("cos")).parse().unwrap();
    let account = AccountReference::with_authentication_key(endpoint, Secret::from(KEY.to_string()));
    CosmosClient::builder().build(account, RoutingStrategy::ProximityTo("East US".into())).await
}

#[tokio::test]
async fn default_client_connects_to_a_gateway_that_only_speaks_http_1_1_or_h2() {
    // Azure's Cosmos gateway negotiates h2 over TLS. This SDK offers only "h2" on its first connection.
    let c = client().await.expect("client should connect (SDK default settings)");
    c.create_database(&uniq("db"), None).await.unwrap();
}
