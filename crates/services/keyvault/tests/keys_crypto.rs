//! The crypto operations, checked against known answers (made with Python's
//! `cryptography`, RFC 6979) and against independent RustCrypto code in the test.

mod common;

use common::*;
use rsa::traits::PublicKeyParts;
use rsa::{BoxedUint, PrivateKey, PublicKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha384, Sha512};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/rsa2048.json")).unwrap()
}

fn big(b64u: &str) -> BoxedUint {
    BoxedUint::from_be_slice_vartime(&unb64(b64u))
}

/// Import the fixture RSA key; returns the path `/keys/{name}/{version}`.
async fn import_fixture(kv: &Kv, name: &str, ops: Value) -> String {
    let f = fixture();
    let r = kv
        .call(
            "PUT",
            &format!("/keys/{name}"),
            Some(json!({"key": {"kty": "RSA", "n": f["n"], "e": f["e"], "d": f["d"], "p": f["p"], "q": f["q"],
                                "dp": f["dp"], "dq": f["dq"], "qi": f["qi"], "key_ops": ops}})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let kid = r.json()["key"]["kid"].as_str().unwrap().to_string();
    kid.strip_prefix(&kv.base).unwrap().to_string()
}

const ALL_OPS: [&str; 6] = ["encrypt", "decrypt", "sign", "verify", "wrapKey", "unwrapKey"];

#[tokio::test]
async fn rsa_known_answers() {
    let kv = Kv::new().await;
    let f = fixture();
    let key = import_fixture(&kv, "fx", json!(ALL_OPS)).await;

    // RS256 (PKCS#1 v1.5) is deterministic: the signature must equal the one Python made
    let r = kv.post(&format!("{key}/sign"), json!({"alg": "RS256", "value": f["digest"]})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["value"], f["rs256"]);
    assert_eq!(r.json()["kid"], format!("{}{key}", kv.base));

    // the service verifies signatures made elsewhere, including PSS with a digest-length salt
    for (alg, sig) in [("RS256", "rs256"), ("PS256", "ps256")] {
        let r = kv.post(&format!("{key}/verify"), json!({"alg": alg, "digest": f["digest"], "value": f[sig]})).await;
        assert_eq!(r.json(), json!({"value": true}), "{alg}");
    }
    // and rejects a signature of another scheme
    let r = kv.post(&format!("{key}/verify"), json!({"alg": "PS256", "digest": f["digest"], "value": f["rs256"]})).await;
    assert_eq!(r.json(), json!({"value": false}));

    // decrypt what Python encrypted
    for (alg, ct) in [("RSA-OAEP-256", "oaep256"), ("RSA-OAEP", "oaep"), ("RSA1_5", "rsa15")] {
        let r = kv.post(&format!("{key}/decrypt"), json!({"alg": alg, "value": f[ct]})).await;
        assert_eq!(r.status, 200, "{alg}: {}", r.text());
        assert_eq!(unb64(r.json()["value"].as_str().unwrap()), b"known plaintext", "{alg}");
        let r = kv.post(&format!("{key}/unwrapkey"), json!({"alg": alg, "value": f[ct]})).await;
        assert_eq!(unb64(r.json()["value"].as_str().unwrap()), b"known plaintext", "unwrap {alg}");
    }
}

#[tokio::test]
async fn rsa_service_output_is_valid_for_independent_code() {
    let kv = Kv::new().await;
    let f = fixture();
    let key = import_fixture(&kv, "fx", json!(ALL_OPS)).await;
    let public = PublicKey::new(big(f["n"].as_str().unwrap()), big(f["e"].as_str().unwrap())).unwrap();
    let private = PrivateKey::from_components(
        big(f["n"].as_str().unwrap()),
        big(f["e"].as_str().unwrap()),
        big(f["d"].as_str().unwrap()),
        vec![big(f["p"].as_str().unwrap()), big(f["q"].as_str().unwrap())],
    )
    .unwrap();

    // signatures: every RSA algorithm, checked with the public key
    macro_rules! check_sig {
        ($alg:expr, $hash:ty, $verify:ident) => {{
            let digest = <$hash>::digest(b"some message");
            let r = kv.post(&format!("{key}/sign"), json!({"alg": $alg, "value": b64(&digest)})).await;
            assert_eq!(r.status, 200, "{}: {}", $alg, r.text());
            let sig = unb64(r.json()["value"].as_str().unwrap());
            assert_eq!(sig.len(), 256);
            public.$verify::<$hash>(&digest, &sig).unwrap_or_else(|e| panic!("{} did not verify: {e}", $alg));
        }};
    }
    check_sig!("RS256", Sha256, pkcs1v15_verify_prehash);
    check_sig!("RS384", Sha384, pkcs1v15_verify_prehash);
    check_sig!("RS512", Sha512, pkcs1v15_verify_prehash);
    check_sig!("PS256", Sha256, pss_verify_prehash);
    check_sig!("PS384", Sha384, pss_verify_prehash);
    check_sig!("PS512", Sha512, pss_verify_prehash);

    // encrypt/wrap: the private key decrypts what the service made
    let pt = b"thirty-two bytes of key material";
    for alg in ["RSA-OAEP-256", "RSA-OAEP", "RSA1_5"] {
        for (op, back) in [("encrypt", "decrypt"), ("wrapkey", "unwrapkey")] {
            let r = kv.post(&format!("{key}/{op}"), json!({"alg": alg, "value": b64(pt)})).await;
            assert_eq!(r.status, 200, "{alg} {op}: {}", r.text());
            let ct = unb64(r.json()["value"].as_str().unwrap());
            assert_eq!(ct.len(), 256);
            let plain = match alg {
                "RSA-OAEP-256" => private.oaep_decrypt::<Sha256>(&ct),
                "RSA-OAEP" => private.oaep_decrypt::<sha1::Sha1>(&ct),
                _ => private.pkcs1v15_decrypt(&ct),
            }
            .unwrap();
            assert_eq!(plain, pt);
            // and the service decrypts its own output
            let r = kv.post(&format!("{key}/{back}"), json!({"alg": alg, "value": b64(&ct)})).await;
            assert_eq!(unb64(r.json()["value"].as_str().unwrap()), pt);
        }
    }
    // OAEP is randomised, PKCS#1 v1.5 signing is not
    let enc =
        || async { kv.post(&format!("{key}/encrypt"), json!({"alg": "RSA-OAEP", "value": b64(b"same")})).await.json()["value"].clone() };
    assert_ne!(enc().await, enc().await);

    // decrypting garbage is a client error, not a crash
    let r = kv.post(&format!("{key}/decrypt"), json!({"alg": "RSA-OAEP", "value": b64(&[7u8; 256])})).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"));
    let r = kv.post(&format!("{key}/decrypt"), json!({"alg": "RSA-OAEP", "value": b64(&[7u8; 10])})).await;
    assert_eq!(r.status, 400);
    // plaintext that is too long for the key
    let r = kv.post(&format!("{key}/encrypt"), json!({"alg": "RSA-OAEP", "value": b64(&[1u8; 250])})).await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn ec_rfc6979_known_answer_and_all_curves_verify_locally() {
    let kv = Kv::new().await;
    // RFC 6979 A.2.5: P-256, SHA-256, message "sample"
    let d = hex("c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721");
    let r = kv.call("PUT", "/keys/rfc", Some(json!({"key": {"kty": "EC", "crv": "P-256", "d": b64(&d)}}))).await;
    let kid = r.json()["key"]["kid"].as_str().unwrap().strip_prefix(&kv.base).unwrap().to_string();
    let digest = Sha256::digest(b"sample");
    let r = kv.post(&format!("{kid}/sign"), json!({"alg": "ES256", "value": b64(&digest)})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let sig = unb64(r.json()["value"].as_str().unwrap());
    assert_eq!(sig.len(), 64);
    let r_expected = hex("efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716");
    let s_expected = hex("f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8");
    assert_eq!(&sig[..32], r_expected.as_slice(), "r");
    assert_eq!(&sig[32..], s_expected.as_slice(), "s");
    let r = kv.post(&format!("{kid}/verify"), json!({"alg": "ES256", "digest": b64(&digest), "value": b64(&sig)})).await;
    assert_eq!(r.json(), json!({"value": true}));

    // every curve: the signature is raw r||s and verifies with independent code
    macro_rules! curve_case {
        ($crv:expr, $alg:expr, $hash:ty, $module:ident, $coord:expr) => {{
            use $module::ecdsa::signature::hazmat::PrehashVerifier;
            let r = kv.post(&format!("/keys/c-{}/create", $crv), json!({"kty": "EC", "crv": $crv})).await;
            let k = r.json();
            let kid = k["key"]["kid"].as_str().unwrap().strip_prefix(&kv.base).unwrap().to_string();
            let mut point = vec![4u8];
            point.extend(unb64(k["key"]["x"].as_str().unwrap()));
            point.extend(unb64(k["key"]["y"].as_str().unwrap()));
            let vk = $module::ecdsa::VerifyingKey::from_sec1_bytes(&point).expect("public point is on the curve");
            let digest = <$hash>::digest(b"curve message");
            let r = kv.post(&format!("{kid}/sign"), json!({"alg": $alg, "value": b64(&digest)})).await;
            assert_eq!(r.status, 200, "{}: {}", $crv, r.text());
            let sig = unb64(r.json()["value"].as_str().unwrap());
            assert_eq!(sig.len(), 2 * $coord, "{}", $crv);
            let parsed = $module::ecdsa::Signature::from_slice(&sig).unwrap();
            vk.verify_prehash(&digest, &parsed).unwrap_or_else(|e| panic!("{} signature did not verify: {e}", $crv));
            // verify through the service, with a tampered digest too
            let r = kv.post(&format!("{kid}/verify"), json!({"alg": $alg, "digest": b64(&digest), "value": b64(&sig)})).await;
            assert_eq!(r.json()["value"], true);
            let other = <$hash>::digest(b"other");
            let r = kv.post(&format!("{kid}/verify"), json!({"alg": $alg, "digest": b64(&other), "value": b64(&sig)})).await;
            assert_eq!(r.json()["value"], false);
        }};
    }
    curve_case!("P-256", "ES256", Sha256, p256, 32);
    curve_case!("P-384", "ES384", Sha384, p384, 48);
    curve_case!("P-521", "ES512", Sha512, p521, 66);
    curve_case!("P-256K", "ES256K", Sha256, k256, 32);
}

#[tokio::test]
async fn errors_for_wrong_algorithm_digest_and_key_state() {
    let kv = Kv::new().await;
    let rsa = import_fixture(&kv, "fx", json!(ALL_OPS)).await;
    let ec_key = kv.post("/keys/ec/create", json!({"kty": "EC"})).await.json();
    let ec = ec_key["key"]["kid"].as_str().unwrap().strip_prefix(&kv.base).unwrap().to_string();
    let d32 = b64(&Sha256::digest(b"x"));

    // algorithm does not fit the key type / curve
    for (path, alg) in [(&rsa, "ES256"), (&ec, "RS256"), (&ec, "PS256")] {
        let r = kv.post(&format!("{path}/sign"), json!({"alg": alg, "value": d32})).await;
        assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"), "{alg}");
    }
    let r = kv.post(&format!("{ec}/sign"), json!({"alg": "ES384", "value": b64(&Sha384::digest(b"x"))})).await;
    assert_eq!(r.status, 400, "ES384 needs a P-384 key");
    let r = kv.post(&format!("{rsa}/sign"), json!({"alg": "NOPE", "value": d32})).await;
    assert_eq!(r.status, 400);
    let r = kv.post(&format!("{rsa}/encrypt"), json!({"alg": "RS256", "value": d32})).await;
    assert_eq!(r.status, 400);
    let r = kv.post(&format!("{ec}/encrypt"), json!({"alg": "RSA-OAEP", "value": d32})).await;
    assert_eq!(r.status, 403, "EC keys do not list encrypt in key_ops");
    kv.call("PATCH", &ec, Some(json!({"key_ops": ["encrypt"]}))).await;
    let r = kv.post(&format!("{ec}/encrypt"), json!({"alg": "RSA-OAEP", "value": d32})).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"), "EC keys cannot encrypt");
    // digest length must match the algorithm
    let r = kv.post(&format!("{rsa}/sign"), json!({"alg": "RS256", "value": b64(&[0u8; 20])})).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"));
    // body problems
    assert_eq!(kv.post(&format!("{rsa}/sign"), json!({"value": d32})).await.status, 400);
    assert_eq!(kv.post(&format!("{rsa}/sign"), json!({"alg": "RS256", "value": "***"})).await.status, 400);

    // key not found, version not found
    let r = kv.post(&format!("/keys/missing/{}/sign", "0".repeat(32)), json!({"alg": "RS256", "value": d32})).await;
    assert_eq!((r.status, code(&r).as_str()), (404, "KeyNotFound"));
    let r = kv.post(&format!("/keys/fx/{}/sign", "0".repeat(32)), json!({"alg": "RS256", "value": d32})).await;
    assert_eq!(r.status, 404);
    // a missing version means the latest one
    let r = kv.post("/keys/fx/sign", json!({"alg": "RS256", "value": d32})).await;
    assert_eq!(r.status, 200);
    let r = kv.post("/keys/fx//sign", json!({"alg": "RS256", "value": d32})).await;
    assert_eq!(r.status, 200);

    // key_ops: an operation that is not listed is forbidden
    kv.call("PATCH", "/keys/fx/", Some(json!({"key_ops": ["sign", "verify"]}))).await;
    for op in ["encrypt", "decrypt", "wrapkey", "unwrapkey"] {
        let r = kv.post(&format!("{rsa}/{op}"), json!({"alg": "RSA-OAEP", "value": d32})).await;
        assert_eq!((r.status, code(&r).as_str(), inner_code(&r).as_str()), (403, "Forbidden", "KeyOperationForbidden"), "{op}");
    }
    assert_eq!(kv.post(&format!("{rsa}/sign"), json!({"alg": "RS256", "value": d32})).await.status, 200);

    // disabled key
    kv.call("PATCH", "/keys/fx/", Some(json!({"attributes": {"enabled": false}}))).await;
    let r = kv.post(&format!("{rsa}/sign"), json!({"alg": "RS256", "value": d32})).await;
    assert_eq!((r.status, code(&r).as_str(), inner_code(&r).as_str()), (403, "Forbidden", "KeyDisabled"));
    kv.call("PATCH", "/keys/fx/", Some(json!({"attributes": {"enabled": true}}))).await;
    assert_eq!(kv.post(&format!("{rsa}/sign"), json!({"alg": "RS256", "value": d32})).await.status, 200);

    // not before / expiry
    kv.call("PATCH", "/keys/fx/", Some(json!({"attributes": {"nbf": 4102444800i64}}))).await;
    assert_eq!(kv.post(&format!("{rsa}/sign"), json!({"alg": "RS256", "value": d32})).await.status, 403);
    kv.call("PATCH", "/keys/fx/", Some(json!({"attributes": {"nbf": 1, "exp": 100}}))).await;
    assert_eq!(kv.post(&format!("{rsa}/sign"), json!({"alg": "RS256", "value": d32})).await.status, 403);
    kv.call("PATCH", "/keys/fx/", Some(json!({"attributes": {"exp": 4102444800i64}}))).await;
    assert_eq!(kv.post(&format!("{rsa}/sign"), json!({"alg": "RS256", "value": d32})).await.status, 200);

    // deleted key
    kv.call("DELETE", "/keys/fx", None).await;
    let r = kv.post(&format!("{rsa}/sign"), json!({"alg": "RS256", "value": d32})).await;
    assert_eq!((r.status, code(&r).as_str()), (404, "KeyNotFound"));
}

#[tokio::test]
async fn private_key_survives_recover_backup_restore_and_each_version_has_its_own() {
    let kv = Kv::new().await;
    let k1 = kv.post("/keys/k/create", json!({"kty": "EC"})).await.json();
    let k2 = kv.post("/keys/k/create", json!({"kty": "EC"})).await.json();
    let path = |k: &Value| k["key"]["kid"].as_str().unwrap().strip_prefix(&kv.base).unwrap().to_string();
    let digest = b64(&Sha256::digest(b"m"));
    let sign = |p: String| {
        let kv = &kv;
        let digest = digest.clone();
        async move {
            let r = kv.post(&format!("{p}/sign"), json!({"alg": "ES256", "value": digest})).await;
            unb64(r.json()["value"].as_str().unwrap())
        }
    };
    let verify_with = |k: &Value, sig: &[u8]| {
        use p256::ecdsa::signature::hazmat::PrehashVerifier;
        let mut point = vec![4u8];
        point.extend(unb64(k["key"]["x"].as_str().unwrap()));
        point.extend(unb64(k["key"]["y"].as_str().unwrap()));
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&point).unwrap();
        vk.verify_prehash(&Sha256::digest(b"m"), &p256::ecdsa::Signature::from_slice(sig).unwrap()).is_ok()
    };
    let s1 = sign(path(&k1)).await;
    assert!(verify_with(&k1, &s1));
    assert!(!verify_with(&k2, &s1), "version 1 must not sign with version 2's key");

    let blob = kv.call("POST", "/keys/k/backup", None).await.json()["value"].as_str().unwrap().to_string();
    kv.call("DELETE", "/keys/k", None).await;
    kv.call("POST", "/deletedkeys/k/recover", None).await;
    assert!(verify_with(&k2, &sign(path(&k2)).await), "after recover");
    kv.call("DELETE", "/keys/k", None).await;
    kv.call("DELETE", "/deletedkeys/k", None).await;
    assert_eq!(kv.post("/keys/restore", json!({"value": blob})).await.status, 200);
    assert!(verify_with(&k1, &sign(path(&k1)).await), "after restore");
    assert!(verify_with(&k2, &sign(path(&k2)).await), "after restore");
}

#[tokio::test]
async fn generated_rsa_key_works_for_every_operation() {
    let kv = Kv::new().await;
    let k = kv.post("/keys/gen/create", json!({"kty": "RSA", "key_size": 2048})).await.json();
    let kid = k["key"]["kid"].as_str().unwrap().strip_prefix(&kv.base).unwrap().to_string();
    let public = PublicKey::new(big(k["key"]["n"].as_str().unwrap()), big(k["key"]["e"].as_str().unwrap())).unwrap();
    assert_eq!(public.size(), 256);
    let ct = public.oaep_encrypt::<Sha256>(b"hello generated key").unwrap();
    let r = kv.post(&format!("{kid}/decrypt"), json!({"alg": "RSA-OAEP-256", "value": b64(&ct)})).await;
    assert_eq!(unb64(r.json()["value"].as_str().unwrap()), b"hello generated key");
    let digest = Sha256::digest(b"gen");
    let r = kv.post(&format!("{kid}/sign"), json!({"alg": "PS256", "value": b64(&digest)})).await;
    public.pss_verify_prehash::<Sha256>(&digest, &unb64(r.json()["value"].as_str().unwrap())).unwrap();
}
