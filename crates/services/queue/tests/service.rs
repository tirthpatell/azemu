mod common;
use common::*;

const GET_PROPS: &str = "/?restype=service&comp=properties";

fn rule(origin: &str) -> String {
    format!("<CorsRule><AllowedOrigins>{origin}</AllowedOrigins><AllowedMethods>GET,PUT</AllowedMethods><AllowedHeaders>x-ms-meta-data*</AllowedHeaders><ExposedHeaders>x-ms-meta-abc</ExposedHeaders><MaxAgeInSeconds>500</MaxAgeInSeconds></CorsRule>")
}

fn props_body(inner: &str) -> String {
    format!("<?xml version='1.0' encoding='utf-8'?>\n<StorageServiceProperties>{inner}</StorageServiceProperties>")
}

#[tokio::test]
async fn fresh_account_reports_default_properties() {
    let q = start().await;
    let r = q.send("GET", GET_PROPS, &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/xml"));
    let b = r.text();
    assert!(b.contains("<Logging><Version>1.0</Version><Read>false</Read><Write>false</Write><Delete>false</Delete><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></Logging>"));
    assert!(b.contains("<HourMetrics><Version>1.0</Version><Enabled>true</Enabled><IncludeAPIs>true</IncludeAPIs><RetentionPolicy><Enabled>true</Enabled><Days>7</Days></RetentionPolicy></HourMetrics>"));
    assert!(b.contains("<MinuteMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></MinuteMetrics>"));
    assert!(b.ends_with("<Cors /></StorageServiceProperties>"));
}

#[tokio::test]
async fn set_properties_merges_sections_and_returns_202() {
    let q = start().await;
    let logging = "<Logging><Version>1.0</Version><Delete>true</Delete><Read>true</Read><Write>false</Write><RetentionPolicy><Enabled>true</Enabled><Days>3</Days></RetentionPolicy></Logging>";
    let r = q.send("PUT", GET_PROPS, &[], &props_body(logging)).await;
    assert_eq!(r.status, 202);
    assert!(r.body.is_empty());

    let minute = "<MinuteMetrics><Version>1.0</Version><Enabled>true</Enabled><IncludeAPIs>false</IncludeAPIs><RetentionPolicy><Enabled>true</Enabled><Days>2</Days></RetentionPolicy></MinuteMetrics>";
    assert_eq!(q.send("PUT", GET_PROPS, &[], &props_body(minute)).await.status, 202);

    let b = q.send("GET", GET_PROPS, &[], "").await.text();
    assert!(
        b.contains("<Read>true</Read><Write>false</Write><Delete>true</Delete><RetentionPolicy><Enabled>true</Enabled><Days>3</Days>"),
        "logging kept: {b}"
    );
    assert!(b.contains("<MinuteMetrics><Version>1.0</Version><Enabled>true</Enabled><IncludeAPIs>false</IncludeAPIs><RetentionPolicy><Enabled>true</Enabled><Days>2</Days>"));
    assert!(b.contains("<Days>7</Days>"), "hour metrics untouched");
}

#[tokio::test]
async fn cors_rules_round_trip_and_empty_element_clears() {
    let q = start().await;
    let body = props_body(&format!("<Cors>{}{}</Cors>", rule("www.xyz.com"), rule("a.com,b.com")));
    assert_eq!(q.send("PUT", GET_PROPS, &[], &body).await.status, 202);
    let b = q.send("GET", GET_PROPS, &[], "").await.text();
    // Azure answers methods before origins
    assert!(b.contains("<CorsRule><AllowedMethods>GET,PUT</AllowedMethods><AllowedOrigins>www.xyz.com</AllowedOrigins><AllowedHeaders>x-ms-meta-data*</AllowedHeaders><ExposedHeaders>x-ms-meta-abc</ExposedHeaders><MaxAgeInSeconds>500</MaxAgeInSeconds></CorsRule>"));
    assert_eq!(tags(&b, "AllowedOrigins"), ["www.xyz.com", "a.com,b.com"]);

    // a body without Cors leaves the rules alone
    let minute = "<HourMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></HourMetrics>";
    q.send("PUT", GET_PROPS, &[], &props_body(minute)).await;
    assert_eq!(tags(&q.send("GET", GET_PROPS, &[], "").await.text(), "AllowedOrigins").len(), 2);

    assert_eq!(q.send("PUT", GET_PROPS, &[], &props_body("<Cors />")).await.status, 202);
    assert!(q.send("GET", GET_PROPS, &[], "").await.text().contains("<Cors />"));
}

#[tokio::test]
async fn empty_header_lists_come_back_as_empty_elements() {
    let q = start().await;
    let r = "<CorsRule><AllowedOrigins>x.com</AllowedOrigins><AllowedMethods>GET</AllowedMethods><AllowedHeaders /><ExposedHeaders /><MaxAgeInSeconds>0</MaxAgeInSeconds></CorsRule>";
    q.send("PUT", GET_PROPS, &[], &props_body(&format!("<Cors>{r}</Cors>"))).await;
    let b = q.send("GET", GET_PROPS, &[], "").await.text();
    assert!(b.contains("<AllowedHeaders /><ExposedHeaders /><MaxAgeInSeconds>0</MaxAgeInSeconds>"));
}

#[tokio::test]
async fn more_than_five_cors_rules_is_invalid_xml_document() {
    let q = start().await;
    let five = props_body(&format!("<Cors>{}</Cors>", rule("a.com").repeat(5)));
    assert_eq!(q.send("PUT", GET_PROPS, &[], &five).await.status, 202);
    let six = props_body(&format!("<Cors>{}</Cors>", rule("a.com").repeat(6)));
    let r = q.send("PUT", GET_PROPS, &[], &six).await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidXmlDocument"));
    let b = r.text();
    assert!(b.contains("<LineNumber>0</LineNumber><LinePosition>0</LinePosition>"));
    // the rejected request changed nothing
    assert_eq!(tags(&q.send("GET", GET_PROPS, &[], "").await.text(), "CorsRule").len(), 5);
}

#[tokio::test]
async fn invalid_cors_rule_contents_are_rejected() {
    let q = start().await;
    let bad_method = "<CorsRule><AllowedOrigins>a.com</AllowedOrigins><AllowedMethods>FETCH</AllowedMethods><AllowedHeaders /><ExposedHeaders /><MaxAgeInSeconds>0</MaxAgeInSeconds></CorsRule>";
    let r = q.send("PUT", GET_PROPS, &[], &props_body(&format!("<Cors>{bad_method}</Cors>"))).await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidXmlDocument"));
    let no_origin = "<CorsRule><AllowedOrigins /><AllowedMethods>GET</AllowedMethods><AllowedHeaders /><ExposedHeaders /><MaxAgeInSeconds>0</MaxAgeInSeconds></CorsRule>";
    let r = q.send("PUT", GET_PROPS, &[], &props_body(&format!("<Cors>{no_origin}</Cors>"))).await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn retention_days_limit_is_365_with_position_and_reason() {
    let q = start().await;
    let metrics = |days: &str| {
        props_body(&format!("<MinuteMetrics><Version>1.0</Version><Enabled>true</Enabled><IncludeAPIs>true</IncludeAPIs><RetentionPolicy><Enabled>true</Enabled><Days>{days}</Days></RetentionPolicy></MinuteMetrics>"))
    };
    assert_eq!(q.send("PUT", GET_PROPS, &[], &metrics("365")).await.status, 202);
    let r = q.send("PUT", GET_PROPS, &[], &metrics("366")).await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidXmlDocument"));
    let b = r.text();
    // same numbers as the recording test_retention_too_long
    assert_eq!(tag(&b, "LineNumber").as_deref(), Some("2"));
    assert_eq!(tag(&b, "LinePosition").as_deref(), Some("176"));
    assert_eq!(tag(&b, "Reason").as_deref(), Some("Retention days must be greater than 0 and less than or equal to 365 days."));
    assert_eq!(q.send("PUT", GET_PROPS, &[], &metrics("0")).await.status, 400);
    // days must be given when retention is on
    let no_days = props_body("<HourMetrics><Version>1.0</Version><Enabled>true</Enabled><RetentionPolicy><Enabled>true</Enabled></RetentionPolicy></HourMetrics>");
    assert_eq!(q.send("PUT", GET_PROPS, &[], &no_days).await.status, 400);
}

#[tokio::test]
async fn empty_or_broken_properties_documents_are_invalid_xml() {
    let q = start().await;
    // recording (async) retention_too_long: an empty root, error at line 2 position 29
    let r = q.send("PUT", GET_PROPS, &[], "<?xml version='1.0' encoding='utf-8'?>\n<StorageServiceProperties />").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidXmlDocument"));
    let b = r.text();
    assert_eq!(tag(&b, "LineNumber").as_deref(), Some("2"));
    assert_eq!(tag(&b, "LinePosition").as_deref(), Some("29"));
    assert_eq!(
        tag(&b, "Reason").as_deref(),
        Some("Error parsing Xml content: &apos;None&apos; is an invalid XmlNodeType. Line 2, position 29.")
    );
    for bad in [
        "",
        "<StorageServiceProperties>",
        "<Other />",
        "<StorageServiceProperties><Logging><Read>maybe</Read></Logging></StorageServiceProperties>",
    ] {
        let r = q.send("PUT", GET_PROPS, &[], bad).await;
        assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidXmlDocument"), "{bad:?}");
    }
}

#[tokio::test]
async fn properties_are_per_account_and_reset() {
    let q = start().await;
    let logging = "<Logging><Version>1.0</Version><Delete>true</Delete><Read>true</Read><Write>true</Write><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></Logging>";
    q.send("PUT", GET_PROPS, &[], &props_body(logging)).await;
    let other = q.srv.url("queue", "acct2");
    let r = q.srv.send("GET", &format!("{other}{GET_PROPS}"), &[("Authorization", "SharedKey acct2:x")], "").await;
    assert!(r.text().contains("<Read>false</Read>"));
    q.srv.reset();
    assert!(q.send("GET", GET_PROPS, &[], "").await.text().contains("<Read>false</Read>"));
}

#[tokio::test]
async fn stats_answer_live_on_the_secondary_endpoint() {
    let q = start().await;
    let secondary = q.srv.url("queue", "acct1-secondary");
    let r = q.srv.send("GET", &format!("{secondary}/?restype=service&comp=stats"), &[("Authorization", "SharedKey acct1:x")], "").await;
    assert_eq!(r.status, 200);
    let b = r.text();
    assert!(b.contains("<StorageServiceStats><GeoReplication><Status>live</Status><LastSyncTime>"));
    assert!(tag(&b, "LastSyncTime").unwrap().ends_with(" GMT"));
}

#[tokio::test]
async fn user_delegation_key_echoes_the_requested_window() {
    let q = start().await;
    let body = "<?xml version='1.0' encoding='utf-8'?>\n<KeyInfo><Start>2025-08-21T21:01:27Z</Start><Expiry>2025-08-21T22:01:27Z</Expiry></KeyInfo>";
    let r = q.send("POST", "/?restype=service&comp=userdelegationkey", &[("x-ms-version", "2026-02-06")], body).await;
    assert_eq!(r.status, 200);
    let b = r.text();
    assert_eq!(tag(&b, "SignedStart").as_deref(), Some("2025-08-21T21:01:27Z"));
    assert_eq!(tag(&b, "SignedExpiry").as_deref(), Some("2025-08-21T22:01:27Z"));
    assert_eq!(tag(&b, "SignedService").as_deref(), Some("q"));
    assert_eq!(tag(&b, "SignedVersion").as_deref(), Some("2026-02-06"));
    assert!(!tag(&b, "Value").unwrap().is_empty());
    // expiry before start, more than 7 days, no expiry
    for bad in [
        "<KeyInfo><Start>2025-08-21T22:00:00Z</Start><Expiry>2025-08-21T21:00:00Z</Expiry></KeyInfo>",
        "<KeyInfo><Start>2025-08-21T22:00:00Z</Start><Expiry>2025-09-21T22:00:00Z</Expiry></KeyInfo>",
        "<KeyInfo><Start>2025-08-21T22:00:00Z</Start></KeyInfo>",
    ] {
        let r = q.send("POST", "/?restype=service&comp=userdelegationkey", &[], bad).await;
        assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidXmlDocument"), "{bad}");
    }
}

#[tokio::test]
async fn service_level_wrong_verbs_and_queries() {
    let q = start().await;
    let r = q.send("DELETE", GET_PROPS, &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (405, "UnsupportedHttpVerb"));
    let r = q.send("GET", "/?comp=nope", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidQueryParameterValue"));
    let r = q.send("GET", "/", &[], "").await;
    assert_eq!(r.status, 405);
}

// --- CORS (Microsoft Learn, "Cross-Origin Resource Sharing (CORS) support for Azure Storage") ---

async fn set_cors(q: &Q, rules: &str) {
    let r = q.send("PUT", GET_PROPS, &[], &props_body(&format!("<Cors>{rules}</Cors>"))).await;
    assert_eq!(r.status, 202, "{}", r.text());
}

#[tokio::test]
async fn preflight_without_rules_is_403_cors_preflight_failure() {
    let q = start().await;
    let r = q.anon_with("OPTIONS", "/someq/messages", &[("Origin", "http://a.example"), ("Access-Control-Request-Method", "GET")]).await;
    assert_eq!(r.status, 403, "{}", r.text());
    assert_eq!(error_code(&r), "CorsPreflightFailure");
}

#[tokio::test]
async fn preflight_follows_the_rules_and_needs_no_credentials() {
    let q = start().await;
    set_cors(&q, &rule("http://a.example")).await;
    // judged against the service: the queue need not exist, the request needs no Authorization
    let r = q
        .anon_with(
            "OPTIONS",
            "/nonexistent/messages",
            &[
                ("Origin", "http://a.example"),
                ("Access-Control-Request-Method", "PUT"),
                ("Access-Control-Request-Headers", "x-ms-meta-data1, x-ms-meta-data2"),
            ],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("access-control-allow-origin"), Some("http://a.example"));
    assert_eq!(r.header("access-control-allow-methods"), Some("PUT"));
    assert_eq!(r.header("access-control-allow-headers"), Some("x-ms-meta-data1,x-ms-meta-data2"));
    assert_eq!(r.header("access-control-max-age"), Some("500"));
    assert_eq!(r.header("vary"), Some("Origin"));
    assert!(r.header("x-ms-request-id").is_some());
    // each way of not matching
    for (origin, method, headers) in
        [("http://b.example", "PUT", ""), ("http://a.example", "DELETE", ""), ("http://a.example", "PUT", "authorization")]
    {
        let mut h = vec![("Origin", origin), ("Access-Control-Request-Method", method)];
        if !headers.is_empty() {
            h.push(("Access-Control-Request-Headers", headers));
        }
        let r = q.anon_with("OPTIONS", "/q/messages", &h).await;
        assert_eq!((r.status, error_code(&r).as_str()), (403, "CorsPreflightFailure"), "{origin} {method} {headers}");
    }
    // the rules are replaced by the next Set Service Properties
    set_cors(&q, &rule("http://c.example")).await;
    let r = q.anon_with("OPTIONS", "/q", &[("Origin", "http://a.example"), ("Access-Control-Request-Method", "GET")]).await;
    assert_eq!(r.status, 403);
}

#[tokio::test]
async fn preflight_missing_headers_is_400() {
    let q = start().await;
    set_cors(&q, &rule("*")).await;
    let r = q.anon_with("OPTIONS", "/q", &[("Access-Control-Request-Method", "GET")]).await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "MissingRequiredHeader"));
    assert!(r.text().contains("<HeaderName>Origin</HeaderName>"));
    let r = q.anon_with("OPTIONS", "/q", &[("Origin", "http://a.example")]).await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "MissingRequiredHeader"));
    assert!(r.text().contains("<HeaderName>Access-Control-Request-Method</HeaderName>"));
}

#[tokio::test]
async fn actual_requests_with_origin_get_the_cors_headers() {
    let q = start().await;
    q.create("corsq").await;
    // no rules: nothing is added
    let r = q.send("GET", "/corsq?comp=metadata", &[("Origin", "http://a.example")], "").await;
    assert_eq!(r.status, 200);
    assert!(r.header("access-control-allow-origin").is_none());
    set_cors(&q, &format!("{}{}", rule("http://a.example"), rule("*").replace("GET,PUT", "GET"))).await;
    let r = q.send("GET", "/corsq?comp=metadata", &[("Origin", "http://a.example")], "").await;
    assert_eq!(r.header("access-control-allow-origin"), Some("http://a.example"));
    assert_eq!(r.header("access-control-expose-headers"), Some("x-ms-meta-abc"));
    assert_eq!(r.header("vary"), Some("Origin"));
    // the wildcard rule for another origin
    let r = q.send("GET", "/corsq?comp=metadata", &[("Origin", "http://z.example")], "").await;
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
    // errors carry them too
    let r = q.send("GET", "/nothere?comp=metadata", &[("Origin", "http://a.example")], "").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.header("access-control-allow-origin"), Some("http://a.example"));
    // a method the rules do not allow, and a request without Origin
    let r = q.send("DELETE", "/corsq", &[("Origin", "http://a.example")], "").await;
    assert_eq!(r.status, 204);
    assert!(r.header("access-control-allow-origin").is_none());
    let r = q.send("GET", "/?comp=list", &[], "").await;
    assert!(r.header("access-control-allow-origin").is_none());
}
