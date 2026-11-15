//! Cosmos DB through `azure_data_cosmos` 0.36.0 with a key, over https. The database and the container are made with the two REST calls a
//! client would send, so the same steps work for a version of the crate that has no call for them; one test uses the crate's own
//! `create_database` and `create_container`, which this version has.

mod common;

use azure_core::credentials::Secret;
use azure_core::http::StatusCode;
use azure_data_cosmos::models::{ContainerProperties, PatchInstructions, PatchOperation};
use azure_data_cosmos::{AccountEndpoint, AccountReference, CosmosClient, FeedScope, Query, RoutingStrategy};
use common::{emu, uniq, COSMOS_KEY};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Doc {
    id: String,
    pk: String,
    n: i64,
}

/// A client for a new account that has a database `app` and a container `items` partitioned on `/pk`.
async fn items() -> azure_data_cosmos::clients::ContainerClient {
    let account = uniq("cos");
    emu().create_cosmos(&account, "app", "items");
    let endpoint: AccountEndpoint = emu().https("documents", &account).parse().unwrap();
    let reference = AccountReference::with_authentication_key(endpoint, Secret::from(COSMOS_KEY.to_string()));
    let client = CosmosClient::builder().build(reference, RoutingStrategy::ProximityTo("East US".into())).await.unwrap();
    client.database_client("app").container_client("items").await.unwrap()
}

fn doc(id: &str, pk: &str, n: i64) -> Doc {
    Doc { id: id.into(), pk: pk.into(), n }
}

#[tokio::test]
async fn upsert_then_read() {
    let items = items().await;
    items.upsert_item("p1", "a", doc("a", "p1", 1), None).await.unwrap();
    items.upsert_item("p1", "a", doc("a", "p1", 2), None).await.unwrap();
    let got: Doc = items.read_item("p1", "a", None).await.unwrap().into_model().unwrap();
    assert_eq!(got, doc("a", "p1", 2));
}

#[tokio::test]
async fn patch_changes_a_field_on_the_server() {
    let items = items().await;
    items.create_item("p1", "a", doc("a", "p1", 1), None).await.unwrap();
    let patch = PatchInstructions::from(vec![PatchOperation::set("/n", json!(7))]);
    items.patch_item("p1", "a", patch, None).await.unwrap();
    let got: Doc = items.read_item("p1", "a", None).await.unwrap().into_model().unwrap();
    assert_eq!(got.n, 7);
}

#[tokio::test]
async fn query_in_one_partition_and_across_partitions() {
    let items = items().await;
    for (id, pk, n) in [("a", "p1", 1), ("b", "p1", 2), ("c", "p2", 3)] {
        items.upsert_item(pk, id, doc(id, pk, n), None).await.unwrap();
    }
    let mut in_one = vec![];
    let mut it = items.query_items::<Doc>(Query::from("SELECT * FROM c ORDER BY c.n"), FeedScope::partition("p1"), None).await.unwrap();
    while let Some(d) = it.try_next().await.unwrap() {
        in_one.push(d.id);
    }
    assert_eq!(in_one, vec!["a", "b"]);
    let query = Query::from("SELECT * FROM c WHERE c.n >= @n").with_parameter("@n", 2).unwrap();
    let mut all = vec![];
    let mut it = items.query_items::<Doc>(query, FeedScope::full_container(), None).await.unwrap();
    while let Some(d) = it.try_next().await.unwrap() {
        all.push(d.id);
    }
    // The crate refuses ORDER BY across partitions itself (400/20113), so the order is not asked for.
    all.sort();
    assert_eq!(all, vec!["b", "c"]);
}

#[tokio::test]
async fn a_missing_item_is_404_and_delete_removes_one() {
    let items = items().await;
    let e = items.read_item("p1", "ghost", None).await.unwrap_err();
    assert_eq!(e.status().status_code(), StatusCode::NotFound);
    items.create_item("p1", "a", doc("a", "p1", 1), None).await.unwrap();
    items.delete_item("p1", "a", None).await.unwrap();
    assert_eq!(items.read_item("p1", "a", None).await.unwrap_err().status().status_code(), StatusCode::NotFound);
}

#[tokio::test]
async fn creating_the_same_item_twice_is_409() {
    let items = items().await;
    items.create_item("p1", "a", doc("a", "p1", 1), None).await.unwrap();
    let e = items.create_item("p1", "a", doc("a", "p1", 1), None).await.unwrap_err();
    assert_eq!(e.status().status_code(), StatusCode::Conflict);
}

#[tokio::test]
async fn the_crates_own_calls_make_a_database_and_a_container() {
    let endpoint: AccountEndpoint = emu().https("documents", &uniq("cos")).parse().unwrap();
    let reference = AccountReference::with_authentication_key(endpoint, Secret::from(COSMOS_KEY.to_string()));
    let client = CosmosClient::builder().build(reference, RoutingStrategy::ProximityTo("East US".into())).await.unwrap();
    client.create_database("shop", None).await.unwrap();
    let db = client.database_client("shop");
    db.create_container(ContainerProperties::new("orders", "/customer".into()), None).await.unwrap();
    let orders = db.container_client("orders").await.unwrap();
    orders.upsert_item("c1", "o1", json!({"id": "o1", "customer": "c1"}), None).await.unwrap();
    let got: serde_json::Value = orders.read_item("c1", "o1", None).await.unwrap().into_model().unwrap();
    assert_eq!(got["customer"], "c1");
    assert_eq!(client.create_database("shop", None).await.unwrap_err().status().status_code(), StatusCode::Conflict);
}
