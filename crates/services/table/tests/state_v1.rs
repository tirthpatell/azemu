//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/table/table.state` (`AZEMU-STATE table 1`) is what `Service::save` wrote for the small fixed state built
//! by `fixed_state`. The file is checked in and is never rewritten by a test run. `version_1_files_still_load` copies it to a
//! scratch directory, loads it into a new service and checks what it holds, through the requests a client would send.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in file and do
//! not weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (reading into the same state this
//! test expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-table --test state_v1 -- --ignored write_the_fixture`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_table::TableService;
use common::*;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("table")
}

async fn start(svc: Arc<TableService>) -> Env {
    let srv = TestServer::start(vec![svc as Arc<dyn Service>]).await;
    let base = srv.url("table", "acct");
    Env { srv, base }
}

async fn fixed_state(svc: Arc<TableService>) -> Env {
    let e = start(svc).await;
    assert_eq!(e.create_table("people").await.status, 201);
    assert_eq!(e.create_table("empty").await.status, 201);
    let typed = r#"{"PartitionKey":"eu","RowKey":"1","name":"Ada","age":36,"score":2.5,"ok":true,
        "born@odata.type":"Edm.DateTime","born":"1815-12-10T00:00:00Z",
        "id@odata.type":"Edm.Guid","id":"11111111-2222-3333-4444-555555555555",
        "blob@odata.type":"Edm.Binary","blob":"AQID",
        "big@odata.type":"Edm.Int64","big":"9007199254740993"}"#;
    assert_eq!(e.insert("people", typed).await.status, 201);
    assert_eq!(e.insert("people", r#"{"PartitionKey":"eu","RowKey":"2","name":"Bob"}"#).await.status, 201);
    assert_eq!(e.insert("people", r#"{"PartitionKey":"us","RowKey":"1","name":"Cy"}"#).await.status, 201);
    let acl = "<SignedIdentifiers><SignedIdentifier><Id>p1</Id><AccessPolicy><Start>2024-01-01T00:00:00Z</Start><Expiry>2099-01-01T00:00:00Z</Expiry><Permission>raud</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(e.send("PUT", "/people?comp=acl", &[], acl).await.status, 204);
    let props = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>https://example.com</AllowedOrigins><AllowedMethods>GET</AllowedMethods><AllowedHeaders>*</AllowedHeaders><ExposedHeaders>*</ExposedHeaders><MaxAgeInSeconds>60</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
    assert_eq!(e.send("PUT", "/?restype=service&comp=properties", &[], props).await.status, 202);
    e
}

#[tokio::test]
#[ignore = "writes the checked-in fixture; see the module text"]
async fn write_the_fixture() {
    let svc = Arc::new(TableService::new());
    let _e = fixed_state(svc.clone()).await;
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    svc.save(&dir).unwrap();
}

#[tokio::test]
async fn version_1_files_still_load() {
    let dir = std::env::temp_dir().join(format!("azemu-table-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture_dir().join("table.state"), dir.join("table.state")).expect("tests/state-v1/table/table.state is checked in");

    let svc = Arc::new(TableService::new());
    if let Err(e) = svc.load(&dir) {
        panic!(
            "Version 1 table state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/persist.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        );
    }
    let e = start(svc).await;
    let tables = e.send("GET", "/Tables", &[], "").await.json();
    let mut names: Vec<&str> = tables["value"].as_array().unwrap().iter().map(|t| t["TableName"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, vec!["empty", "people"]);
    assert_eq!(e.row_keys("people", "").await, vec!["1", "2", "1"], "eu/1, eu/2, us/1");
    assert!(e.row_keys("empty", "").await.is_empty());
    // every property type kept its type and value
    let r = e.send("GET", "/people(PartitionKey='eu',RowKey='1')", &[("Accept", "application/json;odata=fullmetadata")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let j = r.json();
    assert_eq!(
        (j["name"].as_str(), j["age"].as_i64(), j["score"].as_f64(), j["ok"].as_bool()),
        (Some("Ada"), Some(36), Some(2.5), Some(true))
    );
    assert_eq!(j["born@odata.type"], "Edm.DateTime");
    assert!(j["born"].as_str().unwrap().starts_with("1815-12-10T00:00:00"), "{j}");
    assert_eq!((j["id@odata.type"].as_str(), j["id"].as_str()), (Some("Edm.Guid"), Some("11111111-2222-3333-4444-555555555555")));
    assert_eq!((j["blob@odata.type"].as_str(), j["blob"].as_str()), (Some("Edm.Binary"), Some("AQID")));
    assert_eq!((j["big@odata.type"].as_str(), j["big"].as_str()), (Some("Edm.Int64"), Some("9007199254740993")));
    assert!(j["odata.etag"].as_str().is_some(), "the entity kept its etag: {j}");
    let acl = e.send("GET", "/people?comp=acl", &[], "").await.text();
    assert!(acl.contains("<Id>p1</Id>") && acl.contains("<Permission>raud</Permission>"), "{acl}");
    let props = e.send("GET", "/?restype=service&comp=properties", &[], "").await.text();
    assert!(props.contains("https://example.com"), "{props}");
    let _ = std::fs::remove_dir_all(&dir);
}
