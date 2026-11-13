//! The code under test. It takes endpoints and a credential, as it would in production; only the tests know about azemu.
use std::sync::Arc;

use azure_core::credentials::{Secret, TokenCredential};
use azure_core::http::{ClientOptions, RequestContent, StatusCode, Url};
use azure_data_cosmos::models::ContainerProperties;
use azure_data_cosmos::options::{ItemWriteOptions, Precondition};
use azure_data_cosmos::{AccountEndpoint, AccountReference, CosmosClient, FeedScope, Query, RoutingStrategy};
use azure_security_keyvault_secrets::{SecretClient, SecretClientOptions, models::SetSecretParameters};
use azure_storage_blob::{BlobServiceClient, BlobServiceClientOptions};
use azure_storage_queue::{QueueServiceClient, QueueServiceClientOptions, models::QueueMessage};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct Config {
    pub blob_url: String,
    pub queue_url: String,
    pub vault_url: String,
    pub cosmos_url: String,
    pub cosmos_key: String,
    pub credential: Arc<dyn TokenCredential>,
    /// Production: `ClientOptions::default()`. Tests: azemu-testkit's options (trust the emulator's CA, no retries).
    pub client_options: ClientOptions,
}

pub async fn blob_round_trip(cfg: &Config, container: &str, name: &str, data: &[u8]) -> Result<Vec<u8>> {
    let opts = BlobServiceClientOptions { client_options: cfg.client_options.clone(), ..Default::default() };
    let svc = BlobServiceClient::new(Url::parse(&cfg.blob_url)?, Some(cfg.credential.clone()), Some(opts))?;
    let c = svc.blob_container_client(container);
    match c.create(None).await {
        Err(e) if e.http_status() != Some(StatusCode::Conflict) => return Err(e.into()),
        _ => {}
    }
    c.blob_client(name).upload(RequestContent::from(data.to_vec()), None).await?;
    Ok(c.blob_client(name).download(None).await?.body.collect().await?.to_vec())
}

pub async fn queue_round_trip(cfg: &Config, queue: &str, text: &str) -> Result<Option<String>> {
    let opts = QueueServiceClientOptions { client_options: cfg.client_options.clone(), ..Default::default() };
    let q = QueueServiceClient::new(Url::parse(&cfg.queue_url)?, Some(cfg.credential.clone()), Some(opts))?.queue_client(queue)?;
    match q.create(None).await {
        Err(e) if e.http_status() != Some(StatusCode::Conflict) => return Err(e.into()),
        _ => {}
    }
    q.send_message(QueueMessage { message_text: Some(text.into()) }.try_into()?, None).await?;
    let got = q.receive_messages(None).await?.into_model()?.items.unwrap_or_default();
    let Some(m) = got.into_iter().next() else { return Ok(None) };
    q.delete_message(m.message_id.as_deref().unwrap_or_default(), m.pop_receipt.as_deref().unwrap_or_default(), None).await?;
    Ok(m.message_text)
}

pub async fn secret_round_trip(cfg: &Config, name: &str, value: &str) -> Result<String> {
    let opts = SecretClientOptions { client_options: cfg.client_options.clone(), ..Default::default() };
    let kv = SecretClient::new(&cfg.vault_url, cfg.credential.clone(), Some(opts))?;
    kv.set_secret(name, SetSecretParameters { value: Some(value.into()), ..Default::default() }.try_into()?, None).await?;
    Ok(kv.get_secret(name, None).await?.into_model()?.value.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Order {
    pub id: String,
    pub customer: String,
    pub total: f64,
}

/// Create the database and container (nothing exists until it is created, as on Azure), write three orders, query them.
/// Cosmos reads its TLS trust only from SSL_CERT_FILE (azemu-testkit sets it).
pub async fn cosmos_orders(cfg: &Config) -> Result<(Vec<Order>, Vec<Order>, bool)> {
    let endpoint: AccountEndpoint = cfg.cosmos_url.parse()?;
    let account = AccountReference::with_authentication_key(endpoint, Secret::from(cfg.cosmos_key.clone()));
    let client = CosmosClient::builder().build(account, RoutingStrategy::ProximityTo("East US".into())).await?;
    client.create_database("shop", None).await?;
    let db = client.database_client("shop");
    db.create_container(ContainerProperties::new("orders", "/customer".into()), None).await?;
    let items = db.container_client("orders", None).await?;
    for (id, customer, total) in [("1", "alice", 10.0), ("2", "alice", 50.0), ("3", "bob", 70.0)] {
        let o = Order { id: id.into(), customer: customer.into(), total };
        // The partition key and id are owned Strings: the SDK wants 'static values.
        items.create_item(o.customer.clone(), &o.id, o.clone(), None).await?;
    }
    let q = Query::from("SELECT * FROM c WHERE c.total > @min").with_parameter("@min", 20.0)?;
    let alice: Vec<Order> = items.query_items(q.clone(), FeedScope::partition("alice".to_string()), None).await?.try_collect().await?;
    let all: Vec<Order> = items.query_items(q, FeedScope::full_container(), None).await?.try_collect().await?;
    // Optimistic concurrency: a write with a stale ETag fails with 412.
    let read = items.read_item("bob".to_string(), "3", None).await?;
    let etag = read.headers().etag().cloned().ok_or("no ETag")?;
    let mut o: Order = read.into_model()?;
    o.total = 71.0;
    items.replace_item(o.customer.clone(), &o.id, o.clone(), None).await?;
    let stale = ItemWriteOptions::default().with_precondition(Precondition::if_match(etag));
    let id = o.id.clone();
    let lost = matches!(items.replace_item(o.customer.clone(), &id, o, Some(stale)).await, Err(e) if e.status().is_precondition_failed());
    Ok((alice, all, lost))
}
