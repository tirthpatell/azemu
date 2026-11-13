mod common;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use azemu_core::testing::Client;
use azemu_core::Emulator;
use azemu_table::TableService;
use common::{AUTH, JSON};

#[tokio::test]
async fn azurite_style_path_urls_on_a_pinned_port() {
    let dir = std::env::temp_dir().join(format!("azemu-table-pinned-{}", std::process::id()));
    let emu = Emulator::builder()
        .service(Arc::new(TableService::new()))
        .pinned(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0), "table")
        .cert_dir(dir.clone())
        .start()
        .await
        .unwrap();
    let port = emu.pinned_addr("table").unwrap().port();
    let c = Client::new();
    let base = format!("http://127.0.0.1:{port}/devstoreaccount1");

    let r = c.send("POST", &format!("{base}/Tables"), &[AUTH, JSON], r#"{"TableName":"tbl"}"#).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["odata.metadata"], format!("{base}/$metadata#Tables/@Element"));
    assert_eq!(r.header("location"), Some(format!("{base}/Tables('tbl')").as_str()));

    let r = c.send("POST", &format!("{base}/tbl"), &[AUTH, JSON], r#"{"PartitionKey":"p","RowKey":"r"}"#).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.header("location"), Some(format!("{base}/tbl(PartitionKey='p',RowKey='r')").as_str()));
    let r = c.send("GET", &format!("{base}/tbl()"), &[AUTH, ("Accept", "application/json;odata=fullmetadata")], "").await;
    assert_eq!(r.json()["odata.metadata"], format!("{base}/$metadata#tbl"));
    assert_eq!(r.json()["value"][0]["odata.id"], format!("{base}/tbl(PartitionKey='p',RowKey='r')"));

    // a batch whose embedded URLs use the path style
    let (a, b) = (r#"{"PartitionKey":"p","RowKey":"b1"}"#, "");
    let body = common::batch(&[
        ("POST", "/devstoreaccount1/tbl", &[], a),
        ("DELETE", "/devstoreaccount1/tbl(PartitionKey='p',RowKey='r')", &[("If-Match", "*")], b),
    ]);
    let r = c.send("POST", &format!("{base}/$batch"), &[AUTH, common::BATCH_CT], body).await;
    assert_eq!(r.status, 202);
    assert_eq!(common::parts(&r).iter().map(|p| p.0).collect::<Vec<_>>(), [201, 204], "{}", r.text());
    let r = c.send("GET", &format!("{base}/tbl(PartitionKey='p',RowKey='r')"), &[AUTH], "").await;
    assert_eq!(r.status, 404);

    // another account in the same server is separate
    let r = c.send("GET", &format!("http://127.0.0.1:{port}/other/Tables"), &[AUTH], "").await;
    assert_eq!(r.json()["value"].as_array().unwrap().len(), 0);
    emu.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}
