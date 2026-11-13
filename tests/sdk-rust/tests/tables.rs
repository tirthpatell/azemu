//! Table Storage through `azure_data_tables` 0.21.0 (October 2024; the newest crate the Azure SDK team has published
//! for Tables, built on `azure_core` 0.21 and `azure_storage` 0.21). Plain HTTP with Shared Key (the crate has no bearer token),
//! rustls without the platform roots (reqwest 0.12), so HTTPS to the emulator's own CA is not used here.

use azemu_sdk_rust_tests::{emu, uniq};
use azure_core_legacy::{Etag, StatusCode};
use azure_data_tables::operations::InsertEntityResponse;
use azure_data_tables::prelude::*;
use azure_storage_legacy::prelude::*;
use azure_storage_legacy::CloudLocation;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

const DEV_ACCOUNT: &str = "devstoreaccount1";
const DEV_KEY: &str = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
const OTHER_KEY: &str = "c2VjcmV0LWtleS1vZi1hbm90aGVyLWFjY291bnQ=";

fn service_for(account: &str, key: &str) -> TableServiceClient {
    let uri = emu().http("table", account);
    let uri = uri.trim_end_matches('/').to_string();
    azure_data_tables::clients::TableServiceClientBuilder::with_location(
        CloudLocation::Custom { account: account.into(), uri },
        StorageCredentials::access_key(account.to_string(), key.to_string()),
    )
    .build()
}

/// The development account (its Shared Key signature is checked by the emulator).
fn service() -> TableServiceClient {
    service_for(DEV_ACCOUNT, DEV_KEY)
}

async fn table() -> (TableServiceClient, TableClient) {
    let s = service();
    let t = s.table_client(uniq("t"));
    t.create().await.unwrap();
    (s, t)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Person {
    #[serde(rename = "PartitionKey")]
    pk: String,
    #[serde(rename = "RowKey")]
    rk: String,
    name: String,
    age: i32,
}

fn person(pk: &str, rk: &str, name: &str, age: i32) -> Person {
    Person { pk: pk.into(), rk: rk.into(), name: name.into(), age }
}

fn status(e: &azure_core_legacy::Error) -> Option<StatusCode> {
    e.as_http_error().map(|h| h.status())
}

async fn all<T: serde::de::DeserializeOwned + Send + Sync + 'static>(q: azure_data_tables::operations::QueryEntityBuilder) -> Vec<T> {
    let mut s = q.into_stream::<T>();
    let mut out = vec![];
    while let Some(page) = s.next().await {
        out.extend(page.unwrap().entities);
    }
    out
}

#[tokio::test]
async fn create_table_then_list_tables_contains_it() {
    let (s, t) = table().await;
    let mut names = vec![];
    let mut st = s.list().into_stream();
    while let Some(p) = st.next().await {
        names.extend(p.unwrap().tables.into_iter().map(|t| t.name));
    }
    assert!(names.contains(&t.table_name().to_string()), "{names:?}");
}

#[tokio::test]
async fn create_existing_table_is_409_table_already_exists() {
    let (_s, t) = table().await;
    let e = t.create().await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::Conflict));
}

#[tokio::test]
async fn delete_table_then_insert_is_404_table_not_found() {
    let (_s, t) = table().await;
    t.delete().await.unwrap();
    let e = t.insert::<_, Person>(&person("p", "r", "n", 1)).unwrap().await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
}

#[tokio::test]
async fn invalid_table_name_is_400() {
    let s = service();
    let e = s.table_client("bad-name").create().await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::BadRequest));
}

#[tokio::test]
async fn insert_then_get_entity_returns_the_same_values_and_an_etag() {
    let (_s, t) = table().await;
    let p = person("city", "row1", "Ann", 31);
    let r: InsertEntityResponse<Person> = t.insert(&p).unwrap().await.unwrap();
    assert!(!r.etag.to_string().is_empty());
    let got = t.partition_key_client("city").entity_client("row1").get::<Person>().await.unwrap();
    assert_eq!(got.entity, p);
    assert_eq!(got.etag.to_string(), r.etag.to_string());
}

#[tokio::test]
async fn insert_duplicate_entity_is_409_entity_already_exists() {
    let (_s, t) = table().await;
    let p = person("c", "r", "n", 1);
    let _: InsertEntityResponse<Person> = t.insert(&p).unwrap().await.unwrap();
    let e = t.insert::<_, Person>(&p).unwrap().await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::Conflict));
}

#[tokio::test]
async fn get_missing_entity_is_404() {
    let (_s, t) = table().await;
    let e = t.partition_key_client("c").entity_client("nope").get::<Person>().await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
}

#[tokio::test]
async fn update_with_current_etag_replaces_the_entity() {
    let (_s, t) = table().await;
    let r: InsertEntityResponse<Person> = t.insert(&person("c", "r", "old", 1)).unwrap().await.unwrap();
    let ec = t.partition_key_client("c").entity_client("r");
    ec.update(&person("c", "r", "new", 2), Etag::from(r.etag.to_string()).into()).unwrap().await.unwrap();
    assert_eq!(ec.get::<Person>().await.unwrap().entity.name, "new");
}

#[tokio::test]
async fn update_with_stale_etag_is_412_precondition_failed() {
    let (_s, t) = table().await;
    let r: InsertEntityResponse<Person> = t.insert(&person("c", "r", "v1", 1)).unwrap().await.unwrap();
    let ec = t.partition_key_client("c").entity_client("r");
    ec.update(&person("c", "r", "v2", 2), IfMatchCondition::Any).unwrap().await.unwrap();
    let e = ec.update(&person("c", "r", "v3", 3), Etag::from(r.etag.to_string()).into()).unwrap().await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::PreconditionFailed));
    assert_eq!(ec.get::<Person>().await.unwrap().entity.name, "v2");
}

#[tokio::test]
async fn merge_keeps_properties_that_are_not_in_the_request() {
    let (_s, t) = table().await;
    let _: InsertEntityResponse<Person> = t.insert(&person("c", "r", "keep", 7)).unwrap().await.unwrap();
    let ec = t.partition_key_client("c").entity_client("r");
    ec.merge(&serde_json::json!({"PartitionKey":"c","RowKey":"r","extra":"added"}), IfMatchCondition::Any).unwrap().await.unwrap();
    let v: serde_json::Value = ec.get().await.unwrap().entity;
    assert_eq!(v["name"], "keep");
    assert_eq!(v["age"], 7);
    assert_eq!(v["extra"], "added");
}

#[tokio::test]
async fn update_replaces_all_properties_so_unlisted_ones_disappear() {
    let (_s, t) = table().await;
    let _: InsertEntityResponse<Person> = t.insert(&person("c", "r", "n", 7)).unwrap().await.unwrap();
    let ec = t.partition_key_client("c").entity_client("r");
    ec.update(&serde_json::json!({"PartitionKey":"c","RowKey":"r","only":"this"}), IfMatchCondition::Any).unwrap().await.unwrap();
    let v: serde_json::Value = ec.get().await.unwrap().entity;
    assert!(v.get("name").is_none());
    assert_eq!(v["only"], "this");
}

#[tokio::test]
async fn insert_or_replace_creates_and_then_replaces() {
    let (_s, t) = table().await;
    let ec = t.partition_key_client("c").entity_client("r");
    ec.insert_or_replace(&person("c", "r", "first", 1)).unwrap().await.unwrap();
    ec.insert_or_replace(&person("c", "r", "second", 2)).unwrap().await.unwrap();
    assert_eq!(ec.get::<Person>().await.unwrap().entity.name, "second");
}

#[tokio::test]
async fn insert_or_merge_merges_into_an_existing_entity() {
    let (_s, t) = table().await;
    let _: InsertEntityResponse<Person> = t.insert(&person("c", "r", "n", 1)).unwrap().await.unwrap();
    let ec = t.partition_key_client("c").entity_client("r");
    ec.insert_or_merge(&serde_json::json!({"PartitionKey":"c","RowKey":"r","more":true})).unwrap().await.unwrap();
    let v: serde_json::Value = ec.get().await.unwrap().entity;
    assert_eq!(v["name"], "n");
    assert_eq!(v["more"], true);
}

#[tokio::test]
async fn delete_entity_then_get_is_404() {
    let (_s, t) = table().await;
    let _: InsertEntityResponse<Person> = t.insert(&person("c", "r", "n", 1)).unwrap().await.unwrap();
    let ec = t.partition_key_client("c").entity_client("r");
    ec.delete().await.unwrap();
    assert_eq!(status(&ec.get::<Person>().await.unwrap_err()), Some(StatusCode::NotFound));
}

#[tokio::test]
async fn query_with_filter_returns_only_matching_entities() {
    let (_s, t) = table().await;
    for i in 0..10 {
        let _: InsertEntityResponse<Person> = t.insert(&person("p", &format!("r{i:02}"), &format!("n{i}"), i)).unwrap().await.unwrap();
    }
    let got: Vec<Person> = all(t.query().filter("age ge 7")).await;
    let mut ages: Vec<i32> = got.iter().map(|p| p.age).collect();
    ages.sort();
    assert_eq!(ages, vec![7, 8, 9]);
}

#[tokio::test]
async fn query_filter_on_partition_and_row_key_and_string_with_quote() {
    let (_s, t) = table().await;
    for (pk, rk, n) in [("a", "1", "O'Brien"), ("a", "2", "plain"), ("b", "1", "other")] {
        let _: InsertEntityResponse<Person> = t.insert(&person(pk, rk, n, 1)).unwrap().await.unwrap();
    }
    let got: Vec<Person> = all(t.query().filter("PartitionKey eq 'a' and name eq 'O''Brien'")).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].name, "O'Brien");
    let got: Vec<Person> = all(t.query().filter("RowKey eq '1'")).await;
    assert_eq!(got.len(), 2);
}

#[tokio::test]
async fn query_top_pages_through_continuation_tokens() {
    let (_s, t) = table().await;
    for i in 0..7 {
        let _: InsertEntityResponse<Person> = t.insert(&person("p", &format!("r{i}"), "n", i)).unwrap().await.unwrap();
    }
    let mut s = t.query().top(3).into_stream::<Person>();
    let mut sizes = vec![];
    while let Some(p) = s.next().await {
        sizes.push(p.unwrap().entities.len());
    }
    assert_eq!(sizes, vec![3, 3, 1]);
}

#[tokio::test]
async fn query_returns_entities_ordered_by_partition_key_then_row_key() {
    let (_s, t) = table().await;
    for (pk, rk) in [("b", "2"), ("a", "2"), ("b", "1"), ("a", "1")] {
        let _: InsertEntityResponse<Person> = t.insert(&person(pk, rk, "n", 1)).unwrap().await.unwrap();
    }
    let got: Vec<Person> = all(t.query()).await;
    let keys: Vec<_> = got.iter().map(|p| format!("{}{}", p.pk, p.rk)).collect();
    assert_eq!(keys, vec!["a1", "a2", "b1", "b2"]);
}

#[tokio::test]
async fn query_select_returns_only_the_selected_properties() {
    let (_s, t) = table().await;
    let _: InsertEntityResponse<Person> = t.insert(&person("p", "r", "n", 5)).unwrap().await.unwrap();
    let got: Vec<serde_json::Value> = all(t.query().select("name")).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["name"], "n");
    assert!(got[0].get("age").is_none());
}

#[tokio::test]
async fn transaction_applies_inserts_updates_and_deletes_in_one_partition_atomically() {
    let (_s, t) = table().await;
    let _: InsertEntityResponse<Person> = t.insert(&person("p", "old", "n", 1)).unwrap().await.unwrap();
    let pk = t.partition_key_client("p");
    let r = pk
        .transaction()
        .insert(&person("p", "a", "n", 1))
        .unwrap()
        .insert(&person("p", "b", "n", 2))
        .unwrap()
        .delete("old", None)
        .unwrap()
        .await
        .unwrap();
    assert!(
        r.operation_responses.iter().all(|x| [StatusCode::Created, StatusCode::NoContent, StatusCode::Ok].contains(&x.status_code)),
        "{:?}",
        r.operation_responses
    );
    let got: Vec<Person> = all(t.query()).await;
    assert_eq!(got.iter().map(|p| p.rk.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
}

#[tokio::test]
async fn failed_transaction_changes_nothing() {
    let (_s, t) = table().await;
    let _: InsertEntityResponse<Person> = t.insert(&person("p", "exists", "n", 1)).unwrap().await.unwrap();
    let pk = t.partition_key_client("p");
    let r = pk.transaction().insert(&person("p", "fresh", "n", 1)).unwrap().insert(&person("p", "exists", "n", 2)).unwrap().await;
    assert!(r.is_err() || r.unwrap().operation_responses.iter().any(|x| x.status_code == StatusCode::Conflict));
    let got: Vec<Person> = all(t.query()).await;
    assert_eq!(got.iter().map(|p| p.rk.as_str()).collect::<Vec<_>>(), vec!["exists"]);
}

#[tokio::test]
async fn entity_values_with_unicode_and_markup_roundtrip() {
    let (_s, t) = table().await;
    let name = "héllo 日本語 <b>&amp;</b> \u{1F600}";
    let _: InsertEntityResponse<Person> = t.insert(&person("p", "r", name, 1)).unwrap().await.unwrap();
    assert_eq!(t.partition_key_client("p").entity_client("r").get::<Person>().await.unwrap().entity.name, name);
}

#[tokio::test]
async fn typed_properties_keep_their_types_through_odata_annotations() {
    let (_s, t) = table().await;
    let e = serde_json::json!({
        "PartitionKey": "p", "RowKey": "r",
        "big": "9007199254740993", "big@odata.type": "Edm.Int64",
        "when": "2024-05-06T07:08:09.0000000Z", "when@odata.type": "Edm.DateTime",
        "bin": "AAECAw==", "bin@odata.type": "Edm.Binary",
        "flag": true, "ratio": 1.5, "text": "t"
    });
    let _: InsertEntityResponse<serde_json::Value> = t.insert(&e).unwrap().await.unwrap();
    let v: serde_json::Value = t.partition_key_client("p").entity_client("r").get().await.unwrap().entity;
    assert_eq!(v["big"], "9007199254740993");
    // Azure Storage tables (not the Cosmos table API) return whole seconds without the fraction.
    assert_eq!(v["when"], "2024-05-06T07:08:09Z");
    assert_eq!(v["bin"], "AAECAw==");
    assert_eq!(v["flag"], true);
    assert_eq!(v["ratio"], 1.5);
}

#[tokio::test]
async fn row_key_with_spaces_and_unicode_is_addressable() {
    let (_s, t) = table().await;
    for rk in ["with space", "unicodé", "plus+sign", "percent%sign"] {
        let _: InsertEntityResponse<Person> =
            t.insert(&person("p", rk, "n", 1)).unwrap().await.unwrap_or_else(|e| panic!("insert {rk:?}: {e}"));
        let got = t.partition_key_client("p").entity_client(rk).get::<Person>().await.unwrap_or_else(|e| panic!("get {rk:?}: {e}"));
        assert_eq!(got.entity.rk, rk);
    }
}

#[tokio::test]
async fn another_account_with_its_own_key_works_and_is_separate() {
    let a = uniq("acct");
    let s = service_for(&a, OTHER_KEY);
    let t = s.table_client("tbl1");
    t.create().await.unwrap();
    let _: InsertEntityResponse<Person> = t.insert(&person("p", "r", "n", 1)).unwrap().await.unwrap();
    let b = service_for(&uniq("acct"), OTHER_KEY).table_client("tbl1");
    assert_eq!(status(&b.partition_key_client("p").entity_client("r").get::<Person>().await.unwrap_err()), Some(StatusCode::NotFound));
}

#[tokio::test]
async fn wrong_shared_key_for_the_development_account_is_403() {
    let s = service_for(DEV_ACCOUNT, OTHER_KEY);
    let e = s.table_client(uniq("t")).create().await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::Forbidden));
}
