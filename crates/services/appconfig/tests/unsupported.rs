//! A 501 names the operation for `GET /_azemu/unsupported`.

mod common;

use common::*;

#[tokio::test]
async fn unbuilt_operations_are_named_without_the_names_of_keys() {
    let t = start().await;
    let r = t.get("/nothing-here/some-key").await;
    assert_eq!(r.status, 501, "{}", r.text());
    let r = t.call("DELETE", "/kv", &[], "").await;
    assert_eq!(r.status, 501, "{}", r.text());
    let list = t.srv.send("GET", &format!("{}/_azemu/unsupported", t.srv.gateway_url()), &[], "").await;
    let text = list.text();
    for name in ["AppConfig_Unknown (GET /{}/{})", "AppConfig_Unknown (DELETE /kv)"] {
        assert!(text.contains(name), "{name} is not in {text}");
    }
    assert!(!text.contains("some-key"), "{text}");
}
