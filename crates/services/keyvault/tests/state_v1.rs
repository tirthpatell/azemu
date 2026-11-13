//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/keyvault/keyvault.state` (`AZEMU-STATE keyvault 1`) is what `Service::save` wrote for the small fixed
//! state built by `fixed_state`. The file is checked in and is never rewritten by a test run. `version_1_files_still_load`
//! copies it to a scratch directory, loads it into a new service and checks what it holds, through the requests a client
//! would send.
//!
//! **The file holds private keys.** They are throwaway ones: `fixed_state` has the service make them when the fixture is
//! written (an EC key, the key of a self-signed certificate) and imports the RSA key of `tests/fixtures/rsa2048.json`, which
//! is also a throwaway made for the tests. None of them protects anything.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in file and do
//! not weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (reading into the same state this
//! test expects), and add a fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when this state itself changes, which also means a new version):
//! `cargo test -p azemu-keyvault --test state_v1 -- --ignored write_the_fixture`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_keyvault::KeyVaultService;
use common::*;
use serde_json::{json, Value};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("keyvault")
}

async fn start(svc: Arc<KeyVaultService>) -> Kv {
    let srv = TestServer::start(vec![svc as Arc<dyn Service>]).await;
    let base = srv.url("vault", "fixture");
    Kv { srv, base }
}

async fn ok(kv: &Kv, method: &str, path: &str, body: Option<Value>) -> Value {
    let r = kv.call(method, path, body).await;
    assert!(r.status < 300, "{method} {path}: {} {}", r.status, r.text());
    if r.body.is_empty() {
        Value::Null
    } else {
        r.json()
    }
}

async fn fixed_state(svc: Arc<KeyVaultService>) -> Kv {
    let kv = start(svc).await;
    ok(&kv, "PUT", "/secrets/db-password", Some(json!({"value": "first", "contentType": "text/plain", "tags": {"env": "dev"}}))).await;
    ok(&kv, "PUT", "/secrets/db-password", Some(json!({"value": "second", "attributes": {"nbf": 1700000000, "exp": 4102444800i64}}))).await;
    ok(&kv, "PUT", "/secrets/deleted-one", Some(json!({"value": "bye"}))).await;
    ok(&kv, "DELETE", "/secrets/deleted-one", None).await;
    ok(&kv, "POST", "/keys/ec/create", Some(json!({"kty": "EC", "crv": "P-256", "tags": {"k": "v"}}))).await;
    let f: Value = serde_json::from_str(include_str!("fixtures/rsa2048.json")).unwrap();
    ok(&kv, "PUT", "/keys/imported", Some(json!({"key": {"kty": "RSA", "n": f["n"], "e": f["e"], "d": f["d"], "p": f["p"], "q": f["q"], "dp": f["dp"], "dq": f["dq"], "qi": f["qi"], "key_ops": ["sign", "verify"]}}))).await;
    let policy = json!({"key_props": {"kty": "EC", "crv": "P-256", "exportable": true, "reuse_key": false},
                        "x509_props": {"subject": "CN=web.example.com", "validity_months": 6}, "issuer": {"name": "Self"}});
    ok(&kv, "POST", "/certificates/web/create", Some(json!({"policy": policy}))).await;
    ok(&kv, "PUT", "/certificates/contacts", Some(json!({"contacts": [{"email": "a@x.com", "name": "A"}]}))).await;
    kv.call_at(&kv.vault_base("other"), "PUT", "/secrets/s", Some(json!({"value": "in the other vault"}))).await;
    kv
}

#[tokio::test]
#[ignore = "writes the checked-in fixture; see the module text"]
async fn write_the_fixture() {
    let svc = Arc::new(KeyVaultService::new());
    let _kv = fixed_state(svc.clone()).await;
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    svc.save(&dir).unwrap();
}

#[tokio::test]
async fn version_1_files_still_load() {
    let dir = std::env::temp_dir().join(format!("azemu-keyvault-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture_dir().join("keyvault.state"), dir.join("keyvault.state"))
        .expect("tests/state-v1/keyvault/keyvault.state is checked in");

    let svc = Arc::new(KeyVaultService::new());
    if let Err(e) = svc.load(&dir) {
        panic!(
            "Version 1 Key Vault state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/persist.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        );
    }
    let kv = start(svc).await;
    // secrets: the current value, the older version, the deleted one
    let s = ok(&kv, "GET", "/secrets/db-password", None).await;
    assert_eq!(s["value"], "second");
    assert_eq!(s["attributes"]["nbf"], 1700000000);
    let versions = ok(&kv, "GET", "/secrets/db-password/versions", None).await;
    assert_eq!(versions["value"].as_array().unwrap().len(), 2);
    let deleted = ok(&kv, "GET", "/deletedsecrets", None).await;
    assert_eq!(deleted["value"].as_array().unwrap().len(), 1);
    assert!(deleted["value"][0]["id"].as_str().unwrap().contains("deleted-one"));
    // keys keep their public parts and still sign with the private part
    let ec = ok(&kv, "GET", "/keys/ec", None).await;
    assert_eq!((ec["key"]["kty"].as_str(), ec["key"]["crv"].as_str()), (Some("EC"), Some("P-256")));
    assert_eq!(ec["tags"]["k"], "v");
    let digest = b64(&[7u8; 32]);
    let sig = ok(&kv, "POST", "/keys/ec/sign", Some(json!({"alg": "ES256", "value": digest}))).await;
    let verify = ok(&kv, "POST", "/keys/ec/verify", Some(json!({"alg": "ES256", "digest": digest, "value": sig["value"]}))).await;
    assert_eq!(verify["value"], true);
    let sig = ok(&kv, "POST", "/keys/imported/sign", Some(json!({"alg": "RS256", "value": digest}))).await;
    assert!(sig["value"].as_str().is_some_and(|v| !v.is_empty()));
    // the certificate, its secret (the PFX) and the contacts
    let cert = ok(&kv, "GET", "/certificates/web", None).await;
    assert!(cert["cer"].as_str().is_some_and(|c| !c.is_empty()), "{cert}");
    assert_eq!(cert["policy"]["x509_props"]["subject"], "CN=web.example.com");
    let pfx = ok(&kv, "GET", "/secrets/web", None).await;
    assert!(pfx["value"].as_str().is_some_and(|v| !v.is_empty()));
    let contacts = ok(&kv, "GET", "/certificates/contacts", None).await;
    assert_eq!(contacts["contacts"][0]["email"], "a@x.com");
    // another vault
    let r = kv.call_at(&kv.vault_base("other"), "GET", "/secrets/s", None).await;
    assert_eq!(r.json()["value"], "in the other vault");
    let _ = std::fs::remove_dir_all(&dir);
}
