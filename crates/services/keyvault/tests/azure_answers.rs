//! Answers of a real vault that were tried in November 2026 (`docs/services/keyvault.md`, "Checked against Azure"):
//! the `api-version` rules, the texts of the key operations, the limits on a secret, restore of a bad backup, paging.

mod common;

use common::*;
use serde_json::{json, Value};

fn msg(r: &azemu_core::testing::TestResponse) -> String {
    r.json()["error"]["message"].as_str().unwrap_or("").to_string()
}

async fn raw(kv: &Kv, query: &str) -> azemu_core::testing::TestResponse {
    kv.srv.request("GET", &format!("{}/secrets{query}", kv.base)).header("Authorization", "Bearer t").send().await
}

#[tokio::test]
async fn api_version_missing_or_unknown() {
    let kv = Kv::new().await;
    for q in ["", "?api-version="] {
        let r = raw(&kv, q).await;
        assert_eq!((r.status, code(&r).as_str(), msg(&r).as_str()), (400, "BadParameter", "api-version must be specified"));
    }
    for v in ["7.0", "7.6", "7.5-preview.1", "7.6-preview.2", "2016-10-01", "2025-07-01", "2026-04-01", "2026-01-01-preview"] {
        assert_eq!(raw(&kv, &format!("?api-version={v}")).await.status, 200, "{v}");
    }
    for v in ["1.0", "7.7", "2099-01-01", "2026-01-01", "2025-07-01-preview", "latest"] {
        let r = raw(&kv, &format!("?api-version={v}")).await;
        assert_eq!(r.status, 400, "{v}");
        assert_eq!(code(&r), "BadParameter");
        assert_eq!(msg(&r), format!("The specified version ({v}) is not recognized. Consider using the latest supported version (7.6)."));
    }
}

#[tokio::test]
async fn secret_limits_and_bodies() {
    let kv = Kv::new().await;
    let put = |v: Value| async { kv.call("PUT", "/secrets/s", Some(v)).await };
    assert_eq!(put(json!({"value": "x".repeat(25_600)})).await.status, 200);
    let r = put(json!({"value": "x".repeat(25_601)})).await;
    assert_eq!((r.status, code(&r).as_str(), inner_code(&r).as_str()), (400, "BadParameter", "SecretTooLarge"));
    assert_eq!(msg(&r), "Secret is beyond the maximum permitted length of 25600 characters.");
    // no value, a null value, an attributes that is not an object
    for body in [json!({}), json!({"value": null}), json!("text")] {
        let r = put(body).await;
        assert_eq!((r.status, msg(&r).as_str()), (400, "Property  has invalid value\r\n"));
    }
    let r = put(json!({"value": "v", "attributes": "x"})).await;
    assert_eq!(msg(&r), "Property attributes has invalid value\r\n");
    // a number is stored as its text
    assert_eq!(put(json!({"value": 5})).await.json()["value"], "5");
    // no body at all
    let r = kv.srv.request("PUT", &format!("{}/secrets/s?api-version=7.6", kv.base)).header("Authorization", "Bearer t").send().await;
    assert_eq!(msg(&r), "Request body not specified");
}

#[tokio::test]
async fn purge_and_recover_of_an_object_in_the_other_state() {
    let kv = Kv::new().await;
    kv.call("PUT", "/secrets/p", Some(json!({"value": "v"}))).await;
    let r = kv.call("DELETE", "/deletedsecrets/p", None).await;
    assert_eq!((r.status, code(&r).as_str(), msg(&r).as_str()), (400, "MethodNotAllowed", "Secret must be deleted prior to being purged."));
    let r = kv.call("POST", "/deletedsecrets/p/recover", None).await;
    assert_eq!((r.status, msg(&r).as_str()), (400, "Secret exists already: p"));
}

#[tokio::test]
async fn restore_of_a_bad_backup() {
    let kv = Kv::new().await;
    for kind in ["secrets", "keys", "certificates"] {
        for value in ["garbage", "aGVsbG8"] {
            let r = kv.post(&format!("/{kind}/restore"), json!({"value": value})).await;
            assert_eq!(
                (r.status, code(&r).as_str(), msg(&r).as_str()),
                (400, "Malformed backup blob", "Backup blob is corrupt."),
                "{kind} {value}"
            );
        }
        for body in [json!({"value": ""}), json!({})] {
            let r = kv.post(&format!("/{kind}/restore"), body).await;
            assert_eq!(msg(&r), "Property  has invalid value\r\n", "{kind}");
        }
    }
}

#[tokio::test]
async fn paging_limits_and_tokens() {
    let kv = Kv::new().await;
    for path in ["/secrets", "/keys", "/certificates", "/deletedsecrets", "/deletedkeys", "/deletedcertificates"] {
        for bad in ["0", "26", "500", "-1", "abc"] {
            let r = kv.get(&format!("{path}?maxresults={bad}")).await;
            assert_eq!((r.status, msg(&r).as_str()), (400, "invalid maxresults"), "{path} {bad}");
        }
        assert_eq!(kv.get(&format!("{path}?maxresults=25")).await.status, 200);
        let r = kv.get(&format!("{path}?$skiptoken=garbage")).await;
        assert_eq!((r.status, msg(&r).as_str()), (400, "Invalid skip token"), "{path}");
        // a token that is JSON the emulator did not write starts at the first page
        assert_eq!(kv.get(&format!("{path}?$skiptoken=e30")).await.status, 200);
        // the recordings replace tokens by this word
        assert_eq!(kv.get(&format!("{path}?$skiptoken=Sanitized")).await.status, 200);
    }
}

#[tokio::test]
async fn key_operation_texts() {
    let kv = Kv::new().await;
    let rsa = kv.post("/keys/r/create", json!({"kty": "RSA"})).await.json();
    let ec = kv.post("/keys/e/create", json!({"kty": "EC", "crv": "P-256"})).await.json();
    let kid = |k: &Value| k["key"]["kid"].as_str().unwrap().to_string();
    let enc = |v: &[u8]| b64(v);
    let post = |path: &'static str, body: Value| {
        let kv = &kv;
        async move { kv.post(path, body).await }
    };
    let m = |r: &azemu_core::testing::TestResponse| (r.status, msg(r));
    // unknown algorithm, or one of another kind of operation
    for (path, alg) in [("/keys/r/encrypt", "FOO"), ("/keys/r/encrypt", "ES256"), ("/keys/r/decrypt", "FOO")] {
        let r = post(path, json!({"alg": alg, "value": enc(b"x")})).await;
        assert_eq!(m(&r), (400, format!("Invalid algorithm: {alg}")), "{path}");
    }
    // too long for the key, and a ciphertext that does not decrypt (also one of the wrong size)
    let r = post("/keys/r/encrypt", json!({"alg": "RSA-OAEP", "value": enc(&[1u8; 300])})).await;
    assert_eq!(m(&r), (400, "Bad Length.\r\n".into()));
    for len in [256, 10] {
        let r = post("/keys/r/decrypt", json!({"alg": "RSA-OAEP", "value": enc(&vec![7u8; len])})).await;
        assert_eq!(m(&r), (400, "Error occurred while decoding OAEP padding.".into()), "{len}");
    }
    // sign and verify with a digest of the wrong size
    let r = post("/keys/r/sign", json!({"alg": "RS256", "value": enc(&[1u8; 10])})).await;
    assert_eq!(m(&r), (400, "Invalid length of 'value': 10 bytes. RS256 requires 32 bytes, encoded with base64url.".into()));
    let r = post("/keys/r/verify", json!({"alg": "RS256", "digest": enc(&[1u8; 10]), "value": enc(&[1u8; 256])})).await;
    assert_eq!(m(&r), (400, "Bad Hash.\r\n".into()));
    // the algorithm does not fit the key
    let r = post("/keys/r/sign", json!({"alg": "ES256", "value": enc(&[1u8; 32])})).await;
    assert_eq!(
        m(&r),
        (400, format!("Key and signing algorithm are incompatible. Key {} is of type 'RSA', and algorithm 'ES256' can only be used with a key of type 'EC' or 'EC-HSM'.", kid(&rsa)))
    );
    let r = post("/keys/e/sign", json!({"alg": "RS256", "value": enc(&[1u8; 32])})).await;
    assert_eq!(
        m(&r),
        (400, format!("Key and signing algorithm are incompatible. Key {} is of type 'EC', and algorithm 'RS256' can only be used with a key of type 'RSA' or 'RSA-HSM'.", kid(&ec)))
    );
    let r = post("/keys/e/sign", json!({"alg": "ES384", "value": enc(&[1u8; 48])})).await;
    assert_eq!(
        m(&r),
        (400, format!("Key and signing algorithm are incompatible. Key {} uses curve 'P-256', and algorithm 'ES384' can only be used with curve 'P-384'.", kid(&ec)))
    );
    // the body: every missing property is listed, in this order
    let r = post("/keys/r/encrypt", json!({})).await;
    assert_eq!(m(&r), (400, "Property  has invalid value\r\nProperty 'value' is required.\r\n".into()));
    let r = post("/keys/r/encrypt", json!({"alg": "RSA-OAEP"})).await;
    assert_eq!(m(&r), (400, "Property 'value' is required.\r\n".into()));
    let r = post("/keys/r/encrypt", json!({"alg": "RSA-OAEP", "value": "***"})).await;
    assert_eq!(m(&r), (400, "Content of 'value' is not base64url.\r\nProperty 'value' is required.\r\n".into()));
    let r = post("/keys/r/verify", json!({"alg": "RS256", "value": enc(b"x")})).await;
    assert_eq!(m(&r), (400, "Property  has invalid value\r\n".into()));
    // dates on the key
    let soon = azemu_core::time::OffsetDateTime::now_utc().unix_timestamp() + 3600;
    kv.post("/keys/later/create", json!({"kty": "RSA", "attributes": {"nbf": soon}})).await;
    let r = post("/keys/later/encrypt", json!({"alg": "RSA-OAEP", "value": enc(b"x")})).await;
    assert_eq!(m(&r), (403, "Operation encrypt is not allowed because the 'not before' constraint on this key is not met.".into()));
    kv.post("/keys/old/create", json!({"kty": "RSA", "attributes": {"exp": 1}})).await;
    let r = post("/keys/old/encrypt", json!({"alg": "RSA-OAEP", "value": enc(b"x")})).await;
    assert_eq!(m(&r), (403, "Operation encrypt is not allowed on an expired key.".into()));
}

#[tokio::test]
async fn creating_keys_the_vault_refuses() {
    let kv = Kv::new().await;
    for size in [512, 1024, 2000, 8192] {
        let r = kv.post("/keys/k/create", json!({"kty": "RSA", "key_size": size})).await;
        assert_eq!(msg(&r), format!("Unsupported key size: {size}. Valid values for created keys are 2048, 3072, 4096."));
    }
    let r = kv.post("/keys/k/create", json!({"kty": "EC", "crv": "P-999"})).await;
    assert_eq!(msg(&r), "Unsupported curve: \"P-999\". Supported curves are \"P-256\", \"P-384\", \"P-521\", \"P-256K\", \"SECP256K1\".");
    let r = kv.post("/keys/k/create", json!({"kty": "oct", "key_size": 256})).await;
    assert_eq!((r.status, code(&r).as_str(), inner_code(&r).as_str()), (400, "BadParameter", "KeyTypeNotSupported"));
    assert_eq!(msg(&r), "Software oct key type is not supported. Use oct-HSM.");
    assert_eq!(msg(&kv.post("/keys/k/create", json!({"kty": "FOO"})).await), "Property  has invalid value\r\n");
    assert_eq!(msg(&kv.post("/keys/k/create", json!({})).await), "Property  has invalid value\r\nProperty  has invalid value\r\n");
}
