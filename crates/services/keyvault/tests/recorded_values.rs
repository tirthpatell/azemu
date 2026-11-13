//! Values that the replay comparison found different from the recordings of real Azure.

mod common;

use common::*;
use serde_json::json;

const LONG: &str = "was not found in this key vault. If you recently deleted this";

/// Recordings `test_secrets_client.pyTestSecretClienttest_purge[2025-07-01].json` entries 47 to 53,
/// `test_key_client.pyTestKeyClienttest_purge[2025-07-01_vault].json` entries 46 to 50: after a purge a lookup of the deleted
/// name gets the long text of a missing object; while the name is still live it gets the short `Deleted ... not found`.
#[tokio::test]
async fn a_purged_name_gets_the_long_not_found_text_and_a_live_name_the_short_one() {
    let kv = Kv::new().await;

    assert_eq!(kv.call("PUT", "/secrets/s", Some(json!({"value": "v"}))).await.status, 200);
    let live = kv.get("/deletedsecrets/s").await;
    assert_eq!(live.status, 404);
    assert_eq!(live.json()["error"]["message"], "Deleted Secret not found: s");
    kv.call("DELETE", "/secrets/s", None).await;
    assert_eq!(kv.get("/deletedsecrets/s").await.status, 200);
    assert_eq!(kv.call("DELETE", "/deletedsecrets/s", None).await.status, 204);
    let gone = kv.get("/deletedsecrets/s").await;
    assert_eq!((gone.status, code(&gone)), (404, "SecretNotFound".into()));
    let m = gone.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(m.starts_with("A secret with (name/id) s ") && m.contains(LONG), "{m}");

    assert_eq!(kv.post("/keys/k/create", json!({"kty": "RSA"})).await.status, 200);
    assert_eq!(kv.get("/deletedkeys/k").await.json()["error"]["message"], "Deleted Key not found: k");
    kv.call("DELETE", "/keys/k", None).await;
    assert_eq!(kv.call("DELETE", "/deletedkeys/k", None).await.status, 204);
    let gone = kv.get("/deletedkeys/k").await;
    assert_eq!((gone.status, code(&gone)), (404, "KeyNotFound".into()));
    assert!(gone.json()["error"]["message"].as_str().unwrap().starts_with("A key with (name/id) k "));

    // a name that was never made is nowhere either
    let never = kv.get("/deletedsecrets/never").await;
    assert!(never.json()["error"]["message"].as_str().unwrap().contains(LONG));
}
