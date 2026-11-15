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
    // a Shared Key request may not be more than 15 minutes old
    let now = azemu_core::time::rfc1123(azemu_core::time::OffsetDateTime::now_utc());
    let date = now.as_str();
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
async fn a_malformed_signature_is_refused_for_every_account_and_a_wrong_one_for_the_dev_account() {
    let d = dev().await;
    // not base64 (placeholders as recorded traffic carries them): 400, as Azure answers (November 2026)
    for sig in ["replay", "Sanitized", "!!not-base64!!", ""] {
        let auth = format!("SharedKey {DEV_ACCOUNT}:{sig}");
        let r = d.get("/?comp=list", &[("Authorization", &auth)]).await;
        assert_eq!((r.status, r.header("x-ms-error-code")), (400, Some("InvalidAuthenticationInfo")), "{sig}");
    }
    // base64 of any size that does not match: 403 with the MAC detail
    for sig in ["c2ln", &garbage()] {
        let auth = format!("SharedKey {DEV_ACCOUNT}:{sig}");
        let r = d.get("/?comp=list", &[("Authorization", &auth)]).await;
        assert_eq!((r.status, r.header("x-ms-error-code")), (403, Some("AuthenticationFailed")), "{sig}");
        assert!(detail(&r).contains("is not the same as any computed signature"), "{}", detail(&r));
    }
    // another account: the key is invented by azemu, so a well-formed signature passes; a malformed one does not
    let srv = TestServer::start(vec![Arc::new(BlobService::new())]).await;
    let other = srv.url("blob", "someoneelse");
    let auth = format!("SharedKey someoneelse:{}", garbage());
    let r = srv.send("GET", &format!("{other}/?comp=list"), &[("Authorization", &auth)], "").await;
    assert_eq!(r.status, 200);
    let r = srv.send("GET", &format!("{other}/?comp=list"), &[("Authorization", "SharedKey someoneelse:replay")], "").await;
    assert_eq!(r.status, 400);
    // the date: more than 15 minutes old is refused for every account, one in the future is not
    let old = "Fri, 27 Mar 2026 01:00:51 GMT";
    let r = srv.send("GET", &format!("{other}/?comp=list"), &[("Authorization", &auth), ("x-ms-date", old)], "").await;
    assert_eq!(r.status, 403);
    assert!(detail(&r).contains("Request date header too old: ") && detail(&r).contains("Fri, 27 Mar 2026 01:00:51 GMT"), "{}", detail(&r));
    let r = srv
        .send("GET", &format!("{other}/?comp=list"), &[("Authorization", &auth), ("x-ms-date", "Fri, 27 Mar 2099 01:00:51 GMT")], "")
        .await;
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
    // a signature that is not usable is refused with Azure's detail, whatever its length (November 2026)
    for (sig, why) in [
        ("Sanitized", "Signature fields not well formed."),
        ("", "sig is mandatory. Cannot be empty"),
        ("Zm9vYmFy", "Signature size is invalid"),
    ] {
        let sas = format!("sv=2021-08-06&sr=c&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig={sig}");
        let r = d.get(&format!("/c?restype=container&comp=list&{sas}"), &[]).await;
        assert_eq!((r.status, detail(&r).as_str()), (403, why), "{sig}");
    }
}

/// A SAS signed with the newest `sv` of the Go SDK (2026-12-06) and with one newer than any known
/// is checked too: the signature of the account key opens the blob, a changed permission does not.
#[tokio::test]
async fn sas_with_a_newer_sv_than_the_emulator_knows_is_checked() {
    let d = dev().await;
    let key = B64.decode(DEV_ACCOUNT_KEY).unwrap();
    for sv in ["2026-12-06", "2027-06-06"] {
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
        assert_eq!(detail(&r).contains("newer SDK"), sv == "2027-06-06", "{}", detail(&r));
    }
}

fn urlenc(s: &str) -> String {
    s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
}

#[tokio::test]
async fn user_delegation_key_is_derivable_and_signs_a_sas() {
    let d = dev().await;
    let body = "<KeyInfo><Start>2030-01-01T00:00:00Z</Start><Expiry>2030-01-03T00:00:00Z</Expiry></KeyInfo>";
    let r = d
        .srv
        .send(
            "POST",
            &format!("{}/?restype=service&comp=userdelegationkey", d.base),
            &[("Authorization", "Bearer token"), ("x-ms-version", "2023-11-03")],
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

/// `sv`, `sip`, the time window and `x-ms-version`: what Azure answered when they were tried (November 2026). The
/// account is not the development account, so no signature is compared.
#[tokio::test]
async fn sv_sip_time_window_and_version_are_checked_as_azure_does() {
    let srv = TestServer::start(vec![Arc::new(BlobService::new())]).await;
    let base = srv.url("blob", "someoneelse");
    let sig = "A".repeat(43) + "%3D";
    let get = |q: String, h: Vec<(&'static str, &'static str)>| {
        let (srv, base) = (&srv, &base);
        async move { srv.send("GET", &format!("{base}/?comp=list&{q}"), &h, "").await }
    };
    let d = |r: &TestResponse| detail(r).replace("&apos;", "'");
    // sv
    for sv in ["2099-01-01", "1999-01-01", "banana", "2012-02-11"] {
        let r = get(format!("sv={sv}&ss=b&srt=sco&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig={sig}"), vec![]).await;
        assert_eq!((r.status, d(&r).as_str()), (403, "SAS field 'sv' is not well formed."), "{sv}");
    }
    // sip
    let q = |sip: &str| format!("sv=2023-11-03&ss=b&srt=sco&sp=rl&se=2099-01-01T00%3A00%3A00Z&sip={sip}&sig={sig}");
    let r = get(q("203.0.113.9"), vec![]).await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (403, Some("AuthorizationFailure")));
    let r = get(q("banana"), vec![]).await;
    assert_eq!((r.status, d(&r).as_str()), (403, "SAS field 'sip' is not well formed."));
    assert_eq!(get(q("127.0.0.1"), vec![]).await.status, 200);
    assert_eq!(get(q("0.0.0.0-255.255.255.255"), vec![]).await.status, 200);
    // the time window: with no start the expiry is compared with now; an account SAS says "has to", a service SAS "must"
    let r = get(format!("sv=2023-11-03&ss=b&srt=sco&sp=rl&se=2000-01-01T00%3A00%3A00Z&sig={sig}"), vec![]).await;
    assert!(d(&r).starts_with("Signed expiry time [") && d(&r).contains("] has to be after signed start time ["), "{}", d(&r));
    let r = srv
        .send(
            "GET",
            &format!("{base}/c?restype=container&comp=list&sv=2023-11-03&sr=c&sp=rl&se=2000-01-01T00%3A00%3A00Z&sig={sig}"),
            &[],
            "",
        )
        .await;
    assert!(d(&r).contains("] must be after signed start time ["), "{}", d(&r));
    let r =
        get(format!("sv=2023-11-03&ss=b&srt=sco&sp=rl&st=2000-01-01T00%3A00%3A00Z&se=2001-01-01T00%3A00%3A00Z&sig={sig}"), vec![]).await;
    assert!(
        d(&r).starts_with("Signature not valid in the specified time frame: Start [Sat, 01 Jan 2000 00:00:00 GMT] - Expiry ["),
        "{}",
        d(&r)
    );
    let r =
        get(format!("sv=2023-11-03&ss=b&srt=sco&sp=rl&st=2098-01-01T00%3A00%3A00Z&se=2099-01-01T00%3A00%3A00Z&sig={sig}"), vec![]).await;
    assert!(d(&r).starts_with("Signature not valid in the specified time frame"), "{}", d(&r));
    let r =
        get(format!("sv=2023-11-03&ss=b&srt=sco&sp=rl&st=2099-01-01T00%3A00%3A00Z&se=2098-01-01T00%3A00%3A00Z&sig={sig}"), vec![]).await;
    assert!(d(&r).contains("has to be after signed start time"), "{}", d(&r));
    // x-ms-version: a date from 2008-10-27 on that is not too far ahead
    for v in ["1999-01-01", "2099-01-01", "banana", "2008-10-26", "2025-7-5"] {
        let r = get(String::new(), vec![("Authorization", "Bearer t"), ("x-ms-version", v)]).await;
        assert_eq!((r.status, r.header("x-ms-error-code")), (400, Some("InvalidHeaderValue")), "{v}");
    }
    for v in ["2008-10-27", "2009-09-19", "2025-07-05", "2026-12-06"] {
        let r = get(String::new(), vec![("Authorization", "Bearer t"), ("x-ms-version", v)]).await;
        assert_eq!(r.status, 200, "{v}");
    }
    // no x-ms-version: Shared Key is read the way it was before 2009-09-19 (a signature made the SDK way is refused)
    let sdk_way = format!("GET\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{{date}}\n/{DEV_ACCOUNT}/\ncomp:list");
    let date = azemu_core::time::rfc1123(azemu_core::time::OffsetDateTime::now_utc());
    let key = B64.decode(DEV_ACCOUNT_KEY).unwrap();
    let dev = dev().await;
    let auth = format!("SharedKey {DEV_ACCOUNT}:{}", B64.encode(hmac_sha256(&key, sdk_way.replace("{date}", &date).as_bytes())));
    assert_eq!(dev.get("/?comp=list", &[("Authorization", &auth), ("x-ms-date", &date)]).await.status, 403);
    let old_way = format!("GET\n\n\n\nx-ms-date:{date}\n/{DEV_ACCOUNT}/?comp=list");
    let auth = format!("SharedKey {DEV_ACCOUNT}:{}", B64.encode(hmac_sha256(&key, old_way.as_bytes())));
    assert_eq!(dev.get("/?comp=list", &[("Authorization", &auth), ("x-ms-date", &date)]).await.status, 200);
}

/// An anonymous Put Blob without `x-ms-blob-type` is 400, not 401: Azure reads the headers before the credential.
#[tokio::test]
async fn a_missing_blob_type_is_refused_before_the_credential() {
    let d = dev().await;
    let r = d.srv.send("PUT", &format!("{}/c/b", d.base), &[], "x").await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (400, Some("MissingRequiredHeader")));
    assert!(r.text().contains("<HeaderName>x-ms-blob-type</HeaderName>"));
    let r = d.srv.send("PUT", &format!("{}/c/b", d.base), &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert_eq!(r.status, 401);
}
