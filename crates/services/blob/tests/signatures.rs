//! Signature checks for the development account. Other accounts are never checked
//! (the other test files use `acct`); real client signatures are covered by the SDK tests.

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::storage::sig::hmac_sha256;
use azemu_core::storage::{DEV_ACCOUNT, DEV_ACCOUNT_KEY};
use azemu_core::testing::{TestResponse, TestServer};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

fn garbage() -> String {
    B64.encode([7u8; 32])
}

fn detail(r: &TestResponse) -> String {
    let t = r.text();
    let a = t.find("<AuthenticationErrorDetail>").map(|i| i + 27).unwrap_or(0);
    t[a..].split("</AuthenticationErrorDetail>").next().unwrap_or("").to_string()
}

struct Dev {
    srv: TestServer,
    base: String,
}

async fn dev() -> Dev {
    let srv = TestServer::start(vec![Arc::new(BlobService::new())]).await;
    let base = srv.url("blob", DEV_ACCOUNT);
    Dev { srv, base }
}

impl Dev {
    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> TestResponse {
        self.srv.send("GET", &format!("{}{path}", self.base), headers, "").await
    }
}

#[tokio::test]
async fn wrong_shared_key_signature_is_rejected_with_azures_error() {
    let d = dev().await;
    let auth = format!("SharedKey {DEV_ACCOUNT}:{}", garbage());
    let r = d.get("/?comp=list", &[("Authorization", &auth), ("x-ms-version", "2023-11-03")]).await;
    assert_eq!(r.status, 403);
    assert_eq!(r.header("x-ms-error-code"), Some("AuthenticationFailed"));
    assert!(r.text().contains("Server failed to authenticate the request. Make sure the value of Authorization header is formed correctly including the signature."));
    assert!(detail(&r).starts_with("The MAC signature found in the HTTP request"), "{}", detail(&r));
}

#[tokio::test]
async fn correct_shared_key_signature_is_accepted() {
    let d = dev().await;
    let date = "Fri, 27 Mar 2026 01:00:51 GMT";
    let sts = format!("GET\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{date}\nx-ms-version:2023-11-03\n/{DEV_ACCOUNT}/\ncomp:list");
    let key = B64.decode(DEV_ACCOUNT_KEY).unwrap();
    let sig = B64.encode(hmac_sha256(&key, sts.as_bytes()));
    let auth = format!("SharedKey {DEV_ACCOUNT}:{sig}");
    let h = [("Authorization", auth.as_str()), ("x-ms-date", date), ("x-ms-version", "2023-11-03")];
    assert_eq!(d.get("/?comp=list", &h).await.status, 200);
    // the same signature for another path does not pass
    assert_eq!(d.get("/c?restype=container&comp=list", &h).await.status, 403);
}

#[tokio::test]
async fn placeholders_and_other_accounts_are_not_checked() {
    let d = dev().await;
    for sig in ["replay", "Sanitized", "c2ln"] {
        let auth = format!("SharedKey {DEV_ACCOUNT}:{sig}");
        assert_eq!(d.get("/?comp=list", &[("Authorization", &auth)]).await.status, 200, "{sig}");
    }
    // another account: the key is invented by azemu, so a well-formed signature passes
    let srv = TestServer::start(vec![Arc::new(BlobService::new())]).await;
    let other = srv.url("blob", "someoneelse");
    let auth = format!("SharedKey someoneelse:{}", garbage());
    let r = srv.send("GET", &format!("{other}/?comp=list"), &[("Authorization", &auth)], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn sas_with_wrong_signature_is_authentication_failed_even_for_a_valid_window() {
    let d = dev().await;
    let sas = format!("sv=2021-08-06&sr=c&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig={}", urlenc(&garbage()));
    let r = d.get(&format!("/c?restype=container&comp=list&{sas}"), &[]).await;
    assert_eq!(r.status, 403);
    assert_eq!(r.header("x-ms-error-code"), Some("AuthenticationFailed"));
    assert!(detail(&r).starts_with("Signature did not match."), "{}", detail(&r));
    // a placeholder signature is not checked (the container does not exist: 404, not 403)
    let sas = "sv=2021-08-06&sr=c&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig=Sanitized";
    assert_eq!(d.get(&format!("/c?restype=container&comp=list&{sas}"), &[]).await.status, 404);
}

/// A SAS signed with the newest `sv` of the Go SDK (2026-12-06) and with one newer than any known
/// is checked too: the signature of the account key opens the blob, a changed permission does not.
#[tokio::test]
async fn sas_with_a_newer_sv_than_the_emulator_knows_is_checked() {
    let d = dev().await;
    let key = B64.decode(DEV_ACCOUNT_KEY).unwrap();
    for sv in ["2026-12-06", "2031-01-01"] {
        let sts = format!("r\n\n2099-01-01T00:00:00Z\n/blob/{DEV_ACCOUNT}/c/b\n\n\n\n{sv}\nb\n\n\n\n\n\n\n");
        let sig = urlenc(&B64.encode(hmac_sha256(&key, sts.as_bytes())));
        let good = format!("sv={sv}&sr=b&sp=r&se=2099-01-01T00%3A00%3A00Z&sig={sig}");
        let r = d.get(&format!("/c/b?{good}"), &[]).await;
        assert_ne!(r.status, 403, "{sv}: {}", r.text());
        // permissions changed after signing
        let bad = format!("sv={sv}&sr=b&sp=rwd&se=2099-01-01T00%3A00%3A00Z&sig={sig}");
        let r = d.get(&format!("/c/b?{bad}"), &[]).await;
        assert_eq!((r.status, r.header("x-ms-error-code")), (403, Some("AuthenticationFailed")), "{sv}");
        assert!(detail(&r).starts_with("Signature did not match."), "{}", detail(&r));
        assert_eq!(detail(&r).contains("newer SDK"), sv == "2031-01-01", "{}", detail(&r));
    }
}

fn urlenc(s: &str) -> String {
    s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
}

#[tokio::test]
async fn user_delegation_key_is_derivable_and_signs_a_sas() {
    let d = dev().await;
    let body = "<KeyInfo><Start>2030-01-01T00:00:00Z</Start><Expiry>2030-01-03T00:00:00Z</Expiry></KeyInfo>";
    let auth = format!("SharedKey {DEV_ACCOUNT}:c2ln");
    let r = d
        .srv
        .send(
            "POST",
            &format!("{}/?restype=service&comp=userdelegationkey", d.base),
            &[("Authorization", &auth), ("x-ms-version", "2023-11-03")],
            body,
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let t = r.text();
    let g = |n: &str| t.split(&format!("<{n}>")).nth(1).unwrap().split(&format!("</{n}>")).next().unwrap().to_string();
    let key = B64.decode(g("Value")).unwrap();
    let (oid, tid, st, ex, sv) = (g("SignedOid"), g("SignedTid"), g("SignedStart"), g("SignedExpiry"), g("SignedVersion"));
    let sts = format!(
        "r\n2030-01-01T00:00:00Z\n2030-01-02T00:00:00Z\n/blob/{DEV_ACCOUNT}/c/b\n{oid}\n{tid}\n{st}\n{ex}\nb\n{sv}\n\n\n\n\n\n2023-11-03\nb\n\n\n\n\n\n\n"
    );
    let sig = urlenc(&B64.encode(hmac_sha256(&key, sts.as_bytes())));
    let q = format!("sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T00%3A00%3A00Z&skoid={oid}&sktid={tid}&skt={st}&ske={ex}&sks=b&skv={sv}&sv=2023-11-03&sr=b&sig={sig}");
    // valid signature: reaches the expired-window check of the service (the clock is before 2030, so the start is in the future)
    let r = d.get(&format!("/c/b?{q}"), &[]).await;
    assert!(!detail(&r).starts_with("Signature did not match"), "{}", r.text());
    // altered blob name: signature no longer matches
    let r = d.get(&format!("/c/other?{q}"), &[]).await;
    assert!(detail(&r).starts_with("Signature did not match"), "{}", r.text());
}
