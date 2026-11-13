//! A 501 names the operation for `GET /_azemu/unsupported`.

mod common;

use common::*;

#[tokio::test]
async fn unbuilt_requests_are_named_without_tenants() {
    let srv = server().await;
    let r = srv.send("GET", &format!("{}/metadata/identity/nothing/like/this", gateway(&srv)), &[("Metadata", "true")], "").await;
    assert_eq!(r.status, 501, "{}", r.text());
    let r = srv.send("GET", &format!("{}/some-tenant-id/oauth2/never", login(&srv)), &[], "").await;
    assert_eq!(r.status, 501, "{}", r.text());
    let list = srv.send("GET", &format!("{}/_azemu/unsupported", gateway(&srv)), &[], "").await;
    let text = list.text();
    for name in ["Identity_Unknown (GET /metadata/identity/{}/{}/{})", "Identity_Unknown (GET /{}/oauth2/{})"] {
        assert!(text.contains(name), "{name} is not in {text}");
    }
    assert!(!text.contains("some-tenant-id"), "{text}");
}
