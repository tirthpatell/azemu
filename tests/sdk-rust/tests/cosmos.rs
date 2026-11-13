//! Cosmos DB (NoSQL API) through `azure_data_cosmos` 1.1.0-beta.2 with the `key_auth`,
//! `control_plane` and `unstable_patch` features. HTTPS only.

use azemu_sdk_rust_tests::{emu, uniq};
use azure_core::credentials::Secret;
use azure_core::http::{Etag, StatusCode};
use azure_data_cosmos::clients::{ContainerClient, DatabaseClient};
use azure_data_cosmos::models::{
    ContainerProperties, CosmosNumber, PartitionKeyDefinition, PatchInstructions, PatchOperation, ThroughputProperties, TransactionalBatch,
};
use azure_data_cosmos::options::{
    CreateContainerOptions, ItemWriteOptions, MaxItemCountHint, PatchItemOptions, PatchStrategy, Precondition, QueryOptions,
};
use azure_data_cosmos::{AccountEndpoint, AccountReference, CosmosClient, RoutingStrategy};
use azure_data_cosmos::{FeedScope, Query};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::num::{NonZeroU32, NonZeroU8};

const KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

async fn client() -> CosmosClient {
    let e = emu();
    // Binary encoding is on by default and the emulator only reads JSON (see docs/sdk-notes.md); the SDK's README
    // documents this switch for the Linux vNext emulator too.
    std::env::set_var("AZURE_COSMOS_BINARY_ENCODING_ENABLED", "false");
    let endpoint: AccountEndpoint = e.https("documents", &uniq("cos")).parse().unwrap();
    let account = AccountReference::with_authentication_key(endpoint, Secret::from(KEY.to_string()));
    CosmosClient::builder().build(account, RoutingStrategy::ProximityTo("East US".into())).await.unwrap()
}

/// A fresh account, database and a container partitioned on `/pk`.
async fn container() -> (CosmosClient, DatabaseClient, ContainerClient) {
    let c = client().await;
    let db_name = uniq("db");
    c.create_database(&db_name, None).await.unwrap();
    let db = c.database_client(&db_name);
    db.create_container(ContainerProperties::new("items", "/pk".into()), None).await.unwrap();
    let items = db.container_client("items", None).await.unwrap();
    (c, db, items)
}

fn status(e: &azure_data_cosmos::CosmosError) -> StatusCode {
    e.status().status_code()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Doc {
    id: String,
    pk: String,
    n: i64,
    #[serde(default)]
    tags: Vec<String>,
}

fn doc(id: &str, pk: &str, n: i64) -> Doc {
    Doc { id: id.into(), pk: pk.into(), n, tags: vec![] }
}

async fn collect<T: serde::de::DeserializeOwned + Send + 'static>(
    items: &ContainerClient,
    q: Query,
    scope: FeedScope,
    opts: Option<QueryOptions>,
) -> Vec<T> {
    let mut it = items.query_items::<T>(q, scope, opts).await.unwrap();
    let mut out = vec![];
    while let Some(x) = it.try_next().await.unwrap() {
        out.push(x);
    }
    out
}

// ---- databases and containers --------------------------------------------------------------

#[tokio::test]
async fn create_database_returns_its_id_and_a_resource_id() {
    let c = client().await;
    let name = uniq("db");
    let r = c.create_database(&name, None).await.unwrap();
    let p = r.into_model().unwrap();
    assert_eq!(p.id.as_deref(), Some(name.as_str()));
}

#[tokio::test]
async fn create_existing_database_is_409_conflict() {
    let c = client().await;
    let name = uniq("db");
    c.create_database(&name, None).await.unwrap();
    let e = c.create_database(&name, None).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::Conflict);
}

#[tokio::test]
async fn query_databases_lists_created_databases() {
    let c = client().await;
    let a = uniq("dba");
    let b = uniq("dbb");
    c.create_database(&a, None).await.unwrap();
    c.create_database(&b, None).await.unwrap();
    let mut it = c.query_databases("SELECT * FROM d", None).await.unwrap();
    let mut ids = vec![];
    while let Some(d) = it.try_next().await.unwrap() {
        ids.push(d.id.clone().unwrap());
    }
    ids.sort();
    let mut want = vec![a, b];
    want.sort();
    assert_eq!(ids, want);
}

#[tokio::test]
async fn delete_database_then_container_client_is_404() {
    let c = client().await;
    let name = uniq("db");
    c.create_database(&name, None).await.unwrap();
    let db = c.database_client(&name);
    db.delete(None).await.unwrap();
    let e = db.container_client("nope", None).await.err().expect("must fail");
    assert_eq!(status(&e), StatusCode::NotFound);
}

#[tokio::test]
async fn create_container_returns_partition_key_path_and_default_indexing_policy() {
    let c = client().await;
    let name = uniq("db");
    c.create_database(&name, None).await.unwrap();
    let db = c.database_client(&name);
    let r = db.create_container(ContainerProperties::new("c1", "/tenant".into()), None).await.unwrap().into_model().unwrap();
    assert_eq!(r.id, "c1");
    assert_eq!(r.partition_key.paths().len(), 1);
    assert_eq!(r.partition_key.paths()[0].to_string(), "/tenant");
    assert!(r.indexing_policy.is_some());
}

#[tokio::test]
async fn create_existing_container_is_409_conflict() {
    let (_c, db, _items) = container().await;
    let e = db.create_container(ContainerProperties::new("items", "/pk".into()), None).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::Conflict);
}

#[tokio::test]
async fn query_containers_lists_containers_of_the_database() {
    let (_c, db, _items) = container().await;
    db.create_container(ContainerProperties::new("second", "/pk".into()), None).await.unwrap();
    let mut it = db.query_containers("SELECT * FROM c", None).await.unwrap();
    let mut ids = vec![];
    while let Some(x) = it.try_next().await.unwrap() {
        ids.push(x.id.to_string());
    }
    ids.sort();
    assert_eq!(ids, vec!["items", "second"]);
}

#[tokio::test]
async fn container_read_returns_the_container_properties() {
    let (_c, _db, items) = container().await;
    let p = items.read(None).await.unwrap().into_model().unwrap();
    assert_eq!(p.id, "items");
}

#[tokio::test]
async fn delete_container_then_reading_an_item_in_it_is_404() {
    let (_c, db, items) = container().await;
    items.create_item("p1", "a", doc("a", "p1", 1), None).await.unwrap();
    items.delete(None).await.unwrap();
    let e = items.read_item("p1", "a", None).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::NotFound);
    let _ = db;
}

#[tokio::test]
async fn hierarchical_partition_key_container_is_created_with_two_paths() {
    let c = client().await;
    let name = uniq("db");
    c.create_database(&name, None).await.unwrap();
    let db = c.database_client(&name);
    let pk = PartitionKeyDefinition::from(("/tenant", "/user"));
    let r = db.create_container(ContainerProperties::new("hier", pk), None).await.unwrap().into_model().unwrap();
    assert_eq!(r.partition_key.paths().len(), 2);
}

// ---- items ---------------------------------------------------------------------------------

#[tokio::test]
async fn create_then_read_item_returns_the_same_document_with_system_properties() {
    let (_c, _db, items) = container().await;
    let r = items.create_item("p1", "a", doc("a", "p1", 7), None).await.unwrap();
    assert_eq!(r.status().status_code(), StatusCode::Created);
    let read = items.read_item("p1", "a", None).await.unwrap();
    assert!(read.headers().etag().is_some());
    let v: Value = read.into_model().unwrap();
    assert_eq!(v["n"], 7);
    assert!(v["_rid"].is_string());
    assert!(v["_etag"].is_string());
    assert!(v["_ts"].is_number());
}

#[tokio::test]
async fn create_existing_item_is_409_conflict() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "a", doc("a", "p1", 1), None).await.unwrap();
    let e = items.create_item("p1", "a", doc("a", "p1", 2), None).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::Conflict);
}

#[tokio::test]
async fn read_missing_item_is_404_not_found() {
    let (_c, _db, items) = container().await;
    let e = items.read_item("p1", "nope", None).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::NotFound);
}

#[tokio::test]
async fn same_id_in_two_partitions_are_two_items() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "same", doc("same", "p1", 1), None).await.unwrap();
    items.create_item("p2", "same", doc("same", "p2", 2), None).await.unwrap();
    let a: Doc = items.read_item("p1", "same", None).await.unwrap().into_model().unwrap();
    let b: Doc = items.read_item("p2", "same", None).await.unwrap().into_model().unwrap();
    assert_eq!((a.n, b.n), (1, 2));
}

#[tokio::test]
async fn replace_item_overwrites_the_document_and_changes_the_etag() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "a", doc("a", "p1", 1), None).await.unwrap();
    let e1 = items.read_item("p1", "a", None).await.unwrap().headers().etag().cloned().unwrap();
    items.replace_item("p1", "a", doc("a", "p1", 2), None).await.unwrap();
    let r = items.read_item("p1", "a", None).await.unwrap();
    let e2 = r.headers().etag().cloned().unwrap();
    let d: Doc = r.into_model().unwrap();
    assert_eq!(d.n, 2);
    assert_ne!(e1, e2);
}

#[tokio::test]
async fn replace_missing_item_is_404() {
    let (_c, _db, items) = container().await;
    let e = items.replace_item("p1", "ghost", doc("ghost", "p1", 1), None).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::NotFound);
}

#[tokio::test]
async fn upsert_creates_then_replaces() {
    let (_c, _db, items) = container().await;
    let r1 = items.upsert_item("p1", "u", doc("u", "p1", 1), None).await.unwrap();
    assert_eq!(r1.status().status_code(), StatusCode::Created);
    let r2 = items.upsert_item("p1", "u", doc("u", "p1", 2), None).await.unwrap();
    assert_eq!(r2.status().status_code(), StatusCode::Ok);
    let d: Doc = items.read_item("p1", "u", None).await.unwrap().into_model().unwrap();
    assert_eq!(d.n, 2);
}

#[tokio::test]
async fn delete_item_then_read_is_404_and_second_delete_is_404() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "d", doc("d", "p1", 1), None).await.unwrap();
    let r = items.delete_item("p1", "d", None).await.unwrap();
    assert_eq!(r.status().status_code(), StatusCode::NoContent);
    assert_eq!(status(&items.read_item("p1", "d", None).await.unwrap_err()), StatusCode::NotFound);
    assert_eq!(status(&items.delete_item("p1", "d", None).await.unwrap_err()), StatusCode::NotFound);
}

#[tokio::test]
async fn replace_with_stale_etag_is_412_precondition_failed() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "e", doc("e", "p1", 1), None).await.unwrap();
    let etag = items.read_item("p1", "e", None).await.unwrap().headers().etag().cloned().unwrap();
    items.replace_item("p1", "e", doc("e", "p1", 2), None).await.unwrap();
    let o = ItemWriteOptions::default().with_precondition(Precondition::if_match(etag));
    let e = items.replace_item("p1", "e", doc("e", "p1", 3), Some(o)).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::PreconditionFailed);
    assert!(e.status().is_precondition_failed());
    let d: Doc = items.read_item("p1", "e", None).await.unwrap().into_model().unwrap();
    assert_eq!(d.n, 2);
}

#[tokio::test]
async fn replace_with_current_etag_succeeds() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "e", doc("e", "p1", 1), None).await.unwrap();
    let etag: Etag = items.read_item("p1", "e", None).await.unwrap().headers().etag().cloned().unwrap();
    let o = ItemWriteOptions::default().with_precondition(Precondition::if_match(etag));
    items.replace_item("p1", "e", doc("e", "p1", 3), Some(o)).await.unwrap();
}

#[tokio::test]
async fn delete_with_stale_etag_is_412_and_item_survives() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "e", doc("e", "p1", 1), None).await.unwrap();
    let o = ItemWriteOptions::default().with_precondition(Precondition::if_match(Etag::from("\"00000000-0000-0000-0000-000000000000\"")));
    let e = items.delete_item("p1", "e", Some(o)).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::PreconditionFailed);
    items.read_item("p1", "e", None).await.unwrap();
}

#[tokio::test]
async fn item_id_with_special_characters_roundtrips() {
    let (_c, _db, items) = container().await;
    for id in ["with space", "unicodé-日本", "a+b", "100%", "q&a=1", "dots.and-dashes_1"] {
        items.create_item("p1", id, doc(id, "p1", 1), None).await.unwrap_or_else(|e| panic!("create {id:?}: {e}"));
        let d: Doc = items.read_item("p1", id, None).await.unwrap_or_else(|e| panic!("read {id:?}: {e}")).into_model().unwrap();
        assert_eq!(d.id, id);
    }
}

#[tokio::test]
async fn partition_key_values_of_every_scalar_type() {
    let (_c, _db, items) = container().await;
    items.create_item("text", "s", json!({"id":"s","pk":"text"}), None).await.unwrap();
    items.create_item(42i64, "n", json!({"id":"n","pk":42}), None).await.unwrap();
    items.create_item(true, "b", json!({"id":"b","pk":true}), None).await.unwrap();
    for (pk, id) in [(azure_data_cosmos::models::PartitionKey::from("text"), "s")] {
        let v: Value = items.read_item(pk, id, None).await.unwrap().into_model().unwrap();
        assert_eq!(v["pk"], "text");
    }
    let v: Value = items.read_item(42i64, "n", None).await.unwrap().into_model().unwrap();
    assert_eq!(v["pk"], 42);
    let v: Value = items.read_item(true, "b", None).await.unwrap().into_model().unwrap();
    assert_eq!(v["pk"], true);
}

#[tokio::test]
async fn item_with_pk_that_differs_from_the_header_is_400() {
    let (_c, _db, items) = container().await;
    let e = items.create_item("p1", "x", json!({"id":"x","pk":"other"}), None).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::BadRequest);
}

#[tokio::test]
async fn hierarchical_partition_key_items_are_addressed_by_both_values() {
    let c = client().await;
    let name = uniq("db");
    c.create_database(&name, None).await.unwrap();
    let db = c.database_client(&name);
    db.create_container(ContainerProperties::new("hier", PartitionKeyDefinition::from(("/tenant", "/user"))), None).await.unwrap();
    let items = db.container_client("hier", None).await.unwrap();
    items.create_item(("t1", "u1"), "a", json!({"id":"a","tenant":"t1","user":"u1","v":1}), None).await.unwrap();
    items.create_item(("t1", "u2"), "a", json!({"id":"a","tenant":"t1","user":"u2","v":2}), None).await.unwrap();
    let v: Value = items.read_item(("t1", "u2"), "a", None).await.unwrap().into_model().unwrap();
    assert_eq!(v["v"], 2);
}

// ---- patch ---------------------------------------------------------------------------------

#[tokio::test]
async fn patch_set_add_replace_remove_and_increment_on_the_server() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "a", json!({"id":"a","pk":"p1","n":1,"tags":["x"],"old":true}), None).await.unwrap();
    let patch = PatchInstructions::new()
        .with_operation(PatchOperation::set("/title", json!("hello")))
        .with_operation(PatchOperation::add("/tags/-", json!("y")))
        .with_operation(PatchOperation::replace("/n", json!(5)))
        .with_operation(PatchOperation::remove("/old"));
    let opts = PatchItemOptions::default().with_strategy(PatchStrategy::ServerSide);
    items.patch_item("p1", "a", patch, Some(opts)).await.unwrap();
    let v: Value = items.read_item("p1", "a", None).await.unwrap().into_model().unwrap();
    assert_eq!(v["title"], "hello");
    assert_eq!(v["tags"], json!(["x", "y"]));
    assert_eq!(v["n"], 5);
    assert!(v.get("old").is_none());
}

#[tokio::test]
async fn patch_increment_adds_to_a_number() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "a", json!({"id":"a","pk":"p1","n":10}), None).await.unwrap();
    let patch = PatchInstructions::new().with_operation(PatchOperation::increment("/n", CosmosNumber::from(5i64)));
    let opts = PatchItemOptions::default().with_strategy(PatchStrategy::ServerSide);
    items.patch_item("p1", "a", patch, Some(opts)).await.unwrap();
    let v: Value = items.read_item("p1", "a", None).await.unwrap().into_model().unwrap();
    assert_eq!(v["n"], 15);
}

#[tokio::test]
async fn patch_replace_of_a_missing_path_is_400() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "a", json!({"id":"a","pk":"p1"}), None).await.unwrap();
    let patch = PatchInstructions::new().with_operation(PatchOperation::replace("/nope", json!(1)));
    let opts = PatchItemOptions::default().with_strategy(PatchStrategy::ServerSide);
    let e = items.patch_item("p1", "a", patch, Some(opts)).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::BadRequest);
}

#[tokio::test]
async fn patch_of_a_missing_item_is_404() {
    let (_c, _db, items) = container().await;
    let patch = PatchInstructions::new().with_operation(PatchOperation::set("/a", json!(1)));
    let opts = PatchItemOptions::default().with_strategy(PatchStrategy::ServerSide);
    let e = items.patch_item("p1", "ghost", patch, Some(opts)).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::NotFound);
}

#[tokio::test]
async fn client_side_patch_with_move_applies_all_operations() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "a", json!({"id":"a","pk":"p1","tags":["first","second"],"visits":0}), None).await.unwrap();
    let patch = PatchInstructions::new()
        .with_operation(PatchOperation::move_value("/tags/0", "/headline"))
        .with_operation(PatchOperation::increment("/visits", CosmosNumber::from(1i64)));
    let opts = PatchItemOptions::default().with_strategy(PatchStrategy::ClientSide).with_max_attempts(NonZeroU8::new(3).unwrap());
    items.patch_item("p1", "a", patch, Some(opts)).await.unwrap();
    let v: Value = items.read_item("p1", "a", None).await.unwrap().into_model().unwrap();
    assert_eq!(v["headline"], "first");
    assert_eq!(v["visits"], 1);
}

// ---- queries -------------------------------------------------------------------------------

async fn seed(items: &ContainerClient) {
    for p in 0..3 {
        for i in 0..4 {
            let id = format!("p{p}-i{i}");
            items.create_item(format!("p{p}"), &id, doc(&id, &format!("p{p}"), (p * 10 + i) as i64), None).await.unwrap();
        }
    }
}

#[tokio::test]
async fn single_partition_query_with_parameter_returns_matching_items() {
    let (_c, _db, items) = container().await;
    seed(&items).await;
    let q = Query::from("SELECT * FROM c WHERE c.n >= @min").with_parameter("@min", 12).unwrap();
    let mut got: Vec<Doc> = collect(&items, q, FeedScope::partition("p1"), None).await;
    got.sort_by_key(|d| d.n);
    assert_eq!(got.iter().map(|d| d.n).collect::<Vec<_>>(), vec![12, 13]);
}

#[tokio::test]
async fn cross_partition_query_returns_items_from_every_partition() {
    let (_c, _db, items) = container().await;
    seed(&items).await;
    let got: Vec<Doc> = collect(&items, Query::from("SELECT * FROM c"), FeedScope::full_container(), None).await;
    assert_eq!(got.len(), 12);
}

#[tokio::test]
async fn cross_partition_query_with_order_by_returns_sorted_results() {
    let (_c, _db, items) = container().await;
    seed(&items).await;
    let got: Vec<Value> = collect(&items, Query::from("SELECT c.n FROM c ORDER BY c.n DESC"), FeedScope::full_container(), None).await;
    let ns: Vec<i64> = got.iter().map(|v| v["n"].as_i64().unwrap()).collect();
    let mut want = ns.clone();
    want.sort_by(|a, b| b.cmp(a));
    assert_eq!(ns, want);
    assert_eq!(ns.len(), 12);
}

#[tokio::test]
async fn aggregate_count_and_sum_within_one_partition() {
    // The SDK itself refuses aggregates across partitions ("unsupported query feature: aggregates in
    // cross-partition queries", client error 400/20113), so only single-partition aggregates are tested.
    let (_c, _db, items) = container().await;
    seed(&items).await;
    let n: Vec<i64> = collect(&items, Query::from("SELECT VALUE COUNT(1) FROM c"), FeedScope::partition("p1"), None).await;
    assert_eq!(n, vec![4]);
    let s: Vec<i64> = collect(&items, Query::from("SELECT VALUE SUM(c.n) FROM c"), FeedScope::partition("p1"), None).await;
    assert_eq!(s, vec![10 + 11 + 12 + 13]);
}

#[tokio::test]
async fn query_pages_by_max_item_count_and_resumes_from_continuation_token() {
    let (_c, _db, items) = container().await;
    seed(&items).await;
    let opts = QueryOptions::default().with_max_item_count(MaxItemCountHint::Limit(NonZeroU32::new(5).unwrap()));
    let mut pages = items
        .query_items::<Doc>(
            Query::from("SELECT * FROM c WHERE c.pk = 'p0' OR c.pk = 'p1' OR c.pk = 'p2'"),
            FeedScope::full_container(),
            Some(opts),
        )
        .await
        .unwrap()
        .into_pages();
    let mut total = 0;
    let mut sizes = vec![];
    while let Some(p) = pages.try_next().await.unwrap() {
        sizes.push(p.items().len());
        total += p.items().len();
    }
    assert_eq!(total, 12);
    assert!(sizes.len() >= 3, "5 per page over 12 items must make at least 3 pages, got {sizes:?}");
    assert!(sizes.iter().all(|s| *s <= 5), "{sizes:?}");
}

#[tokio::test]
async fn query_with_string_function_and_projection() {
    let (_c, _db, items) = container().await;
    seed(&items).await;
    let got: Vec<String> =
        collect(&items, Query::from("SELECT VALUE UPPER(c.id) FROM c WHERE STARTSWITH(c.id, 'p2-')"), FeedScope::partition("p2"), None)
            .await;
    let mut got = got;
    got.sort();
    assert_eq!(got, vec!["P2-I0", "P2-I1", "P2-I2", "P2-I3"]);
}

#[tokio::test]
async fn invalid_query_syntax_is_400() {
    let (_c, _db, items) = container().await;
    let r = items.query_items::<Value>(Query::from("SELEKT * FROM c"), FeedScope::partition("p1"), None).await;
    let err = match r {
        Err(e) => e,
        Ok(mut it) => it.try_next().await.unwrap_err(),
    };
    assert_eq!(status(&err), StatusCode::BadRequest);
}

#[tokio::test]
async fn query_with_array_contains_and_join() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "a", json!({"id":"a","pk":"p1","tags":["x","y"]}), None).await.unwrap();
    items.create_item("p1", "b", json!({"id":"b","pk":"p1","tags":["z"]}), None).await.unwrap();
    let ids: Vec<String> =
        collect(&items, Query::from("SELECT VALUE c.id FROM c WHERE ARRAY_CONTAINS(c.tags, 'y')"), FeedScope::partition("p1"), None).await;
    assert_eq!(ids, vec!["a"]);
    let tags: Vec<String> = collect(&items, Query::from("SELECT VALUE t FROM c JOIN t IN c.tags"), FeedScope::partition("p1"), None).await;
    let mut tags = tags;
    tags.sort();
    assert_eq!(tags, vec!["x", "y", "z"]);
}

// ---- batch ---------------------------------------------------------------------------------

#[tokio::test]
async fn transactional_batch_applies_all_operations_atomically() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "existing", doc("existing", "p1", 1), None).await.unwrap();
    let batch = TransactionalBatch::new("p1")
        .create_item(doc("new1", "p1", 10))
        .unwrap()
        .upsert_item(doc("new2", "p1", 20), None)
        .unwrap()
        .delete_item("existing", None);
    let r = items.execute_transactional_batch(batch, None).await.unwrap().into_model().unwrap();
    assert_eq!(r.results().len(), 3);
    assert!(r.results().iter().all(|x| x.is_success()));
    assert_eq!(status(&items.read_item("p1", "existing", None).await.unwrap_err()), StatusCode::NotFound);
    items.read_item("p1", "new1", None).await.unwrap();
}

#[tokio::test]
async fn failed_transactional_batch_changes_nothing() {
    let (_c, _db, items) = container().await;
    items.create_item("p1", "dup", doc("dup", "p1", 1), None).await.unwrap();
    let batch = TransactionalBatch::new("p1").create_item(doc("fresh", "p1", 10)).unwrap().create_item(doc("dup", "p1", 2)).unwrap();
    let r = items.execute_transactional_batch(batch, None).await;
    match r {
        Ok(resp) => {
            let m = resp.into_model().unwrap();
            assert!(m.results().iter().any(|x| !x.is_success()));
            assert_eq!(m.results()[1].status_code(), 409);
        }
        Err(e) => assert_eq!(status(&e), StatusCode::Conflict),
    }
    assert_eq!(status(&items.read_item("p1", "fresh", None).await.unwrap_err()), StatusCode::NotFound);
}

// ---- throughput ----------------------------------------------------------------------------

#[tokio::test]
async fn container_created_with_manual_throughput_reports_it() {
    let (_c, db, _items) = container().await;
    let o = CreateContainerOptions::default().with_throughput(ThroughputProperties::manual(1000));
    db.create_container(ContainerProperties::new("hot", "/pk".into()), Some(o)).await.unwrap();
    let hot = db.container_client("hot", None).await.unwrap();
    let t = hot.read_throughput(None).await.unwrap().expect("a dedicated container has an offer");
    assert_eq!(t.throughput(), Some(1000));
}

#[tokio::test]
async fn replace_throughput_changes_the_offer() {
    let (_c, db, _items) = container().await;
    let o = CreateContainerOptions::default().with_throughput(ThroughputProperties::manual(400));
    db.create_container(ContainerProperties::new("hot", "/pk".into()), Some(o)).await.unwrap();
    let hot = db.container_client("hot", None).await.unwrap();
    hot.begin_replace_throughput(ThroughputProperties::manual(800), None).await.unwrap().await.unwrap();
    assert_eq!(hot.read_throughput(None).await.unwrap().unwrap().throughput(), Some(800));
}

#[tokio::test]
async fn autoscale_throughput_reports_the_maximum() {
    let (_c, db, _items) = container().await;
    let o = CreateContainerOptions::default().with_throughput(ThroughputProperties::autoscale(4000, None));
    db.create_container(ContainerProperties::new("auto", "/pk".into()), Some(o)).await.unwrap();
    let c = db.container_client("auto", None).await.unwrap();
    let t = c.read_throughput(None).await.unwrap().unwrap();
    assert_eq!(t.autoscale_maximum(), Some(4000));
}

// ---- limits and validation -----------------------------------------------------------------

#[tokio::test]
async fn item_id_with_a_slash_is_400() {
    // Azure's service refuses '/' and backslash in an item id; '?' and '#' are accepted (database and container names refuse them).
    let (_c, _db, items) = container().await;
    for bad in ["a/b", "a\\b"] {
        let r = items.create_item("p1", bad, json!({"id": bad, "pk": "p1"}), None).await;
        let e = r.expect_err(bad);
        assert_eq!(status(&e), StatusCode::BadRequest, "id {bad:?}");
    }
}

#[tokio::test]
async fn item_larger_than_2_mib_is_413_request_entity_too_large() {
    let (_c, _db, items) = container().await;
    let big = "x".repeat(2 * 1024 * 1024 + 1024);
    let e = items.create_item("p1", "big", json!({"id":"big","pk":"p1","blob":big}), None).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::PayloadTooLarge);
}

#[tokio::test]
async fn item_close_to_the_2_mib_limit_is_stored() {
    let (_c, _db, items) = container().await;
    let big = "x".repeat(1_900_000);
    items.create_item("p1", "big", json!({"id":"big","pk":"p1","blob":big}), None).await.unwrap();
    let v: Value = items.read_item("p1", "big", None).await.unwrap().into_model().unwrap();
    assert_eq!(v["blob"].as_str().unwrap().len(), 1_900_000);
}

#[tokio::test]
async fn unique_key_policy_rejects_a_duplicate_value_in_the_same_partition() {
    use azure_data_cosmos::models::{UniqueKey, UniqueKeyPolicy};
    let c = client().await;
    let name = uniq("db");
    c.create_database(&name, None).await.unwrap();
    let db = c.database_client(&name);
    let mut props = ContainerProperties::new("uk", "/pk".into());
    props.unique_key_policy = Some(UniqueKeyPolicy::default().with_unique_key(UniqueKey::default().with_path("/email")));
    db.create_container(props, None).await.unwrap();
    let items = db.container_client("uk", None).await.unwrap();
    items.create_item("p1", "a", json!({"id":"a","pk":"p1","email":"x@example.com"}), None).await.unwrap();
    let e = items.create_item("p1", "b", json!({"id":"b","pk":"p1","email":"x@example.com"}), None).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::Conflict);
    items.create_item("p2", "c", json!({"id":"c","pk":"p2","email":"x@example.com"}), None).await.unwrap();
}

// ---- change feed ---------------------------------------------------------------------------

/// Read pages until the first empty one (the feed never ends by itself; an empty page means "caught up").
async fn drain<T: serde::de::DeserializeOwned + Send + 'static>(
    pages: &mut azure_data_cosmos::feed::ChangeFeedPageIterator<azure_data_cosmos::models::ChangeFeedItem<T>>,
) {
    while let Some(p) = pages.try_next().await.unwrap() {
        if p.items().is_empty() {
            break;
        }
    }
}

#[tokio::test]
async fn change_feed_from_the_beginning_returns_the_latest_version_of_each_item() {
    use azure_data_cosmos::options::ChangeFeedStartFrom;
    let (_c, _db, items) = container().await;
    items.create_item("p1", "a", doc("a", "p1", 1), None).await.unwrap();
    items.create_item("p2", "b", doc("b", "p2", 1), None).await.unwrap();
    items.replace_item("p1", "a", doc("a", "p1", 2), None).await.unwrap();
    let mut pages = items.query_change_feed::<Doc>(FeedScope::full_container(), ChangeFeedStartFrom::Beginning, None).await.unwrap();
    let mut got: Vec<Doc> = vec![];
    while let Some(p) = pages.try_next().await.unwrap() {
        if p.items().is_empty() {
            break; // caught up: the SDK keeps polling for ever, an empty page is the stop signal
        }
        got.extend(p.items().iter().filter_map(|i| i.current().cloned()));
    }
    got.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(got.iter().map(|d| (d.id.as_str(), d.n)).collect::<Vec<_>>(), vec![("a", 2), ("b", 1)]);
}

#[tokio::test]
async fn change_feed_resumed_from_a_continuation_token_returns_only_new_changes() {
    use azure_data_cosmos::options::ChangeFeedStartFrom;
    let (_c, _db, items) = container().await;
    items.create_item("p1", "a", doc("a", "p1", 1), None).await.unwrap();
    let mut pages = items.query_change_feed::<Doc>(FeedScope::full_container(), ChangeFeedStartFrom::Beginning, None).await.unwrap();
    drain(&mut pages).await;
    let token = pages.to_continuation_token().unwrap();
    items.create_item("p1", "b", doc("b", "p1", 1), None).await.unwrap();
    let opts = azure_data_cosmos::options::ChangeFeedOptions::default().with_continuation_token(token);
    let mut pages = items.query_change_feed::<Doc>(FeedScope::full_container(), ChangeFeedStartFrom::Beginning, Some(opts)).await.unwrap();
    let mut ids = vec![];
    while let Some(p) = pages.try_next().await.unwrap() {
        if p.items().is_empty() {
            break;
        }
        ids.extend(p.items().iter().filter_map(|i| i.current().map(|d| d.id.clone())));
    }
    assert_eq!(ids, vec!["b"]);
}

#[tokio::test]
async fn change_feed_all_versions_and_deletes_reports_create_replace_and_delete() {
    use azure_data_cosmos::options::{ChangeFeedMode, ChangeFeedOptions, ChangeFeedStartFrom};
    let (_c, db, _i) = container().await;
    // All versions and deletes needs a container with the feature switched on.
    let mut props = ContainerProperties::new("cf", "/pk".into());
    props.change_feed_policy =
        Some(azure_data_cosmos::models::ChangeFeedPolicy::default().with_retention_duration(std::time::Duration::from_secs(3600)));
    db.create_container(props, None).await.unwrap();
    let items = db.container_client("cf", None).await.unwrap();
    let mode = ChangeFeedOptions::default().with_mode(ChangeFeedMode::AllVersionsAndDeletes);
    let mut pages = items.query_change_feed::<Value>(FeedScope::full_container(), ChangeFeedStartFrom::Now, Some(mode)).await.unwrap();
    drain(&mut pages).await;
    let token = pages.to_continuation_token().unwrap();
    items.create_item("p1", "a", json!({"id":"a","pk":"p1","n":1}), None).await.unwrap();
    items.replace_item("p1", "a", json!({"id":"a","pk":"p1","n":2}), None).await.unwrap();
    items.delete_item("p1", "a", None).await.unwrap();
    let opts = ChangeFeedOptions::default().with_mode(ChangeFeedMode::AllVersionsAndDeletes).with_continuation_token(token);
    let mut pages = items.query_change_feed::<Value>(FeedScope::full_container(), ChangeFeedStartFrom::Now, Some(opts)).await.unwrap();
    let mut ops = vec![];
    while let Some(p) = pages.try_next().await.unwrap() {
        if p.items().is_empty() {
            break;
        }
        ops.extend(p.items().iter().map(|i| format!("{:?}", i.operation_type().unwrap())));
    }
    assert_eq!(ops, vec!["Create", "Replace", "Delete"]);
}
