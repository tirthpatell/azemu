//! The service: properties and CORS, routing, credentials and SAS, pinned addressing, reset, what is not built.

mod common;

use std::sync::Arc;

use azemu_core::testing::{Client, TestServer};
use azemu_core::{Emulator, Service};
use azemu_file::FileService;
use common::*;

fn urlenc(s: &str) -> String {
    s.replace(':', "%3A").replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
}

#[tokio::test]
async fn service_properties_start_as_a_new_account_has_them() {
    let f = start().await;
    let r = f.send("GET", "/?restype=service&comp=properties", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.text(),
        "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><StorageServiceProperties><HourMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></HourMetrics><MinuteMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></MinuteMetrics><Cors /><ProtocolSettings><SMB><Multichannel><Enabled>false</Enabled></Multichannel></SMB></ProtocolSettings></StorageServiceProperties>"
    );
}

#[tokio::test]
async fn service_properties_are_set_by_section() {
    let f = start().await;
    let body = "<StorageServiceProperties><HourMetrics><Version>1.0</Version><Enabled>true</Enabled><IncludeAPIs>true</IncludeAPIs><RetentionPolicy><Enabled>true</Enabled><Days>7</Days></RetentionPolicy></HourMetrics><ProtocolSettings><SMB><Multichannel><Enabled>true</Enabled></Multichannel></SMB></ProtocolSettings></StorageServiceProperties>";
    assert_eq!(f.send("PUT", "/?restype=service&comp=properties", &[], body).await.status, 202);
    let t = f.send("GET", "/?restype=service&comp=properties", &[], "").await.text();
    assert!(t.contains("<HourMetrics><Version>1.0</Version><Enabled>true</Enabled><IncludeAPIs>true</IncludeAPIs><RetentionPolicy><Enabled>true</Enabled><Days>7</Days></RetentionPolicy></HourMetrics>"));
    assert!(t.contains("<MinuteMetrics><Version>1.0</Version><Enabled>false</Enabled>"), "an absent section stays");
    assert!(t.contains("<Multichannel><Enabled>true</Enabled></Multichannel>"));
    // days: 1 to 365
    for days in ["0", "366"] {
        let body = format!("<StorageServiceProperties><MinuteMetrics><Version>1.0</Version><Enabled>true</Enabled><RetentionPolicy><Enabled>true</Enabled><Days>{days}</Days></RetentionPolicy></MinuteMetrics></StorageServiceProperties>");
        assert_eq!(
            failed(&f.send("PUT", "/?restype=service&comp=properties", &[], body).await),
            (400, "InvalidXmlDocument".into()),
            "{days}"
        );
    }
}

#[tokio::test]
async fn service_property_errors_name_the_place() {
    let f = start().await;
    let put = |body: &'static str| {
        let f = &f;
        async move { f.send("PUT", "/?restype=service&comp=properties", &[], body).await }
    };
    let r = put("<StorageServiceProperties><Logging><Version>1.0</Version></Logging></StorageServiceProperties>").await;
    assert_eq!(failed(&r), (400, "InvalidXmlDocument".into()));
    let t = r.text();
    assert_eq!(tag(&t, "Reason").as_deref(), Some("Element Logging not supported for File service."));
    assert_eq!((tag(&t, "LineNumber").as_deref(), tag(&t, "LinePosition").as_deref()), (Some("1"), Some("28")));
    let r = put("<StorageServiceProperties />").await;
    assert!(tag(&r.text(), "Reason").unwrap().contains("is an invalid XmlNodeType. Line 1, position 29."));
    assert_eq!((tag(&r.text(), "LinePosition").as_deref()), Some("29"));
    assert_eq!(failed(&put("<foo").await), (400, "InvalidXmlDocument".into()));
    assert_eq!(failed(&put("<Other />").await), (400, "InvalidXmlDocument".into()));
    // an empty body is a Content-Length problem
    let r = f.send("PUT", "/?restype=service&comp=properties", &[], "").await;
    assert_eq!(failed(&r), (400, "InvalidHeaderValue".into()));
    assert!(r.text().contains("<HeaderName>Content-Length</HeaderName><HeaderValue>0</HeaderValue>"));
    // stats exist on blobs and queues only
    assert_eq!(failed(&f.send("GET", "/?restype=service&comp=stats", &[], "").await), (400, "InvalidQueryParameterValue".into()));
}

#[tokio::test]
async fn cors_rules_apply_to_preflights_and_answers() {
    let f = start().await;
    f.share("sh1").await;
    let rule = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>http://a.example</AllowedOrigins><AllowedMethods>GET,PUT</AllowedMethods><AllowedHeaders>x-ms-meta-*,Content-Type</AllowedHeaders><ExposedHeaders>x-ms-*</ExposedHeaders><MaxAgeInSeconds>60</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
    assert_eq!(f.send("PUT", "/?restype=service&comp=properties", &[], rule).await.status, 202);
    let t = f.send("GET", "/?restype=service&comp=properties", &[], "").await.text();
    assert!(t.contains("<AllowedMethods>GET,PUT</AllowedMethods><AllowedOrigins>http://a.example</AllowedOrigins><AllowedHeaders>Content-Type,x-ms-meta-*</AllowedHeaders>"));
    let pre = f
        .srv
        .send(
            "OPTIONS",
            &format!("{}/sh1?restype=share", f.base),
            &[("Origin", "http://a.example"), ("Access-Control-Request-Method", "GET")],
            "",
        )
        .await;
    assert_eq!(pre.status, 200);
    assert_eq!((pre.header("access-control-allow-origin"), pre.header("access-control-max-age")), (Some("http://a.example"), Some("60")));
    let r = f.send("GET", "/sh1?restype=share", &[("Origin", "http://a.example")], "").await;
    assert_eq!(r.header("access-control-allow-origin"), Some("http://a.example"));
    assert!(r.header("access-control-expose-headers").unwrap().contains("x-ms-request-id"));
    let no = f
        .srv
        .send(
            "OPTIONS",
            &format!("{}/sh1?restype=share", f.base),
            &[("Origin", "http://nope.example"), ("Access-Control-Request-Method", "GET")],
            "",
        )
        .await;
    assert_eq!(no.status, 403);
    // clearing
    assert_eq!(
        f.send("PUT", "/?restype=service&comp=properties", &[], "<StorageServiceProperties><Cors /></StorageServiceProperties>")
            .await
            .status,
        202
    );
    assert!(f.send("GET", "/?restype=service&comp=properties", &[], "").await.text().contains("<Cors />"));
}

#[tokio::test]
async fn routing_answers_like_azure() {
    let f = start().await;
    f.share("sh1").await;
    f.dir("sh1/d").await;
    let check = |code: u16, name: &'static str| (code, name.to_string());
    // a share needs its restype
    let r = f.send("GET", "/sh1", &[], "").await;
    assert_eq!(failed(&r), check(400, "InvalidUri"));
    assert_eq!(tag(&r.text(), "UriPath").as_deref(), Some("/sh1"));
    assert_eq!(failed(&f.send("GET", "/sh1?restype=bogus", &[], "").await), check(400, "InvalidUri"));
    assert_eq!(failed(&f.send("GET", "/", &[], "").await), check(400, "InvalidUri"));
    // verbs, comps
    assert_eq!(failed(&f.send("POST", "/sh1?restype=share", &[], "").await), check(405, "UnsupportedHttpVerb"));
    assert_eq!(failed(&f.send("PATCH", "/sh1?restype=share", &[], "").await), check(405, "UnsupportedHttpVerb"));
    assert_eq!(failed(&f.send("GET", "/sh1?restype=share&comp=lease", &[], "").await), check(405, "UnsupportedHttpVerb"));
    assert_eq!(failed(&f.send("POST", "/sh1/d/f", &[], "").await), check(405, "UnsupportedHttpVerb"));
    assert_eq!(failed(&f.send("DELETE", "/?comp=list", &[], "").await), check(405, "UnsupportedHttpVerb"));
    let r = f.send("GET", "/sh1?restype=share&comp=bogus", &[], "").await;
    assert_eq!(failed(&r), check(400, "InvalidQueryParameterValue"));
    assert!(r.text().contains("<QueryParameterName>comp</QueryParameterName><QueryParameterValue>bogus</QueryParameterValue>"));
    // for a directory or a file Azure names restype, with the value of comp
    let r = f.send("GET", "/sh1/d?comp=list", &[], "").await;
    assert!(r.text().contains("<QueryParameterName>restype</QueryParameterName><QueryParameterValue>list</QueryParameterValue>"));
    let r = f.send("PUT", "/sh1/d?restype=directory&comp=bogus", &[], "").await;
    assert!(r.text().contains("<QueryParameterName>restype</QueryParameterName><QueryParameterValue>bogus</QueryParameterValue>"));
    assert_eq!(f.send("GET", "/sh1/d?restype=directory&comp=metadata", &[], "").await.status, 200);
    // the query is case-insensitive in its names, as Azure's is not: only exact names count
    assert_eq!(f.send("GET", "/sh1?restype=share&comp=stats", &[], "").await.status, 200);
}

#[tokio::test]
async fn links_and_the_rest_that_is_not_built_answer_501_and_are_counted() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 1).await;
    for (m, p, h) in [
        ("PUT", "/sh1/l?restype=symboliclink", vec![("x-ms-link-text", "f")]),
        ("GET", "/sh1/l?restype=symboliclink", vec![]),
        ("PUT", "/sh1/h?restype=hardlink", vec![("x-ms-type", "file"), ("x-ms-file-target-file", "f")]),
    ] {
        let r = f.send(m, p, &h, "").await;
        assert_eq!(r.status, 501, "{m} {p}");
        assert_eq!(error_code(&r), "NotImplemented");
        assert!(tag(&r.text(), "Message").unwrap().starts_with("file: "));
    }
    let u = f.srv.send("GET", &format!("http://127.0.0.1:{}/_azemu/unsupported", f.srv.port()), &[], "").await.json();
    let ops: Vec<String> = u["notImplemented"].as_array().unwrap().iter().map(|e| e["operation"].as_str().unwrap().to_string()).collect();
    for want in ["File_CreateSymbolicLink", "File_GetSymbolicLink", "File_CreateHardLink"] {
        assert!(ops.contains(&want.to_string()), "{ops:?}");
    }
    // a blob source of a copy is the one thing of Copy File that is left out
    let r = f.send("PUT", "/sh1/c", &[("x-ms-copy-source", "https://acct1.blob.core.windows.net/c/b")], "").await;
    assert_eq!(r.status, 501);
}

#[tokio::test]
async fn credentials_are_checked_like_the_other_storage_services() {
    let f = start().await;
    f.share("sh1").await;
    // none: a challenge, or (an old version) nothing to learn from
    let r = f.anon("GET", "/sh1?restype=share").await;
    assert_eq!(failed(&r), (401, "NoAuthenticationInformation".into()));
    assert!(r.header("www-authenticate").unwrap().starts_with("Bearer authorization_uri=https://login.microsoftonline.com/"));
    let r = f.srv.send("GET", &format!("{}/sh1?restype=share", f.base), &[("x-ms-version", "2018-11-09")], "").await;
    assert_eq!(failed(&r), (404, "ResourceNotFound".into()));
    // a form that cannot be read
    for auth in ["Foo", "Bearer", "SharedKey acct1:replay"] {
        let r = f.srv.send("GET", &format!("{}/sh1?restype=share", f.base), &[("Authorization", auth)], "").await;
        assert_eq!(failed(&r), (400, "InvalidAuthenticationInfo".into()), "{auth}");
    }
    let r = f.srv.send("GET", &format!("{}/sh1?restype=share", f.base), &[("Authorization", "Foo bar")], "").await;
    assert_eq!(failed(&r), (403, "AuthenticationFailed".into()));
    assert_eq!(tag(&r.text(), "AuthenticationErrorDetail").as_deref(), Some("Authentication scheme Foo is not supported"));
    // a bearer token is taken, with or without the intent header
    for h in [vec![("Authorization", "Bearer t")], vec![("Authorization", "Bearer t"), ("x-ms-file-request-intent", "backup")]] {
        assert_eq!(f.srv.send("GET", &format!("{}/sh1?restype=share", f.base), &h, "").await.status, 200);
    }
    // a version Azure does not know
    let r =
        f.srv.send("GET", &format!("{}/sh1?restype=share", f.base), &[("Authorization", "Bearer t"), ("x-ms-version", "banana")], "").await;
    assert_eq!(failed(&r), (400, "InvalidHeaderValue".into()));
}

fn sas(f: &F, query: &str) -> String {
    let _ = f;
    format!("{query}&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA%3D")
}

fn in_hours(h: i64) -> String {
    let t = azemu_core::time::OffsetDateTime::now_utc() + azemu_core::time::Duration::hours(h);
    urlenc(&azemu_core::time::iso8601_secs(t))
}

#[tokio::test]
async fn a_share_sas_is_judged_by_what_it_says() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f.txt", 3).await;
    let q = |sp: &str, extra: &str| sas(&f, &format!("sv=2024-11-04&sr=s&sp={sp}&se={}{extra}", in_hours(1)));
    // read: files, not writes; list: the directories
    assert_eq!(f.anon("GET", &format!("/sh1/f.txt?{}", q("r", ""))).await.status, 200);
    assert_eq!(f.anon("GET", &format!("/sh1?restype=directory&comp=list&{}", q("rl", ""))).await.status, 200);
    assert_eq!(
        failed(&f.anon("GET", &format!("/sh1?restype=directory&comp=list&{}", q("r", ""))).await),
        (403, "AuthorizationPermissionMismatch".into())
    );
    assert_eq!(failed(&f.anon("PUT", &format!("/sh1/n.txt?{}", q("r", ""))).await), (403, "AuthorizationPermissionMismatch".into()));
    // a service SAS does not reach the share itself, or the service
    assert_eq!(
        failed(&f.anon("GET", &format!("/sh1?restype=share&{}", q("rwdlc", ""))).await),
        (403, "AuthorizationPermissionMismatch".into())
    );
    assert_eq!(failed(&f.anon("GET", &format!("/?comp=list&{}", q("rwdlc", ""))).await), (403, "AuthorizationPermissionMismatch".into()));
    // write needs the permission to create or write
    let h = [("x-ms-type", "file"), ("x-ms-content-length", "1")];
    assert_eq!(f.srv.send("PUT", &format!("{}/sh1/n.txt?{}", f.base, q("c", "")), &h, "").await.status, 201);
    assert_eq!(f.srv.send("PUT", &format!("{}/sh1/m.txt?{}", f.base, q("w", "")), &h, "").await.status, 201);
    assert_eq!(f.anon("DELETE", &format!("/sh1/m.txt?{}", q("d", ""))).await.status, 202);
    // a one-file SAS
    let one = sas(&f, &format!("sv=2024-11-04&sr=f&sp=r&se={}", in_hours(1)));
    assert_eq!(f.anon("GET", &format!("/sh1/f.txt?{one}")).await.status, 200);
    assert_eq!(
        failed(&f.anon("GET", &format!("/sh1?restype=directory&comp=list&{one}")).await),
        (403, "AuthorizationPermissionMismatch".into())
    );
}

#[tokio::test]
async fn a_sas_that_is_out_of_time_or_of_the_wrong_form_is_refused() {
    let f = start().await;
    f.share("sh1").await;
    let path = "/sh1?restype=share";
    let expired = sas(&f, &format!("sv=2024-11-04&sr=s&sp=rl&st={}&se={}", in_hours(-3), in_hours(-2)));
    let r = f.anon("GET", &format!("{path}&{expired}")).await;
    assert_eq!(failed(&r), (403, "AuthenticationFailed".into()));
    assert!(tag(&r.text(), "AuthenticationErrorDetail").unwrap().starts_with("Signature not valid in the specified time frame"));
    let future = sas(&f, &format!("sv=2024-11-04&sr=s&sp=rl&st={}&se={}", in_hours(1), in_hours(2)));
    assert_eq!(failed(&f.anon("GET", &format!("{path}&{future}")).await), (403, "AuthenticationFailed".into()));
    let backwards = sas(&f, &format!("sv=2024-11-04&sr=s&sp=rl&st={}&se={}", in_hours(2), in_hours(1)));
    assert!(tag(&f.anon("GET", &format!("{path}&{backwards}")).await.text(), "AuthenticationErrorDetail")
        .unwrap()
        .starts_with("Signed expiry time"));
    let no_expiry = sas(&f, "sv=2024-11-04&sr=s&sp=rl");
    assert_eq!(failed(&f.anon("GET", &format!("{path}&{no_expiry}")).await), (403, "AuthenticationFailed".into()));
    let https = sas(&f, &format!("sv=2024-11-04&sr=s&sp=rl&spr=https&se={}", in_hours(1)));
    assert_eq!(failed(&f.anon("GET", &format!("{path}&{https}")).await), (403, "AuthorizationProtocolMismatch".into()));
    let other_ip = sas(&f, &format!("sv=2024-11-04&sr=s&sp=rl&sip=203.0.113.9&se={}", in_hours(1)));
    assert_eq!(failed(&f.anon("GET", &format!("{path}&{other_ip}")).await), (403, "AuthorizationFailure".into()));
    // the form of the signature and of the version
    let bad_sig = format!("sv=2024-11-04&sr=s&sp=rl&se={}&sig=abc", in_hours(1));
    assert_eq!(
        tag(&f.anon("GET", &format!("{path}&{bad_sig}")).await.text(), "AuthenticationErrorDetail").as_deref(),
        Some("Signature fields not well formed.")
    );
    let bad_sv = format!("sv=1999-01-01&sr=s&sp=rl&se={}&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA%3D", in_hours(1));
    assert_eq!(failed(&f.anon("GET", &format!("{path}&{bad_sv}")).await), (403, "AuthenticationFailed".into()));
}

#[tokio::test]
async fn an_account_sas_names_services_resource_types_and_permissions() {
    let f = start().await;
    f.share("sh1").await;
    let q = |ss: &str, srt: &str, sp: &str| sas(&f, &format!("sv=2024-11-04&ss={ss}&srt={srt}&sp={sp}&se={}", in_hours(1)));
    assert_eq!(f.anon("GET", &format!("/?comp=list&{}", q("f", "s", "l"))).await.status, 200);
    assert_eq!(f.anon("GET", &format!("/?restype=service&comp=properties&{}", q("f", "s", "r"))).await.status, 200);
    assert_eq!(failed(&f.anon("GET", &format!("/?comp=list&{}", q("b", "s", "l"))).await), (403, "AuthorizationServiceMismatch".into()));
    assert_eq!(
        failed(&f.anon("GET", &format!("/?comp=list&{}", q("f", "c", "l"))).await),
        (403, "AuthorizationResourceTypeMismatch".into())
    );
    assert_eq!(failed(&f.anon("GET", &format!("/?comp=list&{}", q("f", "s", "r"))).await), (403, "AuthorizationPermissionMismatch".into()));
    assert_eq!(f.anon("GET", &format!("/sh1?restype=share&{}", q("f", "c", "r"))).await.status, 200);
    assert_eq!(f.anon("PUT", &format!("/new?restype=share&{}", q("f", "c", "c"))).await.status, 201);
    assert_eq!(f.anon("DELETE", &format!("/new?restype=share&{}", q("f", "c", "d"))).await.status, 202);
    assert_eq!(f.anon("PUT", &format!("/sh1/d?restype=directory&{}", q("f", "o", "c"))).await.status, 201);
    assert_eq!(
        failed(&f.anon("PUT", &format!("/sh1/e?restype=directory&{}", q("f", "c", "c"))).await),
        (403, "AuthorizationResourceTypeMismatch".into())
    );
}

#[tokio::test]
async fn a_stored_access_policy_supplies_what_the_token_leaves_out() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f.txt", 1).await;
    let acl = format!("<SignedIdentifiers><SignedIdentifier><Id>reader</Id><AccessPolicy><Permission>r</Permission><Expiry>{}</Expiry></AccessPolicy></SignedIdentifier></SignedIdentifiers>", azemu_core::time::iso8601_secs(azemu_core::time::OffsetDateTime::now_utc() + azemu_core::time::Duration::hours(2)));
    assert_eq!(f.send("PUT", "/sh1?restype=share&comp=acl", &[], acl).await.status, 200);
    let by_policy = sas(&f, "sv=2024-11-04&sr=s&si=reader");
    assert_eq!(f.anon("GET", &format!("/sh1/f.txt?{by_policy}")).await.status, 200);
    assert_eq!(failed(&f.anon("DELETE", &format!("/sh1/f.txt?{by_policy}")).await), (403, "AuthorizationPermissionMismatch".into()));
    let unknown = sas(&f, "sv=2024-11-04&sr=s&si=nobody");
    assert_eq!(failed(&f.anon("GET", &format!("/sh1/f.txt?{unknown}")).await), (403, "AuthenticationFailed".into()));
}

#[tokio::test]
async fn the_signature_of_the_development_account_is_compared() {
    use azemu_core::storage::sig::{hmac_sha256, service_sas_string_to_sign, SasFields, StorageService};
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    let srv = TestServer::start(vec![Arc::new(FileService::new())]).await;
    let base = srv.url("file.core", azemu_core::storage::DEV_ACCOUNT);
    let auth = [("Authorization", "Bearer t")];
    srv.send("PUT", &format!("{base}/sh1?restype=share"), &auth, "").await;
    srv.send("PUT", &format!("{base}/sh1/f.txt"), &[auth[0], ("x-ms-type", "file"), ("x-ms-content-length", "1")], "").await;
    let se = azemu_core::time::iso8601_secs(azemu_core::time::OffsetDateTime::now_utc() + azemu_core::time::Duration::hours(1));
    let f = SasFields { sv: "2024-11-04".into(), sr: "f".into(), sp: "r".into(), se: se.clone(), ..Default::default() };
    // what the Python and .NET SDKs sign: permissions, start, expiry, /file/account/path, policy, ip, protocol, version, then the five headers
    let sts = service_sas_string_to_sign(StorageService::File, azemu_core::storage::DEV_ACCOUNT, &f, "sh1/f.txt", "", None);
    assert_eq!(sts, format!("r\n\n{se}\n/file/devstoreaccount1/sh1/f.txt\n\n\n\n2024-11-04\n\n\n\n\n"));
    let key = B64.decode(azemu_core::storage::DEV_ACCOUNT_KEY).unwrap();
    let sig = B64.encode(hmac_sha256(&key, sts.as_bytes()));
    let url = |sig: &str| format!("{base}/sh1/f.txt?sv=2024-11-04&sr=f&sp=r&se={}&sig={}", urlenc(&se), urlenc(sig));
    assert_eq!(srv.send("GET", &url(&sig), &[], "").await.status, 200);
    let wrong = B64.encode(hmac_sha256(&key, b"something else"));
    let r = srv.send("GET", &url(&wrong), &[], "").await;
    assert_eq!(failed(&r), (403, "AuthenticationFailed".into()));
    assert!(tag(&r.text(), "AuthenticationErrorDetail").unwrap().starts_with("Signature did not match. String to sign used was r"));
}

#[tokio::test]
async fn the_user_delegation_key_is_given_like_queue_gives_it() {
    let f = start().await;
    let expiry = azemu_core::time::iso8601_secs(azemu_core::time::OffsetDateTime::now_utc() + azemu_core::time::Duration::hours(1));
    let r = f.send("POST", "/?restype=service&comp=userdelegationkey", &[], format!("<KeyInfo><Expiry>{expiry}</Expiry></KeyInfo>")).await;
    assert_eq!(r.status, 200);
    let t = r.text();
    assert_eq!(tag(&t, "SignedService").as_deref(), Some("f"));
    assert!(tag(&t, "Value").unwrap().len() > 40);
    let long = azemu_core::time::iso8601_secs(azemu_core::time::OffsetDateTime::now_utc() + azemu_core::time::Duration::days(8));
    assert_eq!(
        failed(
            &f.send("POST", "/?restype=service&comp=userdelegationkey", &[], format!("<KeyInfo><Expiry>{long}</Expiry></KeyInfo>")).await
        ),
        (400, "InvalidXmlDocument".into())
    );
    assert_eq!(
        failed(&f.send("POST", "/?restype=service&comp=userdelegationkey", &[], "<Wrong />").await),
        (400, "InvalidXmlDocument".into())
    );
}

#[tokio::test]
async fn path_style_account_on_a_pinned_port() {
    let dir = std::env::temp_dir().join(format!("azemu-file-pinned-{}", std::process::id()));
    let emu = Emulator::builder()
        .service(Arc::new(FileService::new()))
        .pinned("127.0.0.1:0".parse().unwrap(), "file")
        .cert_dir(dir.clone())
        .start()
        .await
        .expect("start");
    let addr = emu.pinned_addr("file").expect("pinned address");
    let base = format!("http://{addr}/devstoreaccount1");
    let c = Client::new();
    let h = [("Authorization", "Bearer token")];
    assert_eq!(c.send("PUT", &format!("{base}/pinned?restype=share"), &h, "").await.status, 201);
    assert_eq!(c.send("PUT", &format!("{base}/pinned/d?restype=directory"), &h, "").await.status, 201);
    let r = c.send("GET", &format!("{base}/?comp=list"), &h, "").await;
    assert!(r.text().contains(&format!("ServiceEndpoint=\"http://{addr}/devstoreaccount1/\"")), "{}", r.text());
    assert_eq!(all(&r.text(), "Name"), vec!["pinned"]);
    let r = c.send("GET", &format!("{base}/pinned?restype=directory&comp=list"), &h, "").await;
    assert!(r.text().contains("ShareName=\"pinned\" DirectoryPath=\"\""));
    // the account is the default of the port when the path names none
    let r = c.send("GET", &format!("http://{addr}/?comp=list"), &h, "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    // and the share list marker names the account
    c.send("PUT", &format!("{base}/pinned2?restype=share"), &h, "").await;
    let r = c.send("GET", &format!("{base}/?comp=list&maxresults=1"), &h, "").await;
    assert_eq!(tag(&r.text(), "NextMarker").as_deref(), Some("/devstoreaccount1/pinned2"));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn one_account_is_emptied_and_the_other_is_not() {
    let svc = Arc::new(FileService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let (alpha, beta) = (srv.url("file.core", "alpha"), srv.url("file.core", "beta"));
    let auth = [("Authorization", "SharedKey acct1:c2ln")];
    for base in [&alpha, &beta] {
        assert_eq!(srv.send("PUT", &format!("{base}/sh1?restype=share"), &auth, "").await.status, 201);
    }
    let before = svc.persist_generation();
    assert!(svc.reset_account("Alpha"));
    assert_ne!(svc.persist_generation(), before, "persistence must see the reset");
    assert_eq!(srv.send("GET", &format!("{alpha}/sh1?restype=share"), &auth, "").await.status, 404);
    assert_eq!(srv.send("GET", &format!("{beta}/sh1?restype=share"), &auth, "").await.status, 200);
    let r = srv.send("POST", &format!("http://127.0.0.1:{}/_azemu/reset?account=beta", srv.port()), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(srv.send("GET", &format!("{beta}/sh1?restype=share"), &auth, "").await.status, 404);
    // a full reset
    srv.send("PUT", &format!("{alpha}/sh9?restype=share"), &auth, "").await;
    srv.send("POST", &format!("http://127.0.0.1:{}/_azemu/reset", srv.port()), &[], "").await;
    assert_eq!(srv.send("GET", &format!("{alpha}/sh9?restype=share"), &auth, "").await.status, 404);
}

#[tokio::test]
async fn a_fault_rule_answers_in_the_shape_of_storage() {
    let f = start().await;
    f.share("sh1").await;
    let admin = format!("http://127.0.0.1:{}/_azemu/faults", f.srv.port());
    let rule = serde_json::json!({"service": "file", "fault": {"kind": "throttle"}});
    let r = f.srv.send("POST", &admin, &[("content-type", "application/json")], rule.to_string()).await;
    assert!(r.status < 300, "{}", r.text());
    let r = f.send("GET", "/sh1?restype=share", &[], "").await;
    assert_eq!(failed(&r), (503, "ServerBusy".into()));
    assert!(r.text().contains("<Code>ServerBusy</Code>"));
    assert_eq!(r.header("server"), Some(azemu_file::SERVER));
}
