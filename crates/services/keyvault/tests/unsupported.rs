//! A 501 names the operation for `GET /_azemu/unsupported`.

mod common;

use common::*;

#[tokio::test]
async fn unbuilt_operations_are_named_without_the_names_of_secrets_and_keys() {
    let kv = Kv::new().await;
    assert_eq!(kv.get("/storage").await.status, 501);
    assert_eq!(kv.call("DELETE", "/keys/my-key/rotationpolicy", None).await.status, 501);
    assert_eq!(kv.call("DELETE", "/keys/other-key/rotationpolicy", None).await.status, 501);
    assert_eq!(kv.call("GET", "/secrets/s/v/extra", None).await.status, 501);
    let list = kv.srv.send("GET", &format!("{}/_azemu/unsupported", kv.srv.gateway_url()), &[], "").await;
    let text = list.text();
    for name in
        ["KeyVault_ManagedStorage (GET)", "KeyVault_Unknown (DELETE /keys/{}/rotationpolicy)", "KeyVault_Unknown (GET /secrets/{}/{}/{})"]
    {
        assert!(text.contains(name), "{name} is not in {text}");
    }
    assert!(!text.contains("my-key") && !text.contains("other-key"), "names of keys must not show: {text}");
}
