mod common;
use common::*;

fn queue_sas(perms: &str, se: i64) -> String {
    format!("sv=2021-08-06&sp={perms}&se={}&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", urlenc(&sas_time(se)))
}

fn account_sas(services: &str, types: &str, perms: &str, se: i64) -> String {
    format!(
        "sv=2021-08-06&ss={services}&srt={types}&sp={perms}&se={}&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        urlenc(&sas_time(se))
    )
}

#[tokio::test]
async fn no_credentials_is_401_with_the_challenge() {
    let q = start().await;
    q.create("authq").await;
    for (m, p) in [("GET", "/?comp=list"), ("GET", "/authq?comp=metadata"), ("POST", "/authq/messages"), ("PUT", "/authq2")] {
        let r = q.anon(m, p, "").await;
        assert_eq!((r.status, error_code(&r).as_str()), (401, "NoAuthenticationInformation"), "{m} {p}");
        assert!(tag(&r.text(), "Message").unwrap().starts_with("Server failed to authenticate the request. Please refer to"));
        assert!(r.header("www-authenticate").unwrap_or("").starts_with("Bearer authorization_uri=https://login.microsoftonline.com/"));
    }
    // a SAS-looking query without a signature is not a credential
    let r = q.anon("GET", "/authq?comp=metadata&sv=2021-08-06&sp=r&se=2099-01-01T00:00:00Z", "").await;
    assert_eq!(r.status, 401);
    // an x-ms-version from before Entra ID could sign in (older than 2019-12-12): 404, as Azure answers (November 2026)
    let r = q.srv.send("GET", &format!("{}/?comp=list", q.base), &[("x-ms-version", "2019-07-07")], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "ResourceNotFound"));
    let r = q.srv.send("GET", &format!("{}/?comp=list", q.base), &[("x-ms-version", "2019-12-12")], "").await;
    assert_eq!(r.status, 401);
}

#[tokio::test]
async fn the_form_of_the_credential_is_checked_like_azure_does() {
    let q = start().await;
    q.create("formq").await;
    let get = |auth: &'static str| {
        let q = &q;
        async move { q.srv.send("GET", &format!("{}/formq?comp=metadata", q.base), &[("Authorization", auth)], "").await }
    };
    // one word, a Bearer with no token, a Shared Key whose signature is not base64: 400
    for auth in [
        "Foo",
        "Bearer",
        "SharedKey",
        "SharedKey acct",
        "SharedKey acct:",
        "SharedKey :AAAA",
        "SharedKey acct:replay",
        "SharedKeyLite acct:Sanitized",
    ] {
        let r = get(auth).await;
        assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidAuthenticationInfo"), "{auth}");
    }
    // a scheme Azure does not know: 403 with the scheme in the detail
    let r = get("Foo bar").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthenticationFailed"));
    assert_eq!(tag(&r.text(), "AuthenticationErrorDetail").as_deref(), Some("Authentication scheme Foo is not supported"));
    // a SAS signature that is empty, not base64 or not 32 bytes, and an `sv` Azure does not take
    let se = urlenc(&sas_time(3600));
    for (sig, sv, detail) in [
        ("", "2021-08-06", "sig is mandatory. Cannot be empty"),
        ("Sanitized", "2021-08-06", "Signature fields not well formed."),
        ("Zm9vYmFy", "2021-08-06", "Signature size is invalid"),
        (
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "2099-01-01",
            "sv is either not in the correct format or is not equal or later than version 2012-02-12",
        ),
        (
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "banana",
            "sv is either not in the correct format or is not equal or later than version 2012-02-12",
        ),
    ] {
        let r = q.with_sas("GET", "/formq?comp=metadata", &format!("sv={sv}&sp=r&se={se}&sig={sig}"), "").await;
        assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthenticationFailed"), "{sig} {sv}");
        assert_eq!(tag(&r.text(), "AuthenticationErrorDetail").as_deref(), Some(detail), "{sig} {sv}");
    }
    // a Shared Key request more than 15 minutes old
    let old = "Fri, 27 Mar 2026 01:00:51 GMT";
    let r = q
        .srv
        .send("GET", &format!("{}/formq?comp=metadata", q.base), &[("Authorization", "SharedKey acct:AAAA"), ("x-ms-date", old)], "")
        .await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthenticationFailed"));
    assert!(tag(&r.text(), "AuthenticationErrorDetail").unwrap().starts_with("Request date header too old: "));
    // an x-ms-version Azure does not know
    for v in ["1999-01-01", "2099-01-01", "banana"] {
        let r = q.send("GET", "/formq?comp=metadata", &[("x-ms-version", v)], "").await;
        assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidHeaderValue"), "{v}");
        assert_eq!(tag(&r.text(), "HeaderName").as_deref(), Some("x-ms-version"));
        assert_eq!(tag(&r.text(), "HeaderValue").as_deref(), Some(v));
    }
    // sip: an address that is not the caller's is 403 AuthorizationFailure, one that is no address is AuthenticationFailed
    let sas = |sip: &str| format!("sv=2021-08-06&sp=r&se={se}&sip={sip}&sig={G}", G = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    let r = q.with_sas("GET", "/formq?comp=metadata", &sas("203.0.113.9"), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationFailure"));
    let r = q.with_sas("GET", "/formq?comp=metadata", &sas("banana"), "").await;
    assert_eq!(tag(&r.text(), "AuthenticationErrorDetail").as_deref(), Some("SAS field &apos;sip&apos; is not well formed."));
    assert_eq!(q.with_sas("GET", "/formq?comp=metadata", &sas("127.0.0.1"), "").await.status, 200);
}

#[tokio::test]
async fn shared_key_bearer_and_any_well_formed_signature_are_accepted() {
    let q = start().await;
    q.create("looseq").await;
    for auth in
        ["SharedKey someone:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "SharedKeyLite someone:AAAA", "Bearer eyJhbGciOi.fake.token"]
    {
        let r = q.srv.send("GET", &format!("{}/looseq?comp=metadata", q.base), &[("Authorization", auth)], "").await;
        assert_eq!(r.status, 200, "{auth}");
    }
}

#[tokio::test]
async fn queue_sas_permissions_decide_what_is_allowed() {
    let q = start().await;
    q.create("sasq").await;
    q.put("sasq", "m", "").await;
    let body = msg_body("via sas");

    // add only
    let sas = queue_sas("a", 3600);
    assert_eq!(q.with_sas("POST", "/sasq/messages", &sas, &body).await.status, 201);
    for (m, p) in [("GET", "/sasq/messages"), ("GET", "/sasq/messages?peekonly=true"), ("GET", "/sasq?comp=metadata")] {
        let r = q.with_sas(m, p, &sas, "").await;
        assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationPermissionMismatch"), "{p}");
    }
    // read: peek and metadata
    let sas = queue_sas("r", 3600);
    assert_eq!(q.with_sas("GET", "/sasq/messages?peekonly=true", &sas, "").await.status, 200);
    assert_eq!(q.with_sas("GET", "/sasq?comp=metadata", &sas, "").await.status, 200);
    assert_eq!(q.with_sas("GET", "/sasq/messages", &sas, "").await.status, 403);
    // process: get and delete
    let sas = queue_sas("p", 3600);
    let got = q.with_sas("GET", "/sasq/messages", &sas, "").await;
    assert_eq!(got.status, 200);
    let (id, receipt) = (tag(&got.text(), "MessageId").unwrap(), tag(&got.text(), "PopReceipt").unwrap());
    let del = format!("/sasq/messages/{id}?popreceipt={}", urlenc(&receipt));
    assert_eq!(q.with_sas("DELETE", &del, &queue_sas("a", 3600), "").await.status, 403);
    assert_eq!(q.with_sas("DELETE", &del, &sas, "").await.status, 204);
    // update
    let got = q.get("sasq", "").await;
    let (id, receipt) = (tag(&got.text(), "MessageId").unwrap(), tag(&got.text(), "PopReceipt").unwrap());
    let upd = format!("/sasq/messages/{id}?popreceipt={}&visibilitytimeout=0", urlenc(&receipt));
    assert_eq!(q.with_sas("PUT", &upd, &queue_sas("r", 3600), "").await.status, 403);
    assert_eq!(q.with_sas("PUT", &upd, &queue_sas("u", 3600), "").await.status, 204);
    // a queue SAS cannot create, delete or clear
    for (m, p) in [("PUT", "/newq1"), ("DELETE", "/sasq"), ("DELETE", "/sasq/messages"), ("GET", "/?comp=list")] {
        let r = q.with_sas(m, p, &queue_sas("raup", 3600), "").await;
        assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationPermissionMismatch"), "{m} {p}");
    }
}

#[tokio::test]
async fn expired_and_future_sas_are_authentication_failed() {
    let q = start().await;
    q.create("timeq").await;
    let r = q.with_sas("GET", "/timeq?comp=metadata", &queue_sas("r", -600), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthenticationFailed"));
    // no start is the time of the request: the expiry is before it
    assert!(tag(&r.text(), "AuthenticationErrorDetail").unwrap().starts_with("Signed expiry time ["));
    assert!(tag(&r.text(), "AuthenticationErrorDetail").unwrap().contains("] has to be after signed start time ["));
    // a start in the past and an expiry in the past: the time frame
    let past = format!(
        "sv=2021-08-06&sp=r&st={}&se={}&sig={G}",
        urlenc(&sas_time(-3600)),
        urlenc(&sas_time(-600)),
        G = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    );
    let r = q.with_sas("GET", "/timeq?comp=metadata", &past, "").await;
    assert!(tag(&r.text(), "AuthenticationErrorDetail").unwrap().starts_with("Signature not valid in the specified time frame: Start ["));
    // an expiry before the start
    let inverted = format!(
        "sv=2021-08-06&sp=r&st={}&se={}&sig={G}",
        urlenc(&sas_time(3600)),
        urlenc(&sas_time(1800)),
        G = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    );
    let r = q.with_sas("GET", "/timeq?comp=metadata", &inverted, "").await;
    assert!(tag(&r.text(), "AuthenticationErrorDetail").unwrap().starts_with("Signed expiry time ["));

    let future = format!(
        "sv=2021-08-06&sp=r&st={}&se={}&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        urlenc(&sas_time(600)),
        urlenc(&sas_time(3600))
    );
    let r = q.with_sas("GET", "/timeq?comp=metadata", &future, "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthenticationFailed"));

    let started = format!(
        "sv=2021-08-06&sp=r&st={}&se={}&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        urlenc(&sas_time(-600)),
        urlenc(&sas_time(3600))
    );
    assert_eq!(q.with_sas("GET", "/timeq?comp=metadata", &started, "").await.status, 200);

    // neither an expiry nor a stored policy
    let r = q.with_sas("GET", "/timeq?comp=metadata", "sv=2021-08-06&sp=r&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthenticationFailed"));
}

#[tokio::test]
async fn account_sas_services_resource_types_and_permissions() {
    let q = start().await;
    q.create("acctq").await;

    // everything allowed
    let all = account_sas("bfqt", "sco", "rwdlacup", 3600);
    for (m, p, status) in [
        ("GET", "/?comp=list", 200),
        ("GET", "/?restype=service&comp=properties", 200),
        ("PUT", "/acctq2", 201),
        ("GET", "/acctq?comp=metadata", 200),
        ("PUT", "/acctq?comp=metadata", 204),
        ("DELETE", "/acctq2", 204),
    ] {
        assert_eq!(q.with_sas(m, p, &all, "").await.status, status, "{m} {p}");
    }

    // queue service not included in ss
    let r = q.with_sas("GET", "/?comp=list", &account_sas("bft", "sco", "rwdlacup", 3600), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationServiceMismatch"));

    // listing needs the service resource type
    let r = q.with_sas("GET", "/?comp=list", &account_sas("q", "co", "l", 3600), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationResourceTypeMismatch"));
    // messages need the object resource type
    let r = q.with_sas("POST", "/acctq/messages", &account_sas("q", "sc", "a", 3600), &msg_body("x")).await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationResourceTypeMismatch"));
    // right type, missing permission
    let r = q.with_sas("GET", "/?comp=list", &account_sas("q", "s", "r", 3600), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    let r = q.with_sas("PUT", "/acctq3", &account_sas("q", "c", "r", 3600), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));

    // object permissions: add, read, process
    let obj = account_sas("q", "o", "arp", 3600);
    assert_eq!(q.with_sas("POST", "/acctq/messages", &obj, &msg_body("x")).await.status, 201);
    assert_eq!(q.with_sas("GET", "/acctq/messages?peekonly=true", &obj, "").await.status, 200);
    assert_eq!(q.with_sas("GET", "/acctq/messages", &obj, "").await.status, 200);
    assert_eq!(q.with_sas("DELETE", "/acctq/messages", &obj, "").await.status, 403, "clear needs delete");
    assert_eq!(q.with_sas("DELETE", "/acctq/messages", &account_sas("q", "o", "d", 3600), "").await.status, 204);

    // expired
    let r = q.with_sas("GET", "/?comp=list", &account_sas("q", "s", "l", -60), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthenticationFailed"));
}

#[tokio::test]
async fn stored_access_policy_supplies_permissions_and_expiry() {
    let q = start().await;
    q.create("polq").await;
    q.put("polq", "m", "").await;
    let policies = format!(
        "<SignedIdentifiers>\
         <SignedIdentifier><Id>reader</Id><AccessPolicy><Start>{}</Start><Expiry>{}</Expiry><Permission>r</Permission></AccessPolicy></SignedIdentifier>\
         <SignedIdentifier><Id>old</Id><AccessPolicy><Start>{}</Start><Expiry>{}</Expiry><Permission>rap</Permission></AccessPolicy></SignedIdentifier>\
         <SignedIdentifier><Id>future</Id><AccessPolicy><Start>{}</Start><Expiry>{}</Expiry><Permission>r</Permission></AccessPolicy></SignedIdentifier>\
         <SignedIdentifier><Id>noperm</Id><AccessPolicy><Expiry>{}</Expiry></AccessPolicy></SignedIdentifier>\
         </SignedIdentifiers>",
        sas_time(-3600),
        sas_time(3600),
        sas_time(-7200),
        sas_time(-3600),
        sas_time(1800),
        sas_time(3600),
        sas_time(3600),
    );
    assert_eq!(q.send("PUT", "/polq?comp=acl", &[], &policies).await.status, 204);

    let sas = |id: &str| format!("sv=2021-08-06&si={id}&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    // permissions from the policy
    assert_eq!(q.with_sas("GET", "/polq/messages?peekonly=true", &sas("reader"), "").await.status, 200);
    let r = q.with_sas("POST", "/polq/messages", &sas("reader"), &msg_body("x")).await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    // expiry from the policy
    let r = q.with_sas("GET", "/polq/messages?peekonly=true", &sas("old"), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthenticationFailed"));
    let r = q.with_sas("GET", "/polq/messages?peekonly=true", &sas("future"), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthenticationFailed"));
    // an identifier the queue does not have
    let r = q.with_sas("GET", "/polq/messages?peekonly=true", &sas("nosuch"), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthenticationFailed"));
    // a policy without permissions allows nothing
    let r = q.with_sas("GET", "/polq/messages?peekonly=true", &sas("noperm"), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    // the token's own permissions fill in what the policy leaves out
    let both = "sv=2021-08-06&si=noperm&sp=r&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string();
    assert_eq!(q.with_sas("GET", "/polq/messages?peekonly=true", &both, "").await.status, 200);
    // deleting the policy kills the SAS
    q.send("PUT", "/polq?comp=acl", &[], "<SignedIdentifiers />").await;
    assert_eq!(q.with_sas("GET", "/polq/messages?peekonly=true", &sas("reader"), "").await.status, 403);
    // on a queue that does not exist the operation itself answers
    let r = q.with_sas("GET", "/missing1/messages?peekonly=true", &sas("reader"), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "QueueNotFound"));
}

#[tokio::test]
async fn https_only_sas_is_refused_over_http() {
    let q = start().await;
    q.create("sprq").await;
    let sas = format!("sv=2021-08-06&sp=r&spr=https&se={}&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", urlenc(&sas_time(3600)));
    let r = q.with_sas("GET", "/sprq?comp=metadata", &sas, "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (403, "AuthorizationProtocolMismatch"));
    let both = format!("sv=2021-08-06&sp=r&spr=https,http&se={}&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", urlenc(&sas_time(3600)));
    assert_eq!(q.with_sas("GET", "/sprq?comp=metadata", &both, "").await.status, 200);
}
