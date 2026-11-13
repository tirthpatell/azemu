mod common;
use common::*;

fn policy(id: &str, perm: &str) -> String {
    format!("<SignedIdentifier><Id>{id}</Id><AccessPolicy><Start>2021-06-08T02:05:09Z</Start><Expiry>2031-06-08T03:10:09Z</Expiry><Permission>{perm}</Permission></AccessPolicy></SignedIdentifier>")
}

async fn with_table() -> Env {
    let e = env().await;
    e.create_table("tbl").await;
    e
}

const XML: (&str, &str) = ("Content-Type", "application/xml");

#[tokio::test]
async fn acl_round_trip() {
    let e = with_table().await;
    let r = e.send("GET", "/tbl?comp=acl", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><SignedIdentifiers />");
    assert_eq!(r.header("content-type"), Some("application/xml"));

    let body = format!("<?xml version='1.0' encoding='utf-8'?>\n<SignedIdentifiers>{}</SignedIdentifiers>", policy("testid", "r"));
    let r = e.send("PUT", "/tbl?comp=acl", &[XML], &body).await;
    assert_eq!(r.status, 204);
    let r = e.send("GET", "/tbl?comp=acl", &[], "").await;
    assert_eq!(
        r.text(),
        "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><SignedIdentifiers><SignedIdentifier><Id>testid</Id><AccessPolicy><Start>2021-06-08T02:05:09.0000000Z</Start><Expiry>2031-06-08T03:10:09.0000000Z</Expiry><Permission>r</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>"
    );
    // replaced as a set, and per table
    let body = format!("<SignedIdentifiers>{}</SignedIdentifiers>", policy("second", "raud"));
    e.send("PUT", "/tbl?comp=acl", &[XML], &body).await;
    assert!(e.send("GET", "/tbl?comp=acl", &[], "").await.text().contains("<Id>second</Id>"));
    e.create_table("other").await;
    assert!(e.send("GET", "/other?comp=acl", &[], "").await.text().ends_with("<SignedIdentifiers />"));
    e.send("PUT", "/tbl?comp=acl", &[XML], "<SignedIdentifiers />").await;
    assert!(e.send("GET", "/tbl?comp=acl", &[], "").await.text().ends_with("<SignedIdentifiers />"));
}

#[tokio::test]
async fn acl_identifier_without_a_policy() {
    let e = with_table().await;
    let body = "<SignedIdentifiers><SignedIdentifier><Id>bare</Id></SignedIdentifier><SignedIdentifier><Id>empty</Id><AccessPolicy /></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(e.send("PUT", "/tbl?comp=acl", &[XML], body).await.status, 204);
    let text = e.send("GET", "/tbl?comp=acl", &[], "").await.text();
    assert!(
        text.contains("<SignedIdentifier><Id>bare</Id></SignedIdentifier><SignedIdentifier><Id>empty</Id></SignedIdentifier>"),
        "{text}"
    );
}

#[tokio::test]
async fn acl_validation_errors_are_xml() {
    let e = with_table().await;
    let six: String = (0..6).map(|i| format!("<SignedIdentifier><Id>id{i}</Id></SignedIdentifier>")).collect();
    for body in [
        format!("<SignedIdentifiers>{six}</SignedIdentifiers>"),
        "not xml".to_string(),
        "<Other/>".to_string(),
        format!("<SignedIdentifiers>{}</SignedIdentifiers>", policy("x", "z")),
    ] {
        let r = e.send("PUT", "/tbl?comp=acl", &[XML], &body).await;
        assert_eq!(r.status, 400, "{body}");
        assert_eq!(r.header("x-ms-error-code"), Some("InvalidXmlDocument"));
        let t = r.text();
        assert!(t.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?><m:error xmlns:m=\"http://schemas.microsoft.com/ado/2007/08/dataservices/metadata\"><m:code>InvalidXmlDocument</m:code><m:message xml:lang=\"en-US\">XML specified is not syntactically valid.\nRequestId:"), "{t}");
    }
    let five: String = (0..5).map(|i| format!("<SignedIdentifier><Id>id{i}</Id></SignedIdentifier>")).collect();
    assert_eq!(e.send("PUT", "/tbl?comp=acl", &[XML], &format!("<SignedIdentifiers>{five}</SignedIdentifiers>")).await.status, 204);
}

#[tokio::test]
async fn service_properties_defaults_and_partial_updates() {
    let e = with_table().await;
    let r = e.send("GET", "/?restype=service&comp=properties", &[], "").await;
    assert_eq!(r.status, 200);
    let t = r.text();
    assert!(
        t.starts_with("\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><StorageServiceProperties><Logging><Version>1.0</Version>"),
        "{t}"
    );
    assert!(t.contains("<Cors />"));

    let logging = "<StorageServiceProperties><Logging><Version>1.0</Version><Delete>true</Delete><Read>true</Read><Write>false</Write><RetentionPolicy><Enabled>true</Enabled><Days>5</Days></RetentionPolicy></Logging></StorageServiceProperties>";
    let r = e.send("PUT", "/?restype=service&comp=properties", &[XML], logging).await;
    assert_eq!(r.status, 202);
    let cors = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>a.com,b.com</AllowedOrigins><AllowedMethods>GET,PUT</AllowedMethods><AllowedHeaders>x-ms-*</AllowedHeaders><ExposedHeaders /><MaxAgeInSeconds>500</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
    assert_eq!(e.send("PUT", "/?restype=service&comp=properties", &[XML], cors).await.status, 202);
    let t = e.send("GET", "/?restype=service&comp=properties", &[], "").await.text();
    assert!(t.contains("<Logging><Version>1.0</Version><Read>true</Read><Write>false</Write><Delete>true</Delete><RetentionPolicy><Enabled>true</Enabled><Days>5</Days></RetentionPolicy></Logging>"), "{t}");
    assert!(
        t.contains("<AllowedOrigins>a.com,b.com</AllowedOrigins>")
            && t.contains("<ExposedHeaders />")
            && t.contains("<MaxAgeInSeconds>500</MaxAgeInSeconds>"),
        "{t}"
    );
    // an empty Cors element removes the rules, the logging stays
    e.send("PUT", "/?restype=service&comp=properties", &[XML], "<StorageServiceProperties><Cors /></StorageServiceProperties>").await;
    let t = e.send("GET", "/?restype=service&comp=properties", &[], "").await.text();
    assert!(t.contains("<Cors />") && t.contains("<Days>5</Days>"));
}

#[tokio::test]
async fn service_properties_validation() {
    let e = with_table().await;
    let rule = "<CorsRule><AllowedOrigins>a</AllowedOrigins><AllowedMethods>GET</AllowedMethods><AllowedHeaders /><ExposedHeaders /><MaxAgeInSeconds>0</MaxAgeInSeconds></CorsRule>";
    let metrics = |days: &str| {
        format!("<StorageServiceProperties><MinuteMetrics><Version>1.0</Version><Enabled>true</Enabled><IncludeAPIs>true</IncludeAPIs><RetentionPolicy><Enabled>true</Enabled><Days>{days}</Days></RetentionPolicy></MinuteMetrics></StorageServiceProperties>")
    };
    for body in [
        metrics("366"),
        metrics("0"),
        metrics("x"),
        format!("<StorageServiceProperties><Cors>{}</Cors></StorageServiceProperties>", rule.repeat(6)),
        "<StorageServiceProperties><Logging/></StorageServiceProperties>".into(),
        "".into(),
    ] {
        let r = e.send("PUT", "/?restype=service&comp=properties", &[XML], &body).await;
        assert_eq!((r.status, r.header("x-ms-error-code")), (400, Some("InvalidXmlDocument")), "{body}");
    }
    for ok in
        [metrics("365"), metrics("1"), format!("<StorageServiceProperties><Cors>{}</Cors></StorageServiceProperties>", rule.repeat(5))]
    {
        assert_eq!(e.send("PUT", "/?restype=service&comp=properties", &[XML], &ok).await.status, 202);
    }
    // a failed update changes nothing
    let before = e.send("GET", "/?restype=service&comp=properties", &[], "").await.text();
    e.send("PUT", "/?restype=service&comp=properties", &[XML], &metrics("999")).await;
    assert_eq!(e.send("GET", "/?restype=service&comp=properties", &[], "").await.text(), before);
}

#[tokio::test]
async fn stats() {
    let e = env().await;
    let r = e.send("GET", "/?restype=service&comp=stats", &[], "").await;
    assert_eq!(r.status, 200);
    let t = r.text();
    assert!(
        t.contains("<StorageServiceStats><GeoReplication><Status>live</Status><LastSyncTime>") && t.contains(" GMT</LastSyncTime>"),
        "{t}"
    );
}

#[tokio::test]
async fn secondary_endpoint_is_read_only() {
    let e = with_table().await;
    let sec = e.srv.url("table", "acct-secondary");
    let h = [AUTH, JSON];
    let r = e.srv.send("POST", &format!("{sec}/Tables"), &h, r#"{"TableName":"nope"}"#).await;
    assert_eq!((r.status, code(&r).as_str()), (403, "InsufficientAccountPermissions"));
    assert_eq!(msg(&r).lines().next(), Some("Write operations are not allowed."));
    for (m, p) in [("DELETE", "/Tables('tbl')"), ("PATCH", "/tbl(PartitionKey='a',RowKey='b')")] {
        let r = e.srv.send(m, &format!("{sec}{p}"), &h, "{}").await;
        assert_eq!(r.status, 403, "{m}");
    }
    // reads work and see the primary's data
    let r = e.srv.send("GET", &format!("{sec}/Tables"), &[AUTH], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["value"][0]["TableName"], "tbl");
}

#[tokio::test]
async fn requests_without_credentials() {
    let e = with_table().await;
    let url = format!("{}/Tables", e.base);
    // older API versions: 403 AuthenticationFailed
    let r = e.srv.send("GET", &url, &[("x-ms-version", "2019-02-02")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthenticationFailed"));
    let r = e.srv.send("GET", &url, &[], "").await;
    assert_eq!(r.status, 403);
    // newer versions: 401 with the bearer challenge
    let r = e.srv.send("GET", &url, &[("x-ms-version", "2020-12-06")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "InvalidAuthenticationInfo"));
    assert!(r.header("www-authenticate").unwrap().starts_with("Bearer authorization_uri=https://login.microsoftonline.com/"));
    assert!(r.header("www-authenticate").unwrap().ends_with("resource_id=https://storage.azure.com"));
    // service-level operations answer in XML
    let r = e.srv.send("GET", &format!("{}/?restype=service&comp=stats", e.base), &[("x-ms-version", "2020-12-06")], "").await;
    assert_eq!(r.status, 401);
    assert!(r.text().contains("<m:code>InvalidAuthenticationInfo</m:code>"));
    // any credentials are accepted
    for auth in ["SharedKey acct:whatever", "SharedKeyLite acct:x", "Bearer abc.def.ghi"] {
        let r = e.srv.send("GET", &url, &[("Authorization", auth)], "").await;
        assert_eq!(r.status, 200, "{auth}");
    }
}
