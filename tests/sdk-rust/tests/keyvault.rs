//! Key Vault through `azure_security_keyvault_secrets`, `_keys` and `_certificates`.
//! Vault URL: `https://{vault}.vault.localhost:{port}`; the challenge resource check is off because
//! the host is not `*.vault.azure.net` (README of the emulator).

use std::collections::HashMap;

use azemu_sdk_rust_tests::{cred, emu, uniq};
use azure_core::http::{RequestContent, StatusCode};
use azure_core::time::OffsetDateTime;
use azure_core::Error;
use azure_security_keyvault_certificates::models::{
    CertificatePolicy, CreateCertificateParameters, IssuerParameters, X509CertificateProperties,
};
use azure_security_keyvault_certificates::CertificateClient;
use azure_security_keyvault_certificates::ResourceExt as _;
use azure_security_keyvault_keys::models::{
    CreateKeyParameters, CurveName, EncryptionAlgorithm, KeyOperationParameters, KeyType, SignParameters, SignatureAlgorithm,
    VerifyParameters,
};
use azure_security_keyvault_keys::KeyClient;
use azure_security_keyvault_keys::ResourceExt as _;
use azure_security_keyvault_secrets::models::{
    RestoreSecretParameters, SecretAttributes, SetSecretParameters, UpdateSecretPropertiesParameters,
};
use azure_security_keyvault_secrets::{ResourceExt as _, SecretClient, SecretClientOptions};
use futures::TryStreamExt;

fn vault() -> String {
    format!("https://{}.vault.localhost:{}", uniq("kv"), emu().port)
}

// No client switch: the challenge names the domain of the request (vault.localhost:<port>), which the SDK's own check accepts.
// `the_old_switch_still_works` below is the one test that uses `verify_challenge_resource: Some(false)`.
fn secrets(v: &str) -> SecretClient {
    SecretClient::new(v, cred(), None).unwrap()
}
fn keys(v: &str) -> KeyClient {
    KeyClient::new(v, cred(), None).unwrap()
}
fn certs(v: &str) -> CertificateClient {
    CertificateClient::new(v, cred(), None).unwrap()
}

#[tokio::test]
async fn the_old_switch_still_works() {
    let v = vault();
    let o = SecretClientOptions { verify_challenge_resource: Some(false), ..Default::default() };
    let c = SecretClient::new(&v, cred(), Some(o)).unwrap();
    let params = SetSecretParameters { value: Some("v".into()), ..Default::default() };
    c.set_secret("s", RequestContent::try_from(params).unwrap(), None).await.unwrap();
    let got = secrets(&v).get_secret("s", None).await.unwrap().into_model().unwrap();
    assert_eq!(got.value.as_deref(), Some("v"));
}

fn status(e: &Error) -> Option<StatusCode> {
    e.http_status()
}
fn code(e: &Error) -> Option<String> {
    match e.kind() {
        azure_core::error::ErrorKind::HttpResponse { error_code, .. } => error_code.clone(),
        _ => None,
    }
}

async fn set(c: &SecretClient, name: &str, value: &str) -> azure_security_keyvault_secrets::models::Secret {
    let p = SetSecretParameters { value: Some(value.into()), ..Default::default() };
    c.set_secret(name, RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap()
}

// ================================ secrets ====================================================

#[tokio::test]
async fn set_secret_then_get_returns_the_value_and_a_versioned_id() {
    let v = vault();
    let c = secrets(&v);
    let s = set(&c, "db-password", "hunter2").await;
    let rid = s.resource_id().unwrap();
    assert_eq!(rid.name, "db-password");
    let version = rid.version.clone().expect("set_secret returns an id with a version");
    assert!(rid.vault_url.starts_with(&v), "{} vs {v}", rid.vault_url);
    let got = c.get_secret("db-password", None).await.unwrap().into_model().unwrap();
    assert_eq!(got.value.as_deref(), Some("hunter2"));
    assert_eq!(got.resource_id().unwrap().version, Some(version));
    assert_eq!(got.attributes.unwrap().enabled, Some(true));
}

#[tokio::test]
async fn get_missing_secret_is_404_secret_not_found() {
    let c = secrets(&vault());
    let e = c.get_secret("nope", None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    assert_eq!(code(&e).as_deref(), Some("SecretNotFound"));
}

#[tokio::test]
async fn setting_a_secret_again_creates_a_new_version_and_the_old_one_stays_readable() {
    let c = secrets(&vault());
    let v1 = set(&c, "s", "one").await.resource_id().unwrap().version.unwrap();
    let v2 = set(&c, "s", "two").await.resource_id().unwrap().version.unwrap();
    assert_ne!(v1, v2);
    let old = c
        .get_secret(
            "s",
            Some(azure_security_keyvault_secrets::models::SecretClientGetSecretOptions {
                secret_version: Some(v1.clone()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_model()
        .unwrap();
    assert_eq!(old.value.as_deref(), Some("one"));
    assert_eq!(c.get_secret("s", None).await.unwrap().into_model().unwrap().value.as_deref(), Some("two"));
    let mut versions = vec![];
    let mut p = c.list_secret_properties_versions("s", None).unwrap();
    while let Some(x) = p.try_next().await.unwrap() {
        versions.push(x.resource_id().unwrap().version.unwrap());
    }
    versions.sort();
    let mut want = vec![v1, v2];
    want.sort();
    assert_eq!(versions, want);
}

#[tokio::test]
async fn list_secret_properties_returns_every_secret_name_once_even_with_several_versions() {
    let c = secrets(&vault());
    for n in ["a", "b", "c"] {
        set(&c, n, "1").await;
    }
    set(&c, "a", "2").await;
    let mut names = vec![];
    let mut p = c.list_secret_properties(None).unwrap();
    while let Some(x) = p.try_next().await.unwrap() {
        names.push(x.resource_id().unwrap().name);
    }
    names.sort();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn list_secret_properties_follows_next_link_when_maxresults_is_small() {
    let c = secrets(&vault());
    for i in 0..5 {
        set(&c, &format!("n{i}"), "x").await;
    }
    let o = azure_security_keyvault_secrets::models::SecretClientListSecretPropertiesOptions { maxresults: Some(2), ..Default::default() };
    let mut pages = c.list_secret_properties(Some(o)).unwrap().into_pages();
    let mut sizes = vec![];
    while let Some(p) = pages.try_next().await.unwrap() {
        sizes.push(p.into_model().unwrap().value.len());
    }
    assert_eq!(sizes, vec![2, 2, 1]);
}

#[tokio::test]
async fn update_secret_properties_changes_tags_and_content_type_but_not_the_value() {
    let c = secrets(&vault());
    set(&c, "u", "keep").await;
    let p = UpdateSecretPropertiesParameters {
        content_type: Some("text/plain".into()),
        tags: Some(HashMap::from([("env".to_string(), "dev".to_string())])),
        ..Default::default()
    };
    let r = c.update_secret_properties("u", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap();
    assert_eq!(r.content_type.as_deref(), Some("text/plain"));
    let got = c.get_secret("u", None).await.unwrap().into_model().unwrap();
    assert_eq!(got.value.as_deref(), Some("keep"));
    assert_eq!(got.tags.unwrap().get("env").map(String::as_str), Some("dev"));
}

#[tokio::test]
async fn disabled_secret_cannot_be_read_403_secret_disabled() {
    let c = secrets(&vault());
    set(&c, "d", "v").await;
    let p = UpdateSecretPropertiesParameters {
        secret_attributes: Some(SecretAttributes { enabled: Some(false), ..Default::default() }),
        ..Default::default()
    };
    c.update_secret_properties("d", RequestContent::try_from(p).unwrap(), None).await.unwrap();
    let e = c.get_secret("d", None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::Forbidden));
    assert_eq!(code(&e).as_deref(), Some("Forbidden"));
}

#[tokio::test]
async fn secret_with_expiry_and_not_before_returns_them_as_unix_times() {
    let c = secrets(&vault());
    let exp = OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap();
    let nbf = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let p = SetSecretParameters {
        value: Some("v".into()),
        secret_attributes: Some(SecretAttributes { expires: Some(exp), not_before: Some(nbf), ..Default::default() }),
        content_type: Some("application/x-test".into()),
        tags: Some(HashMap::from([("k".to_string(), "v".to_string())])),
    };
    c.set_secret("e", RequestContent::try_from(p).unwrap(), None).await.unwrap();
    let got = c.get_secret("e", None).await.unwrap().into_model().unwrap();
    let a = got.attributes.unwrap();
    assert_eq!(a.expires, Some(exp));
    assert_eq!(a.not_before, Some(nbf));
    assert_eq!(got.content_type.as_deref(), Some("application/x-test"));
}

#[tokio::test]
async fn delete_secret_then_get_is_404_but_get_deleted_secret_works() {
    let c = secrets(&vault());
    set(&c, "del", "v").await;
    let d = c.delete_secret("del", None).await.unwrap().into_model().unwrap();
    assert!(d.deleted_date.is_some());
    assert!(d.scheduled_purge_date.is_some());
    assert!(d.recovery_id.is_some());
    let e = c.get_secret("del", None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    let dd = c.get_deleted_secret("del", None).await.unwrap().into_model().unwrap();
    // Azure returns the deleted secret's id and attributes, not its value.
    assert_eq!(dd.resource_id().unwrap().name, "del");
    assert_eq!(dd.recovery_id, d.recovery_id);
}

#[tokio::test]
async fn recover_deleted_secret_brings_it_back_with_its_value() {
    let c = secrets(&vault());
    set(&c, "r", "back").await;
    c.delete_secret("r", None).await.unwrap();
    c.recover_deleted_secret("r", None).await.unwrap();
    assert_eq!(c.get_secret("r", None).await.unwrap().into_model().unwrap().value.as_deref(), Some("back"));
}

#[tokio::test]
async fn purge_deleted_secret_removes_it_for_good() {
    let c = secrets(&vault());
    set(&c, "p", "gone").await;
    c.delete_secret("p", None).await.unwrap();
    c.purge_deleted_secret("p", None).await.unwrap();
    let e = c.get_deleted_secret("p", None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    let e = c.recover_deleted_secret("p", None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
}

#[tokio::test]
async fn purge_of_a_secret_that_is_not_deleted_is_404() {
    let c = secrets(&vault());
    set(&c, "live", "x").await;
    let e = c.purge_deleted_secret("live", None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
}

#[tokio::test]
async fn list_deleted_secret_properties_lists_only_deleted_secrets() {
    let c = secrets(&vault());
    set(&c, "keep", "x").await;
    set(&c, "dead", "x").await;
    c.delete_secret("dead", None).await.unwrap();
    let mut names = vec![];
    let mut p = c.list_deleted_secret_properties(None).unwrap();
    while let Some(x) = p.try_next().await.unwrap() {
        names.push(x.resource_id().unwrap().name);
    }
    assert_eq!(names, vec!["dead"]);
}

#[tokio::test]
async fn backup_then_restore_into_another_vault_keeps_the_secret() {
    let a = secrets(&vault());
    set(&a, "bk", "backed up").await;
    let blob = a.backup_secret("bk", None).await.unwrap().into_model().unwrap().value.unwrap();
    let b = secrets(&vault());
    let p = RestoreSecretParameters { secret_backup: Some(blob) };
    b.restore_secret(RequestContent::try_from(p).unwrap(), None).await.unwrap();
    assert_eq!(b.get_secret("bk", None).await.unwrap().into_model().unwrap().value.as_deref(), Some("backed up"));
}

#[tokio::test]
async fn secret_value_of_exactly_25_kib_with_unicode_roundtrips() {
    let c = secrets(&vault());
    // Azure allows 25 KiB (25600 bytes) of UTF-8.
    let head = "héllo 日本語 \u{1F600} ";
    let value = format!("{head}{}", "x".repeat(25 * 1024 - head.len()));
    assert_eq!(value.len(), 25 * 1024);
    set(&c, "big", &value).await;
    assert_eq!(c.get_secret("big", None).await.unwrap().into_model().unwrap().value.as_deref(), Some(value.as_str()));
}

#[tokio::test]
async fn secret_name_with_underscore_is_400_bad_parameter() {
    // Azure: secret names may only contain 0-9, a-z, A-Z and -.
    let c = secrets(&vault());
    let p = SetSecretParameters { value: Some("v".into()), ..Default::default() };
    let e = c.set_secret("bad_name", RequestContent::try_from(p).unwrap(), None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::BadRequest));
    assert_eq!(code(&e).as_deref(), Some("BadParameter"));
}

#[tokio::test]
async fn vaults_are_separate_namespaces() {
    let (va, vb) = (vault(), vault());
    set(&secrets(&va), "same", "in-a").await;
    let e = secrets(&vb).get_secret("same", None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
}

// ================================ keys =======================================================

fn rsa_params(size: i32) -> CreateKeyParameters {
    CreateKeyParameters { kty: Some(KeyType::Rsa), key_size: Some(size), ..Default::default() }
}
fn ec_params(curve: CurveName) -> CreateKeyParameters {
    CreateKeyParameters { kty: Some(KeyType::Ec), curve: Some(curve), ..Default::default() }
}
async fn create_key(c: &KeyClient, name: &str, p: CreateKeyParameters) -> azure_security_keyvault_keys::models::Key {
    c.create_key(name, RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap()
}
fn digest(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
}

#[tokio::test]
async fn create_rsa_2048_key_returns_a_public_key_with_256_byte_modulus_and_exponent_65537() {
    let c = keys(&vault());
    let k = create_key(&c, "rsa", rsa_params(2048)).await;
    let jwk = k.key.unwrap();
    assert_eq!(jwk.kty, Some(KeyType::Rsa));
    assert_eq!(jwk.n.as_ref().unwrap().len(), 256);
    assert_eq!(jwk.e.as_deref(), Some(&[1u8, 0, 1][..]));
    assert!(jwk.d.is_none(), "the private exponent is never returned");
    assert!(jwk.kid.unwrap().contains("/keys/rsa/"));
    assert_eq!(k.attributes.unwrap().enabled, Some(true));
}

#[tokio::test]
async fn create_ec_p256_key_returns_32_byte_coordinates() {
    let c = keys(&vault());
    let k = create_key(&c, "ec", ec_params(CurveName::P256)).await;
    let jwk = k.key.unwrap();
    assert_eq!(jwk.kty, Some(KeyType::Ec));
    assert_eq!(jwk.crv, Some(CurveName::P256));
    assert_eq!(jwk.x.as_ref().unwrap().len(), 32);
    assert_eq!(jwk.y.as_ref().unwrap().len(), 32);
}

#[tokio::test]
async fn create_ec_p384_and_p521_keys_return_coordinates_of_the_curve_size() {
    let c = keys(&vault());
    let k = create_key(&c, "p384", ec_params(CurveName::P384)).await.key.unwrap();
    assert_eq!(k.x.unwrap().len(), 48);
    let k = create_key(&c, "p521", ec_params(CurveName::P521)).await.key.unwrap();
    assert_eq!(k.x.unwrap().len(), 66);
}

#[tokio::test]
async fn get_missing_key_is_404_key_not_found() {
    let e = keys(&vault()).get_key("nope", None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    assert_eq!(code(&e).as_deref(), Some("KeyNotFound"));
}

#[tokio::test]
async fn rsa_sign_rs256_then_verify_is_true_and_a_different_digest_is_false() {
    let c = keys(&vault());
    create_key(&c, "sig", rsa_params(2048)).await;
    let d = digest(1, 32);
    let p = SignParameters { algorithm: Some(SignatureAlgorithm::Rs256), value: Some(d.clone()) };
    let sig = c.sign("sig", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap();
    let sig_bytes = sig.result.clone().unwrap();
    assert_eq!(sig_bytes.len(), 256);
    let verify = |digest: Vec<u8>| {
        let c = &c;
        let sig_bytes = sig_bytes.clone();
        async move {
            let p = VerifyParameters { algorithm: Some(SignatureAlgorithm::Rs256), digest: Some(digest), signature: Some(sig_bytes) };
            c.verify("sig", "", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap().value
        }
    };
    assert_eq!(verify(d).await, Some(true));
    assert_eq!(verify(digest(2, 32)).await, Some(false));
}

#[tokio::test]
async fn rsa_sign_ps256_then_verify_is_true() {
    let c = keys(&vault());
    create_key(&c, "pss", rsa_params(2048)).await;
    let d = digest(9, 32);
    let p = SignParameters { algorithm: Some(SignatureAlgorithm::Ps256), value: Some(d.clone()) };
    let sig = c.sign("pss", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap().result.unwrap();
    let p = VerifyParameters { algorithm: Some(SignatureAlgorithm::Ps256), digest: Some(d), signature: Some(sig) };
    assert_eq!(c.verify("pss", "", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap().value, Some(true));
}

#[tokio::test]
async fn ec_sign_es256_gives_a_64_byte_signature_that_verifies() {
    let c = keys(&vault());
    create_key(&c, "es", ec_params(CurveName::P256)).await;
    let d = digest(3, 32);
    let p = SignParameters { algorithm: Some(SignatureAlgorithm::Es256), value: Some(d.clone()) };
    let sig = c.sign("es", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap().result.unwrap();
    assert_eq!(sig.len(), 64);
    let p = VerifyParameters { algorithm: Some(SignatureAlgorithm::Es256), digest: Some(d), signature: Some(sig) };
    assert_eq!(c.verify("es", "", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap().value, Some(true));
}

#[tokio::test]
async fn ec_p384_sign_es384_gives_a_96_byte_signature() {
    let c = keys(&vault());
    create_key(&c, "es384", ec_params(CurveName::P384)).await;
    let p = SignParameters { algorithm: Some(SignatureAlgorithm::Es384), value: Some(digest(4, 48)) };
    let sig = c.sign("es384", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap().result.unwrap();
    assert_eq!(sig.len(), 96);
}

#[tokio::test]
async fn rsa_oaep_encrypt_then_decrypt_returns_the_plaintext_and_ciphertext_differs() {
    let c = keys(&vault());
    create_key(&c, "enc", rsa_params(2048)).await;
    for alg in [EncryptionAlgorithm::RsaOaep, EncryptionAlgorithm::RsaOaep256, EncryptionAlgorithm::Rsa1_5] {
        let plain = b"attack at dawn".to_vec();
        let p = KeyOperationParameters { algorithm: Some(alg.clone()), value: Some(plain.clone()), ..Default::default() };
        let enc = c.encrypt("enc", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap();
        let ct = enc.result.unwrap();
        assert_eq!(ct.len(), 256, "{alg:?}");
        assert_ne!(ct, plain);
        let p = KeyOperationParameters { algorithm: Some(alg.clone()), value: Some(ct), ..Default::default() };
        let dec = c.decrypt("enc", "", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap();
        assert_eq!(dec.result.unwrap(), plain, "{alg:?}");
    }
}

#[tokio::test]
async fn wrap_key_then_unwrap_key_returns_the_original_key_material() {
    let c = keys(&vault());
    create_key(&c, "wrap", rsa_params(2048)).await;
    let cek = digest(7, 32);
    let p = KeyOperationParameters { algorithm: Some(EncryptionAlgorithm::RsaOaep256), value: Some(cek.clone()), ..Default::default() };
    let w = c.wrap_key("wrap", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap();
    let p =
        KeyOperationParameters { algorithm: Some(EncryptionAlgorithm::RsaOaep256), value: Some(w.result.unwrap()), ..Default::default() };
    let u = c.unwrap_key("wrap", "", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap();
    assert_eq!(u.result.unwrap(), cek);
}

#[tokio::test]
async fn rotate_key_makes_a_new_version_and_the_old_version_is_still_readable() {
    let c = keys(&vault());
    let v1 = create_key(&c, "rot", rsa_params(2048)).await.key.unwrap().kid.unwrap();
    let r = c.rotate_key("rot", None).await.unwrap().into_model().unwrap();
    let v2 = r.key.unwrap().kid.unwrap();
    assert_ne!(v1, v2);
    let old_version = v1.rsplit('/').next().unwrap().to_string();
    let old = c
        .get_key(
            "rot",
            Some(azure_security_keyvault_keys::models::KeyClientGetKeyOptions { key_version: Some(old_version), ..Default::default() }),
        )
        .await
        .unwrap()
        .into_model()
        .unwrap();
    assert_eq!(old.key.unwrap().kid.unwrap(), v1);
    let mut n = 0;
    let mut p = c.list_key_properties_versions("rot", None).unwrap();
    while p.try_next().await.unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 2);
}

#[tokio::test]
async fn key_rotation_policy_has_a_rotate_action_by_default() {
    let c = keys(&vault());
    create_key(&c, "pol", rsa_params(2048)).await;
    let p = c.get_key_rotation_policy("pol", None).await.unwrap().into_model().unwrap();
    assert!(p.lifetime_actions.is_some());
}

#[tokio::test]
async fn delete_key_recover_key_and_purge_key_follow_the_soft_delete_lifecycle() {
    let c = keys(&vault());
    create_key(&c, "life", rsa_params(2048)).await;
    c.delete_key("life", None).await.unwrap();
    assert_eq!(status(&c.get_key("life", None).await.unwrap_err()), Some(StatusCode::NotFound));
    c.get_deleted_key("life", None).await.unwrap();
    c.recover_deleted_key("life", None).await.unwrap();
    c.get_key("life", None).await.unwrap();
    c.delete_key("life", None).await.unwrap();
    c.purge_deleted_key("life", None).await.unwrap();
    assert_eq!(status(&c.get_deleted_key("life", None).await.unwrap_err()), Some(StatusCode::NotFound));
}

#[tokio::test]
async fn list_key_properties_lists_every_key_name() {
    let c = keys(&vault());
    for n in ["k1", "k2"] {
        create_key(&c, n, ec_params(CurveName::P256)).await;
    }
    let mut names = vec![];
    let mut p = c.list_key_properties(None).unwrap();
    while let Some(x) = p.try_next().await.unwrap() {
        names.push(x.resource_id().unwrap().name);
    }
    names.sort();
    assert_eq!(names, vec!["k1", "k2"]);
}

#[tokio::test]
async fn sign_with_a_wrong_algorithm_for_an_ec_key_is_a_client_error() {
    let c = keys(&vault());
    create_key(&c, "wrongalg", ec_params(CurveName::P256)).await;
    let p = SignParameters { algorithm: Some(SignatureAlgorithm::Rs256), value: Some(digest(1, 32)) };
    let e = c.sign("wrongalg", RequestContent::try_from(p).unwrap(), None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::BadRequest));
}

#[tokio::test]
async fn backup_and_restore_key_into_another_vault_signs_the_same() {
    let a = keys(&vault());
    create_key(&a, "bk", rsa_params(2048)).await;
    let d = digest(5, 32);
    let p = SignParameters { algorithm: Some(SignatureAlgorithm::Rs256), value: Some(d.clone()) };
    let sig = a.sign("bk", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap().result.unwrap();
    let blob = a.backup_key("bk", None).await.unwrap().into_model().unwrap().value.unwrap();
    let b = keys(&vault());
    let p = azure_security_keyvault_keys::models::RestoreKeyParameters { key_backup: Some(blob) };
    b.restore_key(RequestContent::try_from(p).unwrap(), None).await.unwrap();
    let p = VerifyParameters { algorithm: Some(SignatureAlgorithm::Rs256), digest: Some(d), signature: Some(sig) };
    assert_eq!(b.verify("bk", "", RequestContent::try_from(p).unwrap(), None).await.unwrap().into_model().unwrap().value, Some(true));
}

// ================================ certificates ===============================================

fn self_signed(subject: &str) -> CreateCertificateParameters {
    CreateCertificateParameters {
        certificate_policy: Some(CertificatePolicy {
            x509_certificate_properties: Some(X509CertificateProperties {
                subject: Some(subject.into()),
                validity_in_months: Some(12),
                ..Default::default()
            }),
            issuer_parameters: Some(IssuerParameters { name: Some("Self".into()), ..Default::default() }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn create_self_signed_certificate_completes_and_returns_a_der_certificate_with_the_subject() {
    let c = certs(&vault());
    let cert = c
        .begin_create_certificate("selfsigned", RequestContent::try_from(self_signed("CN=azemu-rust.example")).unwrap(), None)
        .unwrap()
        .await
        .unwrap()
        .into_model()
        .unwrap();
    let der = cert.cer.unwrap();
    assert_eq!(der[0], 0x30, "DER SEQUENCE");
    assert!(der.windows(b"azemu-rust.example".len()).any(|w| w == b"azemu-rust.example"));
    assert!(cert.id.unwrap().contains("/certificates/selfsigned/"));
    assert!(cert.attributes.unwrap().enabled.unwrap_or(false));
}

#[tokio::test]
async fn get_certificate_returns_the_created_certificate_and_its_policy() {
    let c = certs(&vault());
    c.begin_create_certificate("pol", RequestContent::try_from(self_signed("CN=pol.example")).unwrap(), None).unwrap().await.unwrap();
    let got = c.get_certificate("pol", None).await.unwrap().into_model().unwrap();
    assert!(got.cer.is_some());
    let policy = c.get_certificate_policy("pol", None).await.unwrap().into_model().unwrap();
    assert_eq!(policy.issuer_parameters.unwrap().name.as_deref(), Some("Self"));
    assert_eq!(policy.x509_certificate_properties.unwrap().subject.as_deref(), Some("CN=pol.example"));
}

#[tokio::test]
async fn certificate_creation_also_creates_a_secret_and_a_key_with_the_same_name() {
    let v = vault();
    let c = certs(&v);
    c.begin_create_certificate("both", RequestContent::try_from(self_signed("CN=both.example")).unwrap(), None).unwrap().await.unwrap();
    let s = secrets(&v).get_secret("both", None).await.unwrap().into_model().unwrap();
    assert!(s.value.is_some());
    assert_eq!(s.content_type.as_deref(), Some("application/x-pkcs12"));
    let k = keys(&v).get_key("both", None).await.unwrap().into_model().unwrap();
    assert_eq!(k.key.unwrap().kty, Some(KeyType::Rsa));
}

#[tokio::test]
async fn get_missing_certificate_is_404_certificate_not_found() {
    let e = certs(&vault()).get_certificate("nope", None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    assert_eq!(code(&e).as_deref(), Some("CertificateNotFound"));
}

#[tokio::test]
async fn list_certificate_properties_lists_created_certificates() {
    let c = certs(&vault());
    for n in ["c1", "c2"] {
        c.begin_create_certificate(n, RequestContent::try_from(self_signed(&format!("CN={n}.example"))).unwrap(), None)
            .unwrap()
            .await
            .unwrap();
    }
    let mut names = vec![];
    let mut p = c.list_certificate_properties(None).unwrap();
    while let Some(x) = p.try_next().await.unwrap() {
        names.push(x.resource_id().unwrap().name);
    }
    names.sort();
    assert_eq!(names, vec!["c1", "c2"]);
}

#[tokio::test]
async fn creating_a_certificate_again_adds_a_version() {
    let c = certs(&vault());
    for _ in 0..2 {
        c.begin_create_certificate("two", RequestContent::try_from(self_signed("CN=two.example")).unwrap(), None).unwrap().await.unwrap();
    }
    let mut n = 0;
    let mut p = c.list_certificate_properties_versions("two", None).unwrap();
    while p.try_next().await.unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 2);
}

#[tokio::test]
async fn delete_certificate_then_recover_and_purge() {
    let c = certs(&vault());
    c.begin_create_certificate("life", RequestContent::try_from(self_signed("CN=life.example")).unwrap(), None).unwrap().await.unwrap();
    c.delete_certificate("life", None).await.unwrap();
    assert_eq!(status(&c.get_certificate("life", None).await.unwrap_err()), Some(StatusCode::NotFound));
    c.get_deleted_certificate("life", None).await.unwrap();
    c.recover_deleted_certificate("life", None).await.unwrap();
    c.get_certificate("life", None).await.unwrap();
    c.delete_certificate("life", None).await.unwrap();
    c.purge_deleted_certificate("life", None).await.unwrap();
    assert_eq!(status(&c.get_deleted_certificate("life", None).await.unwrap_err()), Some(StatusCode::NotFound));
}
