//! Cosmos DB with the SDK's own defaults (no environment switches). One binary per default because the SDK reads
//! its environment once per process. See docs/sdk-notes.md.

use azemu_sdk_rust_tests::{emu, uniq};
use azure_core::credentials::Secret;
use azure_data_cosmos::models::ContainerProperties;
use azure_data_cosmos::{AccountEndpoint, AccountReference, CosmosClient, RoutingStrategy};

const KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

async fn client() -> azure_data_cosmos::Result<CosmosClient> {
    let endpoint: AccountEndpoint = emu().https("documents", &uniq("cos")).parse().unwrap();
    let account = AccountReference::with_authentication_key(endpoint, Secret::from(KEY.to_string()));
    CosmosClient::builder().build(account, RoutingStrategy::ProximityTo("East US".into())).await
}

#[tokio::test]
#[ignore = "azemu: item writes sent as Cosmos binary JSON are refused with 400 'The request payload is not valid JSON' (see docs/sdk-notes.md)"]
async fn item_create_works_with_the_sdk_default_binary_encoding() {
    // binary encoding stays at the SDK default (on).
    let c = client().await.unwrap();
    let db_name = uniq("db");
    c.create_database(&db_name, None).await.unwrap();
    let db = c.database_client(&db_name);
    db.create_container(ContainerProperties::new("items", "/pk".into()), None).await.unwrap();
    let items = db.container_client("items", None).await.unwrap();
    items.create_item("p1", "i1", serde_json::json!({"id":"i1","pk":"p1","v":1}), None).await.unwrap();
    let v: serde_json::Value = items.read_item("p1", "i1", None).await.unwrap().into_model().unwrap();
    assert_eq!(v["v"], 1);
}
