//! Signature checks for the development account. Other accounts are never checked (the
//! other test files use `acct1`); real client signatures are covered by the SDK tests.

use std::sync::Arc;

use azemu_core::storage::sig::hmac_sha256;
use azemu_core::storage::{DEV_ACCOUNT, DEV_ACCOUNT_KEY};
use azemu_core::testing::{TestResponse, TestServer};
use azemu_queue::QueueService;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

fn garbage() -> String {
    B64.encode([7u8; 32])
}

fn urlenc(s: &str) -> String {
    s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
}

fn detail(r: &TestResponse) -> String {
    let t = r.text();
    let a = t.find("<AuthenticationErrorDetail>").map(|i| i + 27).unwrap_or(0);
    t[a..].split("</AuthenticationErrorDetail>").next().unwrap_or("").to_string()
}

async fn dev() -> (TestServer, String) {
    let srv = TestServer::start(vec![Arc::new(QueueService::new())]).await;
    let base = srv.url("queue", DEV_ACCOUNT);
    (srv, base)
}

#[tokio::test]
async fn wrong_shared_key_signature_is_rejected_with_azures_error() {
    let (srv, base) = dev().await;
    let auth = format!("SharedKey {DEV_ACCOUNT}:{}", garbage());
    let r = srv.send("GET", &format!("{base}/?comp=list"), &[("Authorization", &auth)], "").await;
    assert_eq!(r.status, 403);
    assert_eq!(r.header("x-ms-error-code"), Some("AuthenticationFailed"));
    assert!(detail(&r).starts_with("The MAC signature found in the HTTP request"), "{}", detail(&r));
}

#[tokio::test]
async fn correct_shared_key_and_shared_key_lite_signatures_are_accepted() {
    let (srv, base) = dev().await;
    let date = "Fri, 27 Mar 2026 01:00:51 GMT";
    let key = B64.decode(DEV_ACCOUNT_KEY).unwrap();
    let sign = |sts: &str| B64.encode(hmac_sha256(&key, sts.as_bytes()));
    let full = format!("GET\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{date}\nx-ms-version:2023-11-03\n/{DEV_ACCOUNT}/\ncomp:list");
    let lite = format!("GET\n\n\n\nx-ms-date:{date}\nx-ms-version:2023-11-03\n/{DEV_ACCOUNT}/?comp=list");
    for (scheme, sts) in [("SharedKey", full), ("SharedKeyLite", lite)] {
        let auth = format!("{scheme} {DEV_ACCOUNT}:{}", sign(&sts));
        let h = [("Authorization", auth.as_str()), ("x-ms-date", date), ("x-ms-version", "2023-11-03")];
        let r = srv.send("GET", &format!("{base}/?comp=list"), &h, "").await;
        assert_eq!(r.status, 200, "{scheme}: {}", r.text());
    }
}

#[tokio::test]
async fn placeholders_and_other_accounts_are_not_checked() {
    let (srv, base) = dev().await;
    for sig in ["replay", "Sanitized"] {
        let auth = format!("SharedKey {DEV_ACCOUNT}:{sig}");
        let r = srv.send("GET", &format!("{base}/?comp=list"), &[("Authorization", &auth)], "").await;
        assert_eq!(r.status, 200, "{sig}");
    }
    let other = srv.url("queue", "someoneelse");
    let auth = format!("SharedKey someoneelse:{}", garbage());
    assert_eq!(srv.send("GET", &format!("{other}/?comp=list"), &[("Authorization", &auth)], "").await.status, 200);
}

#[tokio::test]
async fn queue_sas_signature_is_checked_per_queue() {
    let (srv, base) = dev().await;
    let key = B64.decode(DEV_ACCOUNT_KEY).unwrap();
    let (se, sv) = ("2099-01-01T00:00:00Z", "2021-08-06");
    let sts = format!("rau\n\n{se}\n/queue/{DEV_ACCOUNT}/qone\n\n\n\n{sv}");
    let sig = urlenc(&B64.encode(hmac_sha256(&key, sts.as_bytes())));
    let q = format!("sv={sv}&sp=rau&se={}&sig={sig}", se.replace(':', "%3A"));
    // right queue: passes the signature check (the queue does not exist: 404, not 403)
    let r = srv.send("GET", &format!("{base}/qone/messages?peekonly=true&{q}"), &[], "").await;
    assert_eq!(r.status, 404, "{}", r.text());
    // the same token on another queue does not
    let r = srv.send("GET", &format!("{base}/qtwo/messages?peekonly=true&{q}"), &[], "").await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (403, Some("AuthenticationFailed")));
    assert!(detail(&r).starts_with("Signature did not match"), "{}", detail(&r));
}
