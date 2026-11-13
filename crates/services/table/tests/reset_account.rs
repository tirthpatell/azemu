//! Resetting one storage account (`POST /_azemu/reset?account=<name>`): its tables, entities and service
//! properties go; the other accounts stay; persistence sees a change.

mod common;

use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_table::TableService;
use common::*;

async fn create_and_insert(srv: &TestServer, base: &str) {
    let r = srv.send("POST", &format!("{base}/Tables"), &[AUTH, JSON], r#"{"TableName":"people"}"#.to_string()).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = srv.send("POST", &format!("{base}/people"), &[AUTH, JSON], r#"{"PartitionKey":"p","RowKey":"r","v":1}"#.to_string()).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

#[tokio::test]
async fn one_account_is_emptied_and_the_other_is_not() {
    let svc = Arc::new(TableService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let (alpha, beta) = (srv.url("table", "alpha"), srv.url("table", "beta"));
    create_and_insert(&srv, &alpha).await;
    create_and_insert(&srv, &beta).await;
    let props = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>https://example.com</AllowedOrigins><AllowedMethods>GET</AllowedMethods><AllowedHeaders>*</AllowedHeaders><ExposedHeaders>*</ExposedHeaders><MaxAgeInSeconds>60</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
    assert_eq!(srv.send("PUT", &format!("{alpha}/?restype=service&comp=properties"), &[AUTH], props).await.status, 202);

    let before = svc.persist_generation();
    assert!(svc.reset_account("Alpha"));
    assert_ne!(svc.persist_generation(), before, "persistence must see the reset");

    let get = |base: &str| format!("{base}/people(PartitionKey='p',RowKey='r')");
    assert_eq!(srv.send("GET", &get(&alpha), &[AUTH], "").await.status, 404);
    let tables = srv.send("GET", &format!("{alpha}/Tables"), &[AUTH], "").await.json();
    assert!(tables["value"].as_array().unwrap().is_empty(), "{tables}");
    let r = srv.send("GET", &format!("{alpha}/?restype=service&comp=properties"), &[AUTH], "").await;
    assert!(!r.text().contains("example.com"), "the service properties went too");
    assert_eq!(srv.send("GET", &get(&beta), &[AUTH], "").await.status, 200);
    // the emptied account works again; an account that was never used is fine
    let r = srv.send("POST", &format!("{alpha}/Tables"), &[AUTH, JSON], r#"{"TableName":"people"}"#.to_string()).await;
    assert_eq!(r.status, 201);
    assert!(svc.reset_account("never-used"));
}

#[tokio::test]
async fn the_admin_endpoint_resets_one_account() {
    let srv = TestServer::start(vec![Arc::new(TableService::new())]).await;
    let (alpha, beta) = (srv.url("table", "alpha"), srv.url("table", "beta"));
    create_and_insert(&srv, &alpha).await;
    create_and_insert(&srv, &beta).await;
    let r = srv.send("POST", &format!("{}/_azemu/reset?account=alpha", srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let get = |base: &str| format!("{base}/people(PartitionKey='p',RowKey='r')");
    assert_eq!(srv.send("GET", &get(&alpha), &[AUTH], "").await.status, 404);
    assert_eq!(srv.send("GET", &get(&beta), &[AUTH], "").await.status, 200);
}
