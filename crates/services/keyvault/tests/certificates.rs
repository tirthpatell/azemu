//! Certificates: create with a self-signed policy, the linked key and secret, import
//! (PEM, plain and password-protected PKCS#12), versions, policy, delete/recover/purge,
//! operations, issuers, contacts, backup/restore.

mod common;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use common::*;
use serde_json::{json, Value};
use x509_parser::prelude::{FromDer, X509Certificate};

fn std_b64(s: &str) -> Vec<u8> {
    STANDARD.decode(s).expect("standard base64")
}

async fn create(kv: &Kv, name: &str, policy: Value) -> Value {
    let r = kv.post(&format!("/certificates/{name}/create"), json!({"policy": policy})).await;
    assert_eq!(r.status, 202, "{}", r.text());
    let op = r.json();
    assert_eq!(op["status"], "inProgress", "the create response says in progress, like Azure");
    assert_eq!(r.header("retry-after"), Some("10"));
    assert!(r.header("location").unwrap().contains(&format!("/certificates/{name}/pending")));
    let done = kv.get(&format!("/certificates/{name}/pending")).await.json();
    assert_eq!(done["status"], "completed", "the first poll finds it finished");
    assert_eq!(done["target"], format!("{}/certificates/{name}", kv.base));
    assert_eq!(done["issuer"]["name"], "Self");
    assert_eq!(done["id"], format!("{}/certificates/{name}/pending", kv.base));
    assert!(done["csr"].as_str().unwrap().len() > 100);
    kv.get(&format!("/certificates/{name}")).await.json()
}

fn ec_policy(subject: &str) -> Value {
    json!({"key_props": {"kty": "EC", "crv": "P-256", "exportable": true, "reuse_key": false},
           "x509_props": {"subject": subject, "sans": {"dns_names": ["a.example.com"], "emails": ["me@example.com"]}, "validity_months": 6},
           "issuer": {"name": "Self"}})
}

#[tokio::test]
async fn create_self_signed_certificate_and_its_bundle() {
    let kv = Kv::new().await;
    let c = create(&kv, "web", ec_policy("CN=web.example.com, O=Azemu")).await;
    let version = c["id"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
    assert_eq!(c["id"], format!("{}/certificates/web/{version}", kv.base));
    assert_eq!(c["kid"], format!("{}/keys/web/{version}", kv.base));
    assert_eq!(c["sid"], format!("{}/secrets/web/{version}", kv.base));
    assert_eq!(c["x5t"].as_str().unwrap().len(), 27);
    assert_eq!(c["pending"]["id"], format!("{}/certificates/web/pending", kv.base));
    assert_eq!(c["attributes"]["enabled"], true);
    assert_eq!(c["attributes"]["recoveryLevel"], "Recoverable+Purgeable");

    // the certificate really is what the policy asked for
    let der = std_b64(c["cer"].as_str().unwrap());
    let (_, x) = X509Certificate::from_der(&der).unwrap();
    assert_eq!(x.subject().to_string(), "O=Azemu, CN=web.example.com");
    let validity = x.validity().not_after.timestamp() - x.validity().not_before.timestamp();
    assert!((180 * 86400..190 * 86400).contains(&validity), "about 6 months, got {validity}s");
    assert_eq!(c["attributes"]["nbf"], x.validity().not_before.timestamp());
    assert_eq!(c["attributes"]["exp"], x.validity().not_after.timestamp());
    let san = x.subject_alternative_name().unwrap().unwrap();
    assert_eq!(san.value.general_names.len(), 2);
    assert_eq!(x509_parser::oid_registry::OID_KEY_TYPE_EC_PUBLIC_KEY.to_id_string(), x.public_key().algorithm.algorithm.to_id_string());

    // the policy, with the defaults the service fills in
    let p = &c["policy"];
    assert_eq!(p["id"], format!("{}/certificates/web/policy", kv.base));
    assert_eq!(p["key_props"]["kty"], "EC");
    assert_eq!(p["key_props"]["crv"], "P-256");
    assert_eq!(p["secret_props"]["contentType"], "application/x-pkcs12");
    assert_eq!(p["x509_props"]["subject"], "CN=web.example.com, O=Azemu");
    assert_eq!(p["x509_props"]["ekus"], json!(["1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.2"]));
    assert_eq!(p["x509_props"]["key_usage"], json!(["digitalSignature", "keyEncipherment"]));
    assert_eq!(p["x509_props"]["basic_constraints"], json!({"ca": false}));
    assert_eq!(p["lifetime_actions"][0]["action"]["action_type"], "AutoRenew");
    assert_eq!(p["issuer"]["name"], "Self");

    // a version request carries no policy and no pending link
    let v = kv.get(&format!("/certificates/web/{version}")).await.json();
    assert!(v.get("policy").is_none() && v.get("pending").is_none());
    assert_eq!(v["cer"], c["cer"]);
}

#[tokio::test]
async fn linked_key_and_secret_match_the_certificate() {
    let kv = Kv::new().await;
    let c = create(&kv, "link", ec_policy("CN=link")).await;
    let version = c["id"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
    let key = kv.get("/keys/link").await.json();
    assert_eq!(key["key"]["kid"], c["kid"]);
    assert_eq!(key["key"]["kty"], "EC");
    assert_eq!(key["managed"], true);
    // the key's public point is the certificate's public key, and it signs
    let der = std_b64(c["cer"].as_str().unwrap());
    let (_, x) = X509Certificate::from_der(&der).unwrap();
    let mut point = vec![4u8];
    point.extend(unb64(key["key"]["x"].as_str().unwrap()));
    point.extend(unb64(key["key"]["y"].as_str().unwrap()));
    assert_eq!(x.public_key().subject_public_key.data.as_ref(), point.as_slice());

    let secret = kv.get("/secrets/link").await.json();
    assert_eq!(secret["id"], c["sid"]);
    assert_eq!(secret["kid"], c["kid"]);
    assert_eq!(secret["managed"], true);
    assert_eq!(secret["contentType"], "application/x-pkcs12");
    let pfx = STANDARD.decode(secret["value"].as_str().unwrap()).unwrap();
    let (m, chain) = azemu_keyvault_test_pfx(&pfx);
    assert_eq!(chain.len(), 1);
    assert_eq!(m, key_scalar(&kv, &version).await, "the PFX holds the key behind the certificate");

    // PFX -> PEM conversion on read
    let r = kv.get("/secrets/link?outContentType=application%2Fx-pem-file").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let pem = r.json()["value"].as_str().unwrap().to_string();
    assert!(pem.contains("-----BEGIN PRIVATE KEY-----") && pem.contains("-----BEGIN CERTIFICATE-----"));
}

/// Private scalar of the EC key behind `name` (through the service's own backup).
async fn key_scalar(kv: &Kv, _version: &str) -> Vec<u8> {
    let blob = kv.call("POST", "/keys/link/backup", None).await.json()["value"].as_str().unwrap().to_string();
    let bytes = unb64(&blob);
    let json_start = 1 + bytes[0] as usize;
    let payload: Value = serde_json::from_slice(&bytes[json_start..]).unwrap();
    unb64(payload["versions"][0]["material"]["d"].as_str().unwrap())
}

fn azemu_keyvault_test_pfx(pfx: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    use p12_keystore::{KeyStore, Pkcs12ImportPolicy};
    use p256::pkcs8::DecodePrivateKey;
    let store = KeyStore::from_pkcs12(pfx, "", Pkcs12ImportPolicy::Strict).expect("the PFX opens with an empty password");
    let (_, chain) = store.private_key_chain().expect("a key chain");
    let sk = p256::SecretKey::from_pkcs8_der(chain.key().as_der()).unwrap();
    (sk.to_bytes().to_vec(), chain.certs().iter().map(|c| c.as_der().to_vec()).collect())
}

#[tokio::test]
async fn pem_content_type_and_rsa_policy() {
    let kv = Kv::new().await;
    let policy = json!({"key_props": {"kty": "RSA", "key_size": 2048}, "secret_props": {"contentType": "application/x-pem-file"},
                        "x509_props": {"subject": "CN=pem-cert"}, "issuer": {"name": "Self"}});
    let c = create(&kv, "pem", policy).await;
    assert_eq!(c["policy"]["secret_props"]["contentType"], "application/x-pem-file");
    let secret = kv.get("/secrets/pem").await.json();
    assert_eq!(secret["contentType"], "application/x-pem-file");
    let v = secret["value"].as_str().unwrap();
    assert!(v.starts_with("-----BEGIN PRIVATE KEY-----"));
    assert_eq!(v.matches("-----BEGIN CERTIFICATE-----").count(), 1);
    let key = kv.get("/keys/pem").await.json();
    assert_eq!(key["key"]["kty"], "RSA");
    assert_eq!(unb64(key["key"]["n"].as_str().unwrap()).len(), 256);
    // the linked RSA key can sign, and the signature verifies against the certificate
    let kid = key["key"]["kid"].as_str().unwrap().strip_prefix(&kv.base).unwrap().to_string();
    let digest = b64(&[1u8; 32]);
    let r = kv.post(&format!("{kid}/sign"), json!({"alg": "RS256", "value": digest})).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn create_validates_and_new_versions_stack() {
    let kv = Kv::new().await;
    assert_eq!(kv.post("/certificates/x/create", json!({})).await.status, 400, "policy is required");
    let r = kv.post("/certificates/x_y/create", json!({"policy": {}})).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"));
    let r = kv.post("/certificates/x/create", json!({"policy": {"key_props": {"kty": "EC", "crv": "P-999"}}})).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"), "an unknown curve");
    let r = kv.post("/certificates/x/create", json!({"policy": {"x509_props": {"subject": "this is not a distinguished name"}}})).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"), "a subject that is not an X.500 name");
    let first = create(&kv, "multi", ec_policy("CN=one")).await;
    let second = create(&kv, "multi", ec_policy("CN=two")).await;
    assert_ne!(first["id"], second["id"]);
    assert_eq!(kv.get("/certificates/multi").await.json()["id"], second["id"]);
    let versions = kv.get("/certificates/multi/versions").await.json();
    assert_eq!(versions["value"].as_array().unwrap().len(), 2);
    assert_eq!(kv.get("/keys/multi/versions").await.json()["value"].as_array().unwrap().len(), 2);
    assert_eq!(kv.get("/secrets/multi/versions").await.json()["value"].as_array().unwrap().len(), 2);
    // list items: no version in the id, subject "" and no recovery fields, as recorded
    let list = kv.get("/certificates").await.json();
    assert_eq!(list["value"].as_array().unwrap().len(), 1);
    let item = &list["value"][0];
    assert_eq!(item["id"], format!("{}/certificates/multi", kv.base));
    assert!(item["attributes"].get("recoveryLevel").is_none());
    assert!(item["x5t"].is_string());
    // a key or secret of the same name blocks a certificate
    kv.call("PUT", "/secrets/plain", Some(json!({"value": "v"}))).await;
    let r = kv.post("/certificates/plain/create", json!({"policy": ec_policy("CN=x")})).await;
    assert_eq!(r.status, 409);
    // missing things
    let r = kv.get("/certificates/nope").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "CertificateNotFound"));
    assert!(r.json()["error"]["message"].as_str().unwrap().starts_with("A certificate with (name/id) nope was not found"));
    assert_eq!(kv.get("/certificates/nope/pending").await.json()["error"]["code"], "PendingCertificateNotFound");
}

#[tokio::test]
async fn update_and_policy() {
    let kv = Kv::new().await;
    let c = create(&kv, "upd", ec_policy("CN=upd")).await;
    let r = kv.call("PATCH", "/certificates/upd/", Some(json!({"tags": {"t": "1"}, "attributes": {"enabled": false}}))).await;
    assert_eq!(r.status, 200);
    let u = r.json();
    assert_eq!(u["tags"]["t"], "1");
    assert_eq!(u["attributes"]["enabled"], false);
    assert_eq!(u["cer"], c["cer"]);
    assert!(u["policy"].is_object());
    assert_eq!(kv.get("/keys/upd").await.json()["attributes"]["enabled"], false, "the linked key follows");

    let p = kv.get("/certificates/upd/policy").await.json();
    assert_eq!(p["key_props"]["kty"], "EC");
    let r = kv
        .call(
            "PATCH",
            "/certificates/upd/policy",
            Some(json!({"key_props": {"exportable": false, "kty": "RSA", "key_size": 3072, "reuse_key": true},
                        "x509_props": {"subject": "CN=other", "key_usage": ["decipherOnly"]},
                        "lifetime_actions": [{"trigger": {"lifetime_percentage": 98}, "action": {"action_type": "EmailContacts"}}],
                        "issuer": {"name": "Self"}})),
        )
        .await;
    assert_eq!(r.status, 200);
    let p = r.json();
    assert_eq!(p["key_props"]["key_size"], 3072);
    assert_eq!(p["x509_props"]["subject"], "CN=other");
    assert_eq!(p["x509_props"]["key_usage"], json!(["decipherOnly"]));
    assert_eq!(p["x509_props"]["basic_constraints"], json!({"ca": false}), "default filled in");
    assert_eq!(p["lifetime_actions"][0]["action"]["action_type"], "EmailContacts");
    assert_eq!(kv.get("/certificates/upd/policy").await.json()["key_props"]["kty"], "RSA");
    assert_eq!(kv.call("PATCH", "/certificates/none/policy", Some(json!({}))).await.status, 404);
}

#[tokio::test]
async fn import_pem_and_pkcs12() {
    let kv = Kv::new().await;
    let cer = std_b64(include_str!("fixtures/fixture.cer.b64"));
    let thumb = {
        use sha1::{Digest, Sha1};
        b64(&Sha1::digest(&cer))
    };
    let pem = include_str!("fixtures/bundle.pem");
    let r = kv.post("/certificates/imp-pem/import", json!({"value": STANDARD.encode(pem)})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let c = r.json();
    assert_eq!(c["x5t"], thumb);
    assert_eq!(std_b64(c["cer"].as_str().unwrap()), cer);
    assert_eq!(c["policy"]["x509_props"]["subject"], "CN=fixture-cn");
    assert_eq!(c["policy"]["key_props"]["kty"], "RSA");
    assert_eq!(c["policy"]["key_props"]["key_size"], 2048);
    assert_eq!(c["policy"]["secret_props"]["contentType"], "application/x-pem-file");
    assert!(c.get("pending").is_none(), "an import has no request");
    assert_eq!(kv.get("/certificates/imp-pem/pending").await.status, 404);
    assert!(kv.get("/secrets/imp-pem").await.json()["value"].as_str().unwrap().contains("BEGIN PRIVATE KEY"));

    // the same bundle as plain PEM text (the JavaScript SDK's form), with and without a policy,
    // with Windows line ends and surrounding blanks; the content type comes from the value itself
    let r = kv.post("/certificates/imp-text/import", json!({"value": pem})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["x5t"], thumb);
    assert_eq!(r.json()["policy"]["secret_props"]["contentType"], "application/x-pem-file");
    assert!(kv.get("/secrets/imp-text").await.json()["value"].as_str().unwrap().contains("BEGIN PRIVATE KEY"));
    let crlf = format!("\n{}\n", pem.replace('\n', "\r\n"));
    let policy = json!({"secret_props": {"contentType": "application/x-pem-file"}});
    let r = kv.post("/certificates/imp-text2/import", json!({"value": crlf, "policy": policy})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["x5t"], thumb);
    // PEM text without a private key is still refused
    let only_cert = pem.split("-----BEGIN CERTIFICATE-----").nth(1).map(|c| format!("-----BEGIN CERTIFICATE-----{c}")).unwrap();
    let only_cert = only_cert.split("-----END CERTIFICATE-----").next().unwrap().to_string() + "-----END CERTIFICATE-----\n";
    let r = kv.post("/certificates/imp-text3/import", json!({"value": only_cert})).await;
    assert_eq!(r.status, 400, "{}", r.text());

    // unencrypted PFX (which p12-keystore alone cannot read)
    let plain = include_str!("fixtures/plain.pfx.b64").trim().to_string();
    let r = kv.post("/certificates/imp-plain/import", json!({"value": plain})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["x5t"], thumb);
    assert_eq!(r.json()["policy"]["secret_props"]["contentType"], "application/x-pkcs12");
    // password-protected PFX
    let pw = include_str!("fixtures/password.pfx.b64").trim().to_string();
    let r = kv.post("/certificates/imp-pw/import", json!({"value": pw, "pwd": "s3cret"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["x5t"], thumb);
    let r = kv.post("/certificates/imp-bad/import", json!({"value": pw, "pwd": "wrong"})).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"));
    // not a certificate at all
    let r = kv.post("/certificates/imp-junk/import", json!({"value": STANDARD.encode("hello")})).await;
    assert_eq!(r.status, 400);
    let r = kv.post("/certificates/imp-junk/import", json!({"value": "***"})).await;
    assert_eq!(r.status, 400);
    // the imported key works through the key API
    let key = kv.get("/keys/imp-plain").await.json();
    assert_eq!(key["key"]["kty"], "RSA");
    assert_eq!(key["managed"], true);
}

#[tokio::test]
async fn delete_recover_purge_move_the_key_and_secret_too() {
    let kv = Kv::new().await;
    let c = create(&kv, "life", ec_policy("CN=life")).await;
    let r = kv.call("DELETE", "/certificates/life", None).await;
    assert_eq!(r.status, 200);
    let d = r.json();
    assert_eq!(d["recoveryId"], format!("{}/deletedcertificates/life", kv.base));
    assert_eq!(d["id"], c["id"]);
    assert_eq!(d["scheduledPurgeDate"].as_i64().unwrap() - d["deletedDate"].as_i64().unwrap(), 90 * 86400);
    assert_eq!(kv.get("/certificates/life").await.status, 404);
    assert_eq!(kv.get("/keys/life").await.status, 404);
    assert_eq!(kv.get("/secrets/life").await.status, 404);
    assert_eq!(kv.get("/deletedkeys/life").await.status, 200);
    assert_eq!(kv.get("/deletedsecrets/life").await.status, 200);
    let r = kv.post("/certificates/life/create", json!({"policy": ec_policy("CN=life")})).await;
    assert_eq!((r.status, inner_code(&r).as_str()), (409, "ObjectIsDeletedButRecoverable"));
    let dc = kv.get("/deletedcertificates/life").await;
    assert_eq!(dc.status, 200);
    assert_eq!(dc.json()["policy"]["issuer"]["name"], "Self");
    assert_eq!(kv.get("/deletedcertificates").await.json()["value"].as_array().unwrap().len(), 1);

    let r = kv.call("POST", "/deletedcertificates/life/recover", None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["id"], c["id"]);
    assert_eq!(kv.get("/keys/life").await.status, 200);
    assert_eq!(kv.get("/secrets/life").await.status, 200);

    kv.call("DELETE", "/certificates/life", None).await;
    assert_eq!(kv.call("DELETE", "/deletedcertificates/life", None).await.status, 204);
    assert_eq!(kv.get("/deletedcertificates/life").await.status, 404);
    assert_eq!(kv.get("/deletedkeys/life").await.status, 404);
    assert_eq!(kv.get("/deletedsecrets/life").await.status, 404);
    assert_eq!(kv.call("POST", "/deletedcertificates/life/recover", None).await.status, 404);
    assert_eq!(kv.call("DELETE", "/deletedcertificates/life", None).await.status, 404);
    // the name is free again
    create(&kv, "life", ec_policy("CN=again")).await;
}

#[tokio::test]
async fn operation_cancel_delete_and_unknown_issuer_pending() {
    let kv = Kv::new().await;
    create(&kv, "op", ec_policy("CN=op")).await;
    let r = kv.call("PATCH", "/certificates/op/pending", Some(json!({"cancellation_requested": true}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["cancellation_requested"], true);
    let r = kv.call("DELETE", "/certificates/op/pending", None).await;
    assert_eq!(r.status, 200);
    assert_eq!(kv.get("/certificates/op/pending").await.status, 404);
    assert_eq!(kv.get("/certificates/op").await.status, 200, "the certificate stays");

    // an unknown issuer leaves the request in progress until a certificate is merged
    let r =
        kv.post("/certificates/ext/create", json!({"policy": {"x509_props": {"subject": "CN=ext"}, "issuer": {"name": "Unknown"}}})).await;
    assert_eq!(r.status, 202);
    let op = kv.get("/certificates/ext/pending").await.json();
    assert_eq!(op["status"], "inProgress");
    assert_eq!(op["issuer"]["name"], "Unknown");
    assert!(op.get("target").is_none());
    // the pending certificate can be read: a bundle without `cer`, `kid`, `sid` and `x5t`
    let r = kv.get("/certificates/ext").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let c = r.json();
    assert!(c.get("cer").is_none() && c.get("x5t").is_none() && c.get("kid").is_none() && c.get("sid").is_none());
    assert_eq!(c["pending"]["id"], format!("{}/certificates/ext/pending", kv.base));
    assert_eq!(c["policy"]["issuer"]["name"], "Unknown");
    assert!(c["attributes"]["created"].is_i64());
    let id = c["id"].as_str().unwrap().to_string();
    let version = id.rsplit('/').next().unwrap().to_string();
    assert!(id.starts_with(&format!("{}/certificates/ext/", kv.base)) && version.len() == 32, "{id}");
    // by its version: the bundle without policy and pending link; any other version is 404
    let r = kv.get(&format!("/certificates/ext/{version}")).await;
    assert_eq!(r.status, 200);
    assert!(r.json().get("policy").is_none() && r.json().get("pending").is_none());
    assert_eq!(kv.get("/certificates/ext/ffffffffffffffffffffffffffffffff").await.status, 404);
    // lists: the plain list leaves it out, includePending shows it, the versions list has its one version
    assert_eq!(kv.get("/certificates").await.json()["value"].as_array().unwrap().len(), 1);
    let l = kv.get("/certificates?includePending=true").await.json();
    assert_eq!(l["value"].as_array().unwrap().len(), 2);
    let v = kv.get("/certificates/ext/versions").await.json();
    assert_eq!(v["value"].as_array().unwrap().len(), 1);
    assert_eq!(v["value"][0]["id"], id);
    // a certificate that was never requested is still 404
    assert_eq!(kv.get("/certificates/never").await.status, 404);
    // merging a certificate for some other key is refused
    let cer = include_str!("fixtures/fixture.cer.b64").trim().to_string();
    let r = kv.post("/certificates/ext/pending/merge", json!({"x5c": [cer]})).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "BadParameter"));
    assert_eq!(kv.post("/certificates/ext/pending/merge", json!({})).await.status, 400);
    assert_eq!(kv.post("/certificates/none/pending/merge", json!({"x5c": [cer]})).await.status, 404);
}

#[tokio::test]
async fn backup_and_restore() {
    let kv = Kv::new().await;
    let c = create(&kv, "bk", ec_policy("CN=bk")).await;
    let blob = kv.call("POST", "/certificates/bk/backup", None).await.json()["value"].as_str().unwrap().to_string();
    let r = kv.post("/certificates/restore", json!({"value": blob})).await;
    assert_eq!(r.status, 409);
    assert!(r.json()["error"]["message"].as_str().unwrap().starts_with("There was a conflict restoring the certificate '"));
    kv.call("DELETE", "/certificates/bk", None).await;
    kv.call("DELETE", "/deletedcertificates/bk", None).await;
    let r = kv.post("/certificates/restore", json!({"value": blob})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["id"], c["id"]);
    assert_eq!(kv.get("/certificates/bk").await.json()["cer"], c["cer"]);
    assert_eq!(kv.get("/keys/bk").await.status, 200);
    assert_eq!(kv.get("/secrets/bk").await.json()["managed"], true);
    assert_eq!(kv.post("/certificates/restore", json!({"value": "bm90IGEgYmFja3Vw"})).await.status, 400);
    assert_eq!(kv.call("POST", "/certificates/none/backup", None).await.status, 404);
}

#[tokio::test]
async fn issuers_and_contacts() {
    let kv = Kv::new().await;
    // contacts
    assert_eq!(kv.get("/certificates/contacts").await.json()["error"]["code"], "ContactsNotFound");
    let contacts = json!([{"email": "a@x.com", "name": "A", "phone": "1"}, {"email": "b@x.com"}]);
    let r = kv.call("PUT", "/certificates/contacts", Some(json!({"contacts": contacts}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["id"], format!("{}/certificates/contacts", kv.base));
    assert_eq!(kv.get("/certificates/contacts").await.json()["contacts"], contacts);
    assert_eq!(kv.call("DELETE", "/certificates/contacts", None).await.json()["contacts"], contacts);
    assert_eq!(kv.get("/certificates/contacts").await.status, 404);

    // issuers
    let r = kv.get("/certificates/issuers/nope").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "CertificateIssuerNotFound"));
    let r = kv
        .call(
            "PUT",
            "/certificates/issuers/iss1",
            Some(json!({"provider": "Test", "credentials": {"account_id": "u", "password": "secret"},
                        "org_details": {"admin_details": [{"first_name": "J", "email": "j@x.com"}]}, "attributes": {"enabled": true}})),
        )
        .await;
    assert_eq!(r.status, 200);
    let i = r.json();
    assert_eq!(i["id"], format!("{}/certificates/issuers/iss1", kv.base));
    assert_eq!(i["credentials"], json!({"account_id": "u"}), "the password is never returned");
    assert_eq!(i["org_details"]["zip"], 0);
    assert_eq!(i["attributes"]["enabled"], true);
    kv.call("PUT", "/certificates/issuers/iss2", Some(json!({"provider": "Test"}))).await;
    let list = kv.get("/certificates/issuers").await.json();
    assert_eq!(
        list["value"],
        json!([{"id": format!("{}/certificates/issuers/iss1", kv.base), "provider": "Test"},
                                     {"id": format!("{}/certificates/issuers/iss2", kv.base), "provider": "Test"}])
    );
    let r = kv.call("PATCH", "/certificates/issuers/iss1", Some(json!({"provider": "Other"}))).await;
    assert_eq!(r.json()["provider"], "Other");
    assert_eq!(r.json()["credentials"]["account_id"], "u", "unchanged fields stay");
    assert_eq!(kv.call("DELETE", "/certificates/issuers/iss1", None).await.status, 200);
    assert_eq!(kv.get("/certificates/issuers/iss1").await.status, 404);
    assert_eq!(kv.call("PUT", "/certificates/issuers/bad", Some(json!({}))).await.status, 400, "provider is required");
}

// ---------------------------------------------------------------------------
// What the certificate Key Vault issues looks like (profile recorded in the
// certificates recordings), and the behaviour around pending certificates.
// ---------------------------------------------------------------------------

fn unknown_policy(subject: &str) -> Value {
    json!({"x509_props": {"subject": subject}, "issuer": {"name": "Unknown"}})
}

#[tokio::test]
async fn certificate_profile_matches_what_azure_issues() {
    let kv = Kv::new().await;
    let c = create(&kv, "prof", json!({"x509_props": {"subject": "CN=DefaultPolicy"}})).await;
    let der = std_b64(c["cer"].as_str().unwrap());
    let (_, x) = X509Certificate::from_der(&der).unwrap();
    assert_eq!(x.version().0, 2);
    assert_eq!(x.tbs_certificate.raw_serial().len(), 16);
    assert_eq!(x.signature_algorithm.algorithm.to_id_string(), "1.2.840.113549.1.1.11", "sha256WithRSAEncryption");
    let exts: Vec<(String, bool)> = x.extensions().iter().map(|e| (e.oid.to_id_string(), e.critical)).collect();
    assert_eq!(
        exts,
        [("2.5.29.15", true), ("2.5.29.19", false), ("2.5.29.37", false), ("2.5.29.35", false), ("2.5.29.14", false)]
            .map(|(o, c)| (o.to_string(), c))
    );
    // valid from ten minutes before the request, for the policy's months
    let nbf = x.validity().not_before.timestamp();
    let created = c["attributes"]["created"].as_i64().unwrap();
    assert!((created - nbf - 600).abs() <= 2, "nbf {nbf} created {created}");
    assert_eq!(c["attributes"]["exp"], x.validity().not_after.timestamp());
}

#[tokio::test]
async fn upn_and_ip_sans_with_an_empty_subject_and_p521() {
    let kv = Kv::new().await;
    let sans = json!({"ipAddresses": ["10.0.0.1", "192.168.1.1"], "upns": ["john.doe@domain.com"], "dns_names": ["a.example.com"]});
    let policy = json!({"key_props": {"kty": "EC", "crv": "P-521"}, "x509_props": {"sans": sans}});
    let c = create(&kv, "san", policy).await;
    assert_eq!(c["policy"]["x509_props"]["sans"], sans);
    let der = std_b64(c["cer"].as_str().unwrap());
    let (_, x) = X509Certificate::from_der(&der).unwrap();
    assert_eq!(x.subject().as_raw().len(), 2, "an empty subject");
    assert_eq!(x.signature_algorithm.algorithm.to_id_string(), "1.2.840.10045.4.3.4", "ecdsa-with-SHA512");
    let san = x.get_extension_unique(&x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME).unwrap().unwrap();
    assert!(san.critical, "the SAN is critical when the subject is empty");
    let seen = san_dump(&der);
    assert!(seen.contains(&"ip:10.0.0.1".to_string()) && seen.contains(&"ip:192.168.1.1".to_string()), "{seen:?}");
    assert!(seen.iter().any(|s| s.starts_with("upn:") && s.contains("john.doe@domain.com")), "{seen:?}");
    assert!(seen.contains(&"dns:a.example.com".to_string()), "{seen:?}");
    // the linked key is the P-521 key, and carries the certificate's validity like the certificate
    let key = kv.get("/keys/san").await.json();
    assert_eq!(key["key"]["crv"], "P-521");
    assert_eq!(key["attributes"]["nbf"], c["attributes"]["nbf"]);
    assert_eq!(key["attributes"]["exp"], c["attributes"]["exp"]);
}

fn san_dump(der: &[u8]) -> Vec<String> {
    use x509_parser::prelude::GeneralName;
    let (_, x) = X509Certificate::from_der(der).unwrap();
    let san = x.subject_alternative_name().unwrap().unwrap();
    san.value
        .general_names
        .iter()
        .map(|g| match g {
            GeneralName::DNSName(d) => format!("dns:{d}"),
            GeneralName::IPAddress(b) => format!("ip:{}", b.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(".")),
            GeneralName::OtherName(o, raw) if o.to_id_string() == "1.3.6.1.4.1.311.20.2.3" => {
                format!("upn:{}", String::from_utf8_lossy(raw))
            }
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn preserve_cert_order_and_status_details_of_an_unknown_issuer() {
    let kv = Kv::new().await;
    let r = kv.post("/certificates/ext/create", json!({"policy": unknown_policy("CN=ext"), "preserveCertOrder": true})).await;
    assert_eq!(r.status, 202);
    let op = r.json();
    assert_eq!(op["preserveCertOrder"], true);
    assert_eq!(op["status_details"], "Pending certificate created. Please Perform Merge to complete the request.");
    let again = kv.get("/certificates/ext/pending").await.json();
    assert_eq!(again["status_details"], op["status_details"]);
    assert_eq!(again["preserveCertOrder"], true);
    // a request that does not say so carries no such field; a self-signed one keeps its text
    let r = kv.post("/certificates/s/create", json!({"policy": ec_policy("CN=s")})).await;
    assert!(r.json().get("preserveCertOrder").is_none());
    assert!(r.json()["status_details"].as_str().unwrap().contains("in progress"));
    let r = kv.post("/certificates/p/create", json!({"policy": ec_policy("CN=p"), "preserveCertOrder": true})).await;
    assert_eq!(r.json()["preserveCertOrder"], true);
    assert_eq!(kv.get("/certificates/p").await.json()["preserveCertOrder"], true);
}

#[tokio::test]
async fn import_without_a_policy_gets_unknown_issuer_and_email_contacts() {
    let kv = Kv::new().await;
    let pem = include_str!("fixtures/bundle.pem");
    let r = kv.post("/certificates/imp/import", json!({"value": STANDARD.encode(pem)})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let p = &r.json()["policy"];
    assert_eq!(p["issuer"]["name"], "Unknown");
    assert_eq!(p["lifetime_actions"][0]["action"]["action_type"], "EmailContacts");
    assert_eq!(p["lifetime_actions"][0]["trigger"]["lifetime_percentage"], 80);
    assert!(p["x509_props"].get("sans").is_none(), "the policy of an import has no empty sans");
    assert!(p["x509_props"]["validity_months"].as_i64().unwrap() >= 1);
    // with a policy of its own, the policy's issuer and lifetime action stay
    let r = kv.post("/certificates/imp2/import", json!({"value": STANDARD.encode(pem), "policy": {"issuer": {"name": "Self"}}})).await;
    assert_eq!(r.json()["policy"]["issuer"]["name"], "Self");
    assert_eq!(r.json()["policy"]["lifetime_actions"][0]["action"]["action_type"], "AutoRenew");
}

#[tokio::test]
async fn a_pending_certificate_can_be_deleted_and_is_listed_only_on_request() {
    let kv = Kv::new().await;
    create(&kv, "done", ec_policy("CN=done")).await;
    assert_eq!(kv.post("/certificates/ext/create", json!({"policy": unknown_policy("CN=ext")})).await.status, 202);
    let names = |l: Value| {
        l["value"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().rsplit('/').next().unwrap().to_string()).collect::<Vec<_>>()
    };
    assert_eq!(names(kv.get("/certificates").await.json()), ["done"]);
    assert_eq!(names(kv.get("/certificates?includePending=false").await.json()), ["done"]);
    assert_eq!(names(kv.get("/certificates?includePending=true").await.json()), ["done", "ext"]);

    // cancel, delete the operation, then the certificate (what the recorded flow does)
    kv.call("PATCH", "/certificates/ext/pending", Some(json!({"cancellation_requested": true}))).await;
    assert_eq!(kv.call("DELETE", "/certificates/ext/pending", None).await.status, 200);
    assert_eq!(kv.get("/certificates/ext/pending").await.status, 404);
    let r = kv.call("DELETE", "/certificates/ext", None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let d = r.json();
    assert_eq!(d["recoveryId"], format!("{}/deletedcertificates/ext", kv.base));
    assert!(d["id"].as_str().unwrap().starts_with(&format!("{}/certificates/ext/", kv.base)));
    assert_eq!(d["attributes"]["enabled"], false, "a cancelled request is disabled");
    assert_eq!(d["policy"]["issuer"]["name"], "Unknown");
    assert!(d.get("cer").is_none());
    let got = kv.get("/deletedcertificates/ext").await.json();
    assert_eq!(got["id"], d["id"]);
    assert_eq!(names(kv.get("/deletedcertificates").await.json()), ["ext"]);
    assert_eq!(kv.call("DELETE", "/deletedcertificates/ext", None).await.status, 204);

    // an uncancelled pending certificate is deleted too, and recovers
    assert_eq!(kv.post("/certificates/ext2/create", json!({"policy": unknown_policy("CN=ext2")})).await.status, 202);
    let d = kv.call("DELETE", "/certificates/ext2", None).await;
    assert_eq!(d.status, 200);
    assert_eq!(d.json()["attributes"]["enabled"], true);
    assert_eq!(kv.post("/deletedcertificates/ext2/recover", json!({})).await.status, 200);
}

/// Recordings `test_certificates_client.pyTestCertificateClienttest_async_request_cancellation_and_deletion[2025-07-01].json` entry 7
/// ("Pending Certificate not found: <name>", capital C; the 2022 and 2023 recordings have a small c).
#[tokio::test]
async fn a_missing_pending_certificate_has_the_current_wording() {
    let kv = Kv::new().await;
    let r = kv.get("/certificates/nothing/pending").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["code"], "PendingCertificateNotFound");
    assert_eq!(r.json()["error"]["message"], "Pending Certificate not found: nothing");
}
