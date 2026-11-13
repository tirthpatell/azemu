//! Keys: creation, JWK shape, import, update, versions, delete/recover/purge, backup,
//! rotation and rotation policy.

mod common;

use std::time::Instant;

use common::*;
use serde_json::{json, Value};

fn kid_version(key: &Value) -> String {
    key["key"]["kid"].as_str().unwrap().rsplit('/').next().unwrap().to_string()
}

#[tokio::test]
async fn create_rsa_keys_of_every_size_and_time_them() {
    let kv = Kv::new().await;
    for (bits, limit_secs) in [(2048usize, 5.0f64), (3072, 15.0), (4096, 30.0)] {
        let t = Instant::now();
        let r = kv.post(&format!("/keys/rsa{bits}/create"), json!({"kty": "RSA", "key_size": bits})).await;
        let secs = t.elapsed().as_secs_f64();
        println!("RSA {bits} key generation: {secs:.2}s");
        assert_eq!(r.status, 200, "{}", r.text());
        assert!(secs < limit_secs, "RSA {bits} took {secs}s");
        let k = r.json();
        let n = unb64(k["key"]["n"].as_str().unwrap());
        assert_eq!(n.len() * 8, bits);
        assert_eq!(k["key"]["e"], "AQAB");
    }
}

#[tokio::test]
async fn rsa_jwk_has_exactly_the_public_fields() {
    let kv = Kv::new().await;
    let r = kv.post("/keys/jwk/create", json!({"kty": "RSA"})).await;
    assert_eq!(r.status, 200);
    let k = r.json();
    let jwk = k["key"].as_object().unwrap();
    let mut names: Vec<&str> = jwk.keys().map(|s| s.as_str()).collect();
    names.sort();
    assert_eq!(names, ["e", "key_ops", "kid", "kty", "n"]);
    assert_eq!(jwk["kty"], "RSA");
    assert_eq!(jwk["key_ops"], json!(["encrypt", "decrypt", "sign", "verify", "wrapKey", "unwrapKey"]));
    let version = kid_version(&k);
    assert_eq!(jwk["kid"], format!("{}/keys/jwk/{version}", kv.base));
    assert_eq!(version.len(), 32);
    let a = &k["attributes"];
    assert_eq!(a["enabled"], true);
    assert_eq!(a["recoveryLevel"], "Recoverable+Purgeable");
    assert_eq!(a["exportable"], false);
    for private in ["d", "p", "q", "dp", "dq", "qi"] {
        assert!(jwk.get(private).is_none());
    }
}

#[tokio::test]
async fn ec_keys_for_every_curve_have_the_right_coordinate_size() {
    let kv = Kv::new().await;
    for (crv, len) in [("P-256", 32), ("P-384", 48), ("P-521", 66), ("P-256K", 32)] {
        let r = kv.post(&format!("/keys/ec-{crv}/create"), json!({"kty": "EC", "crv": crv})).await;
        assert_eq!(r.status, 200, "{crv}: {}", r.text());
        let k = r.json();
        let jwk = &k["key"];
        assert_eq!(jwk["kty"], "EC");
        assert_eq!(jwk["crv"], crv);
        assert_eq!(unb64(jwk["x"].as_str().unwrap()).len(), len);
        assert_eq!(unb64(jwk["y"].as_str().unwrap()).len(), len);
        assert_eq!(jwk["key_ops"], json!(["sign", "verify"]));
        assert!(jwk.get("d").is_none());
    }
    // default curve
    let k = kv.post("/keys/ec-default/create", json!({"kty": "EC"})).await.json();
    assert_eq!(k["key"]["crv"], "P-256");
}

#[tokio::test]
async fn create_validates_the_request() {
    let kv = Kv::new().await;
    let r = kv.post("/keys/k/create", json!({"kty": "RSA", "key_size": 1000})).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"));
    assert_eq!(kv.post("/keys/k/create", json!({"kty": "EC", "crv": "P-999"})).await.status, 400);
    assert_eq!(kv.post("/keys/k/create", json!({"kty": "DSA"})).await.status, 400);
    assert_eq!(kv.post("/keys/k/create", json!({})).await.status, 400);
    assert_eq!(kv.post("/keys/k/create", json!({"kty": "EC", "key_ops": ["fly"]})).await.status, 400);
    for bad in ["under_score", "dot.ted", "sp%20ace"] {
        let r = kv.post(&format!("/keys/{bad}/create"), json!({"kty": "EC"})).await;
        assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"), "{bad}");
    }
    let r = kv
        .post(
            "/keys/meta/create",
            json!({"kty": "EC", "key_ops": ["sign"], "tags": {"a": "b"}, "attributes": {"enabled": false, "nbf": 1, "exp": 4102444800i64}}),
        )
        .await;
    let k = r.json();
    assert_eq!(k["key"]["key_ops"], json!(["sign"]));
    assert_eq!(k["tags"]["a"], "b");
    assert_eq!(k["attributes"]["enabled"], false);
    assert_eq!(k["attributes"]["nbf"], 1);
    assert_eq!(k["attributes"]["exp"], 4102444800i64);
}

#[tokio::test]
async fn get_list_versions_and_update() {
    let kv = Kv::new().await;
    let v1 = kv.post("/keys/k/create", json!({"kty": "EC", "tags": {"t": "1"}})).await.json();
    let v2 = kv.post("/keys/k/create", json!({"kty": "EC"})).await.json();
    assert_ne!(kid_version(&v1), kid_version(&v2));
    kv.post("/keys/other/create", json!({"kty": "EC"})).await;

    for p in ["/keys/k", "/keys/k/"] {
        assert_eq!(kv.get(p).await.json()["key"]["kid"], v2["key"]["kid"]);
    }
    assert_eq!(kv.get(&format!("/keys/k/{}", kid_version(&v1))).await.json()["key"]["x"], v1["key"]["x"]);
    let r = kv.get(&format!("/keys/k/{}", "f".repeat(32))).await;
    assert_eq!((r.status, code(&r).as_str()), (404, "KeyNotFound"));
    let r = kv.get("/keys/nokey").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "KeyNotFound"));
    assert!(r.json()["error"]["message"].as_str().unwrap().starts_with("A key with (name/id) nokey was not found in this key vault."));

    // list: one entry per key, kid without a version, no key material
    let list = kv.get("/keys").await.json();
    let items = list["value"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["kid"], format!("{}/keys/k", kv.base));
    assert!(items[0].get("key").is_none());
    assert_eq!(items[0]["tags"], Value::Null, "latest version has no tags");
    // versions list: kid with version, sorted by version id
    let versions = kv.get("/keys/k/versions").await.json();
    let kids: Vec<&str> = versions["value"].as_array().unwrap().iter().map(|v| v["kid"].as_str().unwrap()).collect();
    assert_eq!(kids.len(), 2);
    assert!(kids[0] < kids[1]);
    assert_eq!(kv.get("/keys/none/versions").await.json()["value"].as_array().unwrap().len(), 0);

    // update: key_ops, tags and attributes of one version; the key itself is untouched
    let r = kv
        .call(
            "PATCH",
            &format!("/keys/k/{}", kid_version(&v1)),
            Some(json!({"key_ops": ["verify"], "tags": {"x": "y"}, "attributes": {"enabled": false}})),
        )
        .await;
    assert_eq!(r.status, 200);
    let u = r.json();
    assert_eq!(u["key"]["key_ops"], json!(["verify"]));
    assert_eq!(u["key"]["x"], v1["key"]["x"]);
    assert_eq!(u["attributes"]["enabled"], false);
    assert_eq!(u["attributes"]["created"], v1["attributes"]["created"]);
    assert_eq!(kv.get("/keys/k").await.json()["attributes"]["enabled"], true, "latest untouched");
    let r = kv.call("PATCH", "/keys/missing/", Some(json!({"tags": {}}))).await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn pages_of_keys() {
    let kv = Kv::new().await;
    for i in 0..27 {
        kv.post(&format!("/keys/k{i:02}/create"), json!({"kty": "EC"})).await;
    }
    let p1 = kv.get("/keys").await.json();
    assert_eq!(p1["value"].as_array().unwrap().len(), 25);
    let next = p1["nextLink"].as_str().unwrap();
    assert!(next.starts_with(&format!("{}/keys?api-version=7.6&$skiptoken=", kv.base)), "{next}");
    let p2 = kv.srv.request("GET", next).header("Authorization", "Bearer t").send().await.json();
    assert_eq!(p2["value"].as_array().unwrap().len(), 2);
    assert!(p2["nextLink"].is_null());
}

#[tokio::test]
async fn import_rsa_and_ec_returns_only_public_parts() {
    let kv = Kv::new().await;
    let f: Value = serde_json::from_str(include_str!("fixtures/rsa2048.json")).unwrap();
    // a leading zero byte in n is stripped by the service
    let mut n = unb64(f["n"].as_str().unwrap());
    n.insert(0, 0);
    let r = kv
        .call(
            "PUT",
            "/keys/imported",
            Some(json!({"key": {"kty": "RSA", "n": b64(&n), "e": f["e"], "d": f["d"], "p": f["p"], "q": f["q"],
                                "dp": f["dp"], "dq": f["dq"], "qi": f["qi"], "key_ops": ["sign", "verify"]},
                        "tags": {"src": "local"}})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let k = r.json();
    assert_eq!(k["key"]["n"], f["n"]);
    assert_eq!(k["key"]["e"], f["e"]);
    assert_eq!(k["key"]["key_ops"], json!(["sign", "verify"]));
    for private in ["d", "p", "q", "dp", "dq", "qi"] {
        assert!(k["key"].get(private).is_none(), "{private} must not be returned");
    }
    assert_eq!(k["tags"]["src"], "local");

    // EC import: the public point is derived from d
    let d = hex("c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721");
    let r = kv.call("PUT", "/keys/imported-ec", Some(json!({"key": {"kty": "EC", "crv": "P-256", "d": b64(&d)}}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let k = r.json();
    assert_eq!(unb64(k["key"]["x"].as_str().unwrap()), hex("60fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6"));
    assert_eq!(unb64(k["key"]["y"].as_str().unwrap()), hex("7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299"));

    // bad imports
    let r = kv.call("PUT", "/keys/bad", Some(json!({"key": {"kty": "RSA", "key_ops": ["sign"]}}))).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"));
    let r = kv.call("PUT", "/keys/bad", Some(json!({"key": {"kty": "RSA", "n": f["n"], "e": f["e"]}}))).await;
    assert_eq!(r.status, 400, "public-only RSA import is refused");
    let r = kv.call("PUT", "/keys/bad", Some(json!({"key": {"kty": "EC", "crv": "P-256", "d": b64(&[0u8; 32])}}))).await;
    assert_eq!(r.status, 400, "a zero scalar is not a key");
    let r = kv.call("PUT", "/keys/bad", Some(json!({"key": {"kty": "EC", "crv": "P-384", "d": b64(&d)}}))).await;
    assert_eq!(r.status, 200, "a short scalar is left-padded");
}

#[tokio::test]
async fn key_soft_delete_recover_purge() {
    let kv = Kv::new().await;
    let created = kv.post("/keys/k/create", json!({"kty": "EC", "tags": {"a": "b"}})).await.json();
    kv.post("/keys/k/create", json!({"kty": "EC"})).await;

    let r = kv.call("DELETE", "/keys/k", None).await;
    assert_eq!(r.status, 200);
    let d = r.json();
    assert_eq!(d["recoveryId"], format!("{}/deletedkeys/k", kv.base));
    assert!(d["key"]["kid"].as_str().unwrap().starts_with(&format!("{}/keys/k/", kv.base)));
    assert_eq!(d["scheduledPurgeDate"].as_i64().unwrap() - d["deletedDate"].as_i64().unwrap(), 90 * 86400);

    assert_eq!(code(&kv.get("/keys/k").await), "KeyNotFound");
    assert_eq!(kv.get("/keys").await.json()["value"].as_array().unwrap().len(), 0);
    let r = kv.post("/keys/k/create", json!({"kty": "EC"})).await;
    assert_eq!((r.status, code(&r).as_str(), inner_code(&r).as_str()), (409, "Conflict", "ObjectIsDeletedButRecoverable"));
    let r = kv.call("PUT", "/keys/k", Some(json!({"key": {"kty": "EC", "crv": "P-256", "d": b64(&[1u8; 32])}}))).await;
    assert_eq!(r.status, 409);

    let r = kv.get("/deletedkeys/k").await;
    assert_eq!(r.status, 200);
    assert!(r.json()["key"]["x"].is_string());
    let list = kv.get("/deletedkeys").await.json();
    assert_eq!(list["value"][0]["kid"], format!("{}/keys/k", kv.base));
    assert_eq!(list["value"][0]["recoveryId"], format!("{}/deletedkeys/k", kv.base));

    let r = kv.call("POST", "/deletedkeys/k/recover", None).await;
    assert_eq!(r.status, 200);
    assert_eq!(kv.get("/keys/k/versions").await.json()["value"].as_array().unwrap().len(), 2);
    assert_eq!(kv.get(&format!("/keys/k/{}", kid_version(&created))).await.json()["tags"]["a"], "b");

    let r = kv.call("DELETE", "/deletedkeys/k", None).await;
    assert_eq!((r.status, r.json()["error"]["message"].as_str().unwrap()), (404, "Deleted Key not found: k"));
    kv.call("DELETE", "/keys/k", None).await;
    assert_eq!(kv.call("DELETE", "/deletedkeys/k", None).await.status, 204);
    assert_eq!(kv.get("/deletedkeys/k").await.status, 404);
    assert_eq!(kv.call("POST", "/deletedkeys/k/recover", None).await.status, 404);
    assert_eq!(kv.call("DELETE", "/keys/k", None).await.status, 404);
}

#[tokio::test]
async fn backup_restore_keeps_ids_and_metadata() {
    let kv = Kv::new().await;
    let v1 = kv.post("/keys/b/create", json!({"kty": "RSA", "key_ops": ["sign", "verify"], "tags": {"t": "1"}})).await.json();
    let v2 = kv.post("/keys/b/create", json!({"kty": "EC"})).await.json();
    let blob = kv.call("POST", "/keys/b/backup", None).await.json()["value"].as_str().unwrap().to_string();

    let r = kv.post("/keys/restore", json!({"value": blob})).await;
    assert_eq!(r.status, 409);
    assert_eq!(code(&r), "Conflict");
    assert!(r.json()["error"]["message"].as_str().unwrap().starts_with("There was a conflict restoring the key '"));

    kv.call("DELETE", "/keys/b", None).await;
    kv.call("DELETE", "/deletedkeys/b", None).await;
    let r = kv.post("/keys/restore", json!({"value": blob})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["key"]["kid"], v2["key"]["kid"], "the restored bundle is the latest version");
    let again = kv.get(&format!("/keys/b/{}", kid_version(&v1))).await.json();
    assert_eq!(again["key"]["n"], v1["key"]["n"]);
    assert_eq!(again["key"]["key_ops"], json!(["sign", "verify"]));
    assert_eq!(again["tags"]["t"], "1");
    assert_eq!(again["attributes"]["created"], v1["attributes"]["created"]);

    assert_eq!(kv.post("/keys/restore", json!({"value": "bm90IGEgYmFja3Vw"})).await.status, 400);
    // a secret backup is not a key backup
    kv.call("PUT", "/secrets/s", Some(json!({"value": "v"}))).await;
    let sblob = kv.call("POST", "/secrets/s/backup", None).await.json()["value"].as_str().unwrap().to_string();
    assert_eq!(kv.post("/keys/restore", json!({"value": sblob})).await.status, 400);
    assert_eq!(kv.call("POST", "/keys/nokey/backup", None).await.status, 404);
}

#[tokio::test]
async fn rotate_makes_a_new_version_of_the_same_kind() {
    let kv = Kv::new().await;
    let v1 = kv.post("/keys/r/create", json!({"kty": "EC", "crv": "P-384", "key_ops": ["sign"], "tags": {"a": "b"}})).await.json();
    let r = kv.call("POST", "/keys/r/rotate", None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v2 = r.json();
    assert_ne!(kid_version(&v1), kid_version(&v2));
    assert_eq!(v2["key"]["crv"], "P-384");
    assert_ne!(v2["key"]["x"], v1["key"]["x"]);
    assert_eq!(v2["key"]["key_ops"], json!(["sign"]));
    assert_eq!(v2["tags"]["a"], "b");
    assert_eq!(kv.get("/keys/r").await.json()["key"]["kid"], v2["key"]["kid"]);
    assert_eq!(kv.get("/keys/r/versions").await.json()["value"].as_array().unwrap().len(), 2);
    let rsa = kv.post("/keys/rr/create", json!({"kty": "RSA", "key_size": 3072})).await.json();
    let rotated = kv.call("POST", "/keys/rr/rotate", None).await.json();
    assert_eq!(unb64(rotated["key"]["n"].as_str().unwrap()).len(), unb64(rsa["key"]["n"].as_str().unwrap()).len());
    assert_eq!(kv.call("POST", "/keys/none/rotate", None).await.status, 404);
}

#[tokio::test]
async fn rotation_policy_defaults_and_merge_rule() {
    let kv = Kv::new().await;
    kv.post("/keys/p/create", json!({"kty": "EC"})).await;
    let p = kv.get("/keys/p/rotationpolicy").await.json();
    assert_eq!(p["id"], format!("{}/keys/p/rotationpolicy", kv.base));
    assert_eq!(p["lifetimeActions"], json!([{"trigger": {"timeBeforeExpiry": "P30D"}, "action": {"type": "Notify"}}]));
    assert!(p["attributes"]["created"].is_i64() && p["attributes"]["updated"].is_i64());
    assert!(p["attributes"].get("expiryTime").is_none());

    // sending only a Rotate action keeps the Notify one
    let put = |body: Value| {
        let kv = &kv;
        async move { kv.call("PUT", "/keys/p/rotationpolicy", Some(body)).await }
    };
    let r =
        put(json!({"lifetimeActions": [{"action": {"type": "Rotate"}, "trigger": {"timeAfterCreate": "P2M"}}], "attributes": {}})).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.json()["lifetimeActions"],
        json!([{"trigger": {"timeAfterCreate": "P2M"}, "action": {"type": "Rotate"}},
               {"trigger": {"timeBeforeExpiry": "P30D"}, "action": {"type": "Notify"}}])
    );
    // sending only a Notify action drops the Rotate one and replaces the old Notify
    let r = put(json!({"lifetimeActions": [{"action": {"type": "Notify"}, "trigger": {"timeBeforeExpiry": "P60D"}}],
                       "attributes": {"expiryTime": "P90D"}}))
    .await;
    let j = r.json();
    assert_eq!(j["lifetimeActions"], json!([{"trigger": {"timeBeforeExpiry": "P60D"}, "action": {"type": "Notify"}}]));
    assert_eq!(j["attributes"]["expiryTime"], "P90D");
    assert_eq!(kv.get("/keys/p/rotationpolicy").await.json()["attributes"]["expiryTime"], "P90D");
    assert_eq!(kv.get("/keys/none/rotationpolicy").await.status, 404);
    let r = put(json!({"lifetimeActions": [{"action": {"type": "Explode"}, "trigger": {"timeAfterCreate": "P1D"}}]})).await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn random_bytes_release_and_other_unbuilt_operations() {
    let kv = Kv::new().await;
    let r = kv.post("/rng", json!({"count": 48})).await;
    assert_eq!(r.status, 200);
    assert_eq!(unb64(r.json()["value"].as_str().unwrap()).len(), 48);
    assert_eq!(kv.post("/rng", json!({"count": 0})).await.status, 400);
    kv.post("/keys/k/create", json!({"kty": "EC"})).await;
    let r = kv.post("/keys/k/release", json!({"target": "x"})).await;
    assert_eq!(r.status, 501);
    assert_eq!(code(&r), "NotImplemented");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("release"));
    assert_eq!(kv.get("/keys/k/abcdef/attestation").await.status, 501);
}
