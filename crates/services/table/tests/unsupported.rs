//! A 501 names the operation for `GET /_azemu/unsupported`.

mod common;

use common::*;

#[tokio::test]
async fn an_unbuilt_request_is_named_by_its_shape() {
    let e = env().await;
    let r = e.send("DELETE", "/", &[], "").await;
    assert_eq!(r.status, 501, "{}", r.text());
    let list = e.srv.send("GET", &format!("{}/_azemu/unsupported", e.srv.gateway_url()), &[], "").await;
    assert!(list.text().contains("Table_Unknown (DELETE Service)"), "{}", list.text());
}
