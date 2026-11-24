//! Blob over plain http: no certificate and no token.
mod common;

use azure_core::http::RequestContent;
use common::Emulator;

#[tokio::test]
async fn blob_over_http() {
    let emulator = Emulator::start().await;
    let service = emulator.blob_service("devstoreaccount1").await;

    // "uploads" comes from seed.json; nothing here creates it.
    let blob = service.blob_container_client("uploads").blob_client("hello.txt");
    blob.upload(RequestContent::from(b"hello".to_vec()), None).await.unwrap();
    let body = blob.download(None).await.unwrap().body.collect().await.unwrap();
    assert_eq!(&body[..], b"hello");
}
