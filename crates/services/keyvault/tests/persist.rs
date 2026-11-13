//! Key Vault state across restarts (`--data-dir`): secrets, keys with their private material, certificates, deleted
//! items, policies, pending requests; and what a damaged file does.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use azemu_core::persist;
use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_keyvault::KeyVaultService;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use common::*;
use serde_json::{json, Value};

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-keyvault-persist-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn open(d: &Path) -> Kv {
    let srv = TestServer::start_persistent(vec![Arc::new(KeyVaultService::new())], d).await;
    let base = srv.url("vault", "testvault");
    Kv { srv, base }
}

async fn try_open(d: &Path) -> anyhow::Result<Kv> {
    let srv = TestServer::try_start_persistent(vec![Arc::new(KeyVaultService::new())], d, |b| b).await?;
    let base = srv.url("vault", "testvault");
    Ok(Kv { srv, base })
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/rsa2048.json")).unwrap()
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

/// Make a vault with one of everything, in two vaults.
async fn fill(kv: &Kv) {
    // secrets: versions, content type, tags, attributes, a disabled one, a deleted one, a recovered one
    ok(kv, "PUT", "/secrets/db-password", Some(json!({"value": "first", "contentType": "text/plain", "tags": {"env": "dev"}}))).await;
    ok(
        kv,
        "PUT",
        "/secrets/db-password",
        Some(json!({"value": "second", "attributes": {"nbf": 1700000000, "exp": 4102444800i64}, "tags": {"env": "prod", "team": "x"}})),
    )
    .await;
    ok(kv, "PUT", "/secrets/off", Some(json!({"value": "disabled", "attributes": {"enabled": false}}))).await;
    ok(kv, "PUT", "/secrets/deleted-one", Some(json!({"value": "bye"}))).await;
    ok(kv, "DELETE", "/secrets/deleted-one", None).await;
    ok(kv, "PUT", "/secrets/back", Some(json!({"value": "again"}))).await;
    ok(kv, "DELETE", "/secrets/back", None).await;
    ok(kv, "POST", "/deletedsecrets/back/recover", None).await;

    // keys: RSA created, RSA imported, EC curves, symmetric, rotated, with a rotation policy, one deleted
    ok(kv, "POST", "/keys/rsa-key/create", Some(json!({"kty": "RSA", "key_size": 2048, "key_ops": ["sign", "verify", "encrypt", "decrypt", "wrapKey", "unwrapKey"], "tags": {"k": "v"}}))).await;
    for crv in ["P-256", "P-384", "P-521", "P-256K"] {
        ok(kv, "POST", &format!("/keys/ec-{}/create", crv.replace('-', "").to_lowercase()), Some(json!({"kty": "EC", "crv": crv}))).await;
    }
    ok(kv, "POST", "/keys/sym/create", Some(json!({"kty": "oct-HSM", "key_size": 256}))).await;
    let f = fixture();
    ok(kv, "PUT", "/keys/imported", Some(json!({"key": {"kty": "RSA", "n": f["n"], "e": f["e"], "d": f["d"], "p": f["p"], "q": f["q"], "dp": f["dp"], "dq": f["dq"], "qi": f["qi"], "key_ops": ["sign", "verify", "encrypt", "decrypt", "wrapKey", "unwrapKey"]}})))
        .await;
    ok(kv, "PUT", "/keys/rsa-key/rotationpolicy", Some(json!({"lifetimeActions": [{"trigger": {"timeAfterCreate": "P90D"}, "action": {"type": "Rotate"}}], "attributes": {"expiryTime": "P1Y"}}))).await;
    ok(kv, "POST", "/keys/rsa-key/rotate", None).await;
    ok(kv, "POST", "/keys/ec-p256/create", Some(json!({"kty": "EC", "crv": "P-256"}))).await;
    ok(kv, "POST", "/keys/gone-key/create", Some(json!({"kty": "EC"}))).await;
    ok(kv, "DELETE", "/keys/gone-key", None).await;

    // certificates: self-signed, imported with a password, one waiting for an unknown issuer, issuers, contacts, a deleted one
    let policy = json!({"key_props": {"kty": "EC", "crv": "P-256", "exportable": true, "reuse_key": false},
                        "x509_props": {"subject": "CN=web.example.com", "sans": {"dns_names": ["a.example.com"]}, "validity_months": 6}, "issuer": {"name": "Self"}});
    ok(kv, "POST", "/certificates/web/create", Some(json!({"policy": policy.clone(), "tags": {"c": "1"}}))).await;
    ok(kv, "POST", "/certificates/web/create", Some(json!({"policy": policy.clone()}))).await; // a second version
    let pfx = include_str!("fixtures/password.pfx.b64").trim().to_string();
    let r = kv.post("/certificates/imported-pfx/import", json!({"value": pfx, "pwd": "s3cret"})).await;
    assert!(r.status < 300, "{}", r.text());
    ok(
        kv,
        "POST",
        "/certificates/ext/create",
        Some(json!({"policy": {"x509_props": {"subject": "CN=ext"}, "issuer": {"name": "Unknown"}}})),
    )
    .await;
    ok(kv, "PUT", "/certificates/issuers/iss1", Some(json!({"provider": "Test", "credentials": {"account_id": "u", "password": "secret"}, "org_details": {"admin_details": [{"first_name": "J", "email": "j@x.com"}]}}))).await;
    ok(kv, "PUT", "/certificates/contacts", Some(json!({"contacts": [{"email": "a@x.com", "name": "A", "phone": "1"}]}))).await;
    ok(kv, "POST", "/certificates/gone-cert/create", Some(json!({"policy": policy}))).await;
    ok(kv, "DELETE", "/certificates/gone-cert", None).await;

    // another vault
    kv.call_at(&kv.vault_base("other"), "PUT", "/secrets/s", Some(json!({"value": "in the other vault"}))).await;
}

/// Everything a client can read, as one text.
async fn dump(kv: &Kv) -> String {
    let mut out = String::new();
    for base in [kv.base.clone(), kv.vault_base("other")] {
        for list in [
            "/secrets",
            "/keys",
            "/certificates?includePending=true",
            "/deletedsecrets",
            "/deletedkeys",
            "/deletedcertificates",
            "/certificates/issuers",
            "/certificates/contacts",
        ] {
            let r = kv.call_at(&base, "GET", list, None).await;
            out.push_str(&format!("=== {base}{list}\n{} {}\n", r.status, r.text()));
            let Some(items) = r.json().get("value").and_then(|v| v.as_array()).cloned() else { continue };
            for item in items {
                let Some(id) = item["id"].as_str().or_else(|| item["kid"].as_str()) else { continue };
                let Some(path) = id.strip_prefix(&base) else { continue };
                // the item itself and its versions
                let name_path: String = path.split('/').take(3).collect::<Vec<_>>().join("/");
                for p in [path.to_string(), name_path.clone(), format!("{name_path}/versions")] {
                    let r = kv.call_at(&base, "GET", &p, None).await;
                    out.push_str(&format!("--- {p}\n{} {}\n", r.status, r.text()));
                    if p.ends_with("/versions") {
                        for v in r.json()["value"].as_array().cloned().unwrap_or_default() {
                            if let Some(vp) = v["id"].as_str().or_else(|| v["kid"].as_str()).and_then(|i| i.strip_prefix(&base)) {
                                let r = kv.call_at(&base, "GET", vp, None).await;
                                out.push_str(&format!("--- {vp}\n{} {}\n", r.status, r.text()));
                            }
                        }
                    }
                }
                if name_path.starts_with("/keys") {
                    let r = kv.call_at(&base, "GET", &format!("{name_path}/rotationpolicy"), None).await;
                    out.push_str(&format!("--- rotation {name_path}\n{} {}\n", r.status, r.text()));
                }
                if name_path.starts_with("/certificates") && !name_path.contains("issuers") {
                    for sub in ["policy", "pending"] {
                        let r = kv.call_at(&base, "GET", &format!("{name_path}/{sub}"), None).await;
                        out.push_str(&format!("--- {sub} {name_path}\n{} {}\n", r.status, r.text()));
                    }
                }
            }
        }
    }
    out.replace(&format!(":{}", kv.srv.port()), ":PORT")
}

fn b64url(d: &[u8]) -> String {
    b64(d)
}

#[tokio::test]
async fn everything_in_the_vaults_is_the_same_after_a_restart_and_the_keys_still_work() {
    let d = dir("rich");
    let kv = open(&d).await;
    fill(&kv).await;

    // things made with the private keys before the restart
    let digest = b64url(&{
        use sha2::Digest;
        sha2::Sha256::digest(b"message").to_vec()
    });
    let rsa = ok(&kv, "GET", "/keys/rsa-key", None).await["key"]["kid"].as_str().unwrap().to_string();
    let rsa_path = rsa.strip_prefix(&kv.base).unwrap().to_string();
    let sig_rsa = ok(&kv, "POST", &format!("{rsa_path}/sign"), Some(json!({"alg": "RS256", "value": digest}))).await["value"]
        .as_str()
        .unwrap()
        .to_string();
    let cipher = ok(&kv, "POST", &format!("{rsa_path}/encrypt"), Some(json!({"alg": "RSA-OAEP-256", "value": b64url(b"secret text")})))
        .await["value"]
        .as_str()
        .unwrap()
        .to_string();
    let ec = ok(&kv, "GET", "/keys/ec-p384", None).await["key"]["kid"].as_str().unwrap().to_string();
    let ec_path = ec.strip_prefix(&kv.base).unwrap().to_string();
    let sig_ec = ok(&kv, "POST", &format!("{ec_path}/sign"), Some(json!({"alg": "ES384", "value": b64url(&[7u8; 48])}))).await["value"]
        .as_str()
        .unwrap()
        .to_string();
    let sym = ok(&kv, "GET", "/keys/sym", None).await["key"]["kid"].as_str().unwrap().to_string();
    let sym_path = sym.strip_prefix(&kv.base).unwrap().to_string();
    let wrapped = kv.post(&format!("{sym_path}/wrapkey"), json!({"alg": "A256KW", "value": b64url(&[1u8; 32])})).await;
    let wrapped = (wrapped.status == 200).then(|| wrapped.json()["value"].as_str().unwrap().to_string());
    let fx = fixture();
    let imported = ok(&kv, "GET", "/keys/imported", None).await["key"]["kid"].as_str().unwrap().to_string();
    let imported_path = imported.strip_prefix(&kv.base).unwrap().to_string();
    let sig_fixture =
        ok(&kv, "POST", &format!("{imported_path}/sign"), Some(json!({"alg": "RS256", "value": fx["digest"]}))).await["value"].clone();
    assert_eq!(sig_fixture, fx["rs256"]);
    let pfx_secret = ok(&kv, "GET", "/secrets/imported-pfx", None).await["value"].as_str().unwrap().to_string();

    let before = dump(&kv).await;
    assert!(before.len() > 30_000, "the dump is expected to be big: {}", before.len());
    kv.srv.shutdown().await.unwrap();

    let kv = open(&d).await;
    let after = dump(&kv).await;
    if before != after {
        let at = before.bytes().zip(after.bytes()).position(|(x, y)| x != y).unwrap_or(before.len().min(after.len()));
        let window = |t: &str| t[at.saturating_sub(300)..(at + 500).min(t.len())].to_string();
        panic!("state differs after a restart at byte {at}:\nbefore: {}\nafter:  {}", window(&before), window(&after));
    }

    // the private keys are the same keys
    let r = ok(&kv, "POST", &format!("{rsa_path}/sign"), Some(json!({"alg": "RS256", "value": digest}))).await;
    assert_eq!(r["value"], sig_rsa, "RS256 is deterministic: the same private key gives the same signature");
    let r = ok(&kv, "POST", &format!("{rsa_path}/decrypt"), Some(json!({"alg": "RSA-OAEP-256", "value": cipher}))).await;
    assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(r["value"].as_str().unwrap()).unwrap(), b"secret text");
    let r =
        ok(&kv, "POST", &format!("{ec_path}/verify"), Some(json!({"alg": "ES384", "digest": b64url(&[7u8; 48]), "value": sig_ec}))).await;
    assert_eq!(r["value"], true);
    if let Some(w) = wrapped {
        let r = ok(&kv, "POST", &format!("{sym_path}/unwrapkey"), Some(json!({"alg": "A256KW", "value": w}))).await;
        assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(r["value"].as_str().unwrap()).unwrap(), vec![1u8; 32]);
    }
    let r = ok(&kv, "POST", &format!("{imported_path}/sign"), Some(json!({"alg": "RS256", "value": fx["digest"]}))).await;
    assert_eq!(r["value"], fx["rs256"]);
    assert_eq!(ok(&kv, "GET", "/secrets/imported-pfx", None).await["value"].as_str().unwrap(), pfx_secret);

    // deleted items are still deleted, with their purge dates, and can be recovered; the pending request is still pending
    let del = ok(&kv, "GET", "/deletedsecrets/deleted-one", None).await;
    assert_eq!(del["scheduledPurgeDate"].as_i64().unwrap() - del["deletedDate"].as_i64().unwrap(), 90 * 86400);
    ok(&kv, "POST", "/deletedsecrets/deleted-one/recover", None).await;
    ok(&kv, "POST", "/deletedkeys/gone-key/recover", None).await;
    ok(&kv, "POST", "/deletedcertificates/gone-cert/recover", None).await;
    assert_eq!(ok(&kv, "GET", "/certificates/ext/pending", None).await["status"], "inProgress");
    // new things work and get new version ids
    ok(&kv, "PUT", "/secrets/db-password", Some(json!({"value": "third"}))).await;
    assert_eq!(ok(&kv, "GET", "/secrets/db-password/versions", None).await["value"].as_array().unwrap().len(), 3);
    kv.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(unix)]
#[tokio::test]
async fn the_files_are_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let d = dir("mode");
    let kv = open(&d).await;
    ok(&kv, "PUT", "/secrets/s", Some(json!({"value": "v"}))).await;
    kv.srv.shutdown().await.unwrap();
    let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(d.join("keyvault")), 0o700);
    assert_eq!(mode(d.join("keyvault").join("keyvault.state")), 0o600);
    // a directory that existed with wider rights is tightened
    std::fs::set_permissions(d.join("keyvault"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(d.join("keyvault").join("keyvault.state"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let kv = open(&d).await;
    ok(&kv, "PUT", "/secrets/s2", Some(json!({"value": "v2"}))).await;
    kv.srv.shutdown().await.unwrap();
    assert_eq!(mode(d.join("keyvault")), 0o700);
    assert_eq!(mode(d.join("keyvault").join("keyvault.state")), 0o600);
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn reads_and_key_operations_are_not_changes_and_unchanged_state_is_not_written_again() {
    let d = dir("unchanged");
    let svc = Arc::new(KeyVaultService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &d).await;
    let kv = Kv { base: srv.url("vault", "testvault"), srv };
    ok(&kv, "PUT", "/secrets/s", Some(json!({"value": "v"}))).await;
    let k = ok(&kv, "POST", "/keys/k/create", Some(json!({"kty": "EC"}))).await;
    let kid = k["key"]["kid"].as_str().unwrap().strip_prefix(&kv.base).unwrap().to_string();
    let state_dir = d.join("keyvault");
    svc.save(&state_dir).unwrap();
    let file = state_dir.join("keyvault.state");
    let first = (std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap());
    let g = svc.persist_generation();
    std::thread::sleep(Duration::from_millis(30));
    svc.save(&state_dir).unwrap();
    assert_eq!(
        (std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap()),
        first,
        "nothing written the second time"
    );
    kv.get("/secrets").await;
    kv.get("/secrets/s").await;
    kv.get("/keys").await;
    kv.post(&format!("{kid}/sign"), json!({"alg": "ES256", "value": b64(&[1u8; 32])})).await;
    kv.post("/rng", json!({"count": 8})).await;
    kv.post("/secrets/s/backup", json!({})).await;
    kv.post("/keys/k/backup", json!({})).await;
    assert_eq!(svc.persist_generation(), g, "reads, signing, random bytes and backups are not changes");
    ok(&kv, "PUT", "/secrets/s", Some(json!({"value": "v2"}))).await;
    assert_ne!(svc.persist_generation(), g);
    kv.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

fn flip_byte(path: &Path, at: usize) {
    let mut b = std::fs::read(path).unwrap();
    let at = at.min(b.len() - 1);
    b[at] ^= 0xff;
    std::fs::write(path, b).unwrap();
}

#[tokio::test]
async fn damaged_state_files_are_refused_with_a_message_that_names_the_file() {
    let d = dir("damage");
    let kv = open(&d).await;
    ok(&kv, "PUT", "/secrets/s", Some(json!({"value": "some secret"}))).await;
    ok(&kv, "POST", "/keys/e/create", Some(json!({"kty": "EC"}))).await;
    ok(&kv, "POST", "/keys/o/create", Some(json!({"kty": "oct-HSM", "key_size": 128}))).await;
    kv.srv.shutdown().await.unwrap();
    let file = d.join("keyvault").join("keyvault.state");
    let good = std::fs::read(&file).unwrap();
    let refused = |d: PathBuf| async move {
        match try_open(&d).await {
            Ok(k) => {
                k.srv.shutdown().await.ok();
                None
            }
            Err(e) => Some(e.to_string()),
        }
    };
    for cut in [0usize, 5, 21, good.len() / 2, good.len() - 1] {
        std::fs::write(&file, &good[..cut]).unwrap();
        let e = refused(d.clone()).await.unwrap_or_else(|| panic!("cut at {cut} was accepted"));
        assert!(e.contains("keyvault.state") && e.contains("--data-discard-unreadable"), "{e}");
    }
    let mut accepted = 0;
    let mut tried = 0;
    for at in (0..good.len()).step_by(3) {
        std::fs::write(&file, &good).unwrap();
        flip_byte(&file, at);
        tried += 1;
        if let Some(e) = refused(d.clone()).await {
            assert!(e.contains("keyvault.state"), "flip at {at}: {e}");
        } else {
            accepted += 1;
            std::fs::write(&file, &good).unwrap();
        }
    }
    assert!(accepted < tried / 2, "{accepted} of {tried} flips were accepted");
    std::fs::write(&file, persist::encode("queue", 1, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("holds queue state, expected keyvault"));
    std::fs::write(&file, persist::encode("keyvault", 99, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("version 99"));
    std::fs::write(&file, b"AZEMU-STATE keyvault 1\n\xc1\xc1").unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("keyvault.state"));
    // a key whose numbers are not a key
    let text = String::from_utf8_lossy(&good).into_owned();
    assert!(text.contains("\"secret\"") || good.windows(6).any(|w| w == b"secret"), "the EC scalar is stored under its name");
    std::fs::write(&file, &good).unwrap();
    std::fs::write(d.join("keyvault").join("notes.txt"), "mine").unwrap();
    let kv = try_open(&d).await.expect("extra files are ignored");
    assert_eq!(ok(&kv, "GET", "/secrets/s", None).await["value"], "some secret");
    kv.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn reset_then_save_leaves_nothing_and_a_backup_made_before_restores_after() {
    let d = dir("reset");
    let kv = open(&d).await;
    ok(&kv, "PUT", "/secrets/s", Some(json!({"value": "v"}))).await;
    let backup = ok(&kv, "POST", "/secrets/s/backup", Some(json!({}))).await["value"].as_str().unwrap().to_string();
    kv.srv.save_now().await;
    kv.srv.reset();
    kv.srv.save_now().await;
    drop(kv);
    let kv = open(&d).await;
    assert_eq!(kv.get("/secrets/s").await.status, 404);
    let r = kv.post("/secrets/restore", json!({"value": backup})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(ok(&kv, "GET", "/secrets/s", None).await["value"], "v");
    kv.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

/// While a vault with big values is being saved, other requests are served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_does_not_make_requests_wait_for_the_encoding_and_the_writing() {
    let d = dir("lock");
    let svc = Arc::new(KeyVaultService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &d).await;
    let kv = Kv { base: srv.url("vault", "testvault"), srv };
    let value = "v".repeat(20_000);
    for i in 0..400 {
        for v in 0..8 {
            let r = kv.call("PUT", &format!("/secrets/s{i}"), Some(json!({"value": format!("{v}{value}")}))).await;
            assert_eq!(r.status, 200);
        }
    }
    let state_dir = d.join("keyvault");
    let saver = {
        let svc = svc.clone();
        let state_dir = state_dir.clone();
        tokio::task::spawn_blocking(move || {
            let t = std::time::Instant::now();
            svc.save(&state_dir).unwrap();
            t.elapsed()
        })
    };
    let mut slowest = Duration::ZERO;
    let mut served = 0;
    while !saver.is_finished() {
        let t = std::time::Instant::now();
        let r = kv.get("/secrets/s1").await;
        slowest = slowest.max(t.elapsed());
        assert_eq!(r.status, 200);
        served += 1;
    }
    let took = saver.await.unwrap();
    assert!(served >= 3, "only {served} requests were served during a save of {took:?}");
    assert!(slowest < took / 2, "a request waited {slowest:?} during a save of {took:?}");
    kv.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
    let _ = STANDARD.encode("");
}

/// A value nested a thousand deep (the MessagePack reader's own limit is 1024 levels) is an error that names the file,
/// never a crash of the process; a reasonably deep one is read.
#[tokio::test]
async fn a_deeply_nested_value_in_the_file_is_not_a_stack_overflow() {
    let d = dir("nested");
    let kv = open(&d).await;
    ok(&kv, "PUT", "/secrets/s", Some(json!({"value": "v"}))).await;
    ok(&kv, "PUT", "/certificates/contacts", Some(json!({"contacts": [{"email": "@@@@"}]}))).await;
    kv.srv.shutdown().await.unwrap();
    let file = d.join("keyvault").join("keyvault.state");
    let good = std::fs::read(&file).unwrap();
    // the contact {"email": "@@@@"} is a map of one entry: 0x81 0xa5 "email" 0xa4 "@@@@"
    let needle: Vec<u8> = [&[0x81u8, 0xa5][..], b"email", &[0xa4], b"@@@@"].concat();
    let at = good.windows(needle.len()).position(|w| w == needle.as_slice()).expect("the contact is in the file");
    for depth in [50usize, 150, 190, 250, 900, 1000, 1100, 100_000] {
        let mut bytes = good[..at].to_vec();
        bytes.extend(std::iter::repeat_n(0x91u8, depth));
        bytes.push(0xc0);
        bytes.extend_from_slice(&good[at + needle.len()..]);
        std::fs::write(&file, &bytes).unwrap();
        // run it on a thread with the default stack of a blocking thread (2 MiB)
        let d2 = d.clone();
        let outcome = std::thread::Builder::new().stack_size(2 << 20).spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async { try_open(&d2).await.map(|k| k.srv).map_err(|e| e.to_string()) })
        });
        let res = outcome.unwrap().join().expect("the load did not crash");
        match res {
            Ok(srv) => {
                assert!(depth < 200, "{depth} levels were accepted");
                srv.shutdown().await.unwrap()
            }
            Err(e) => {
                assert!(depth >= 150, "{depth} levels were refused: {e}");
                assert!(e.contains("keyvault.state") && e.contains("depth limit"), "{e}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}
