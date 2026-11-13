//! CORS preflight and the Access-Control headers on actual requests
//! (Microsoft Learn, "Cross-Origin Resource Sharing (CORS) support for Azure Storage").
mod common;
use common::*;

const XML: (&str, &str) = ("Content-Type", "application/xml");
const PROPS: &str = "/?restype=service&comp=properties";

async fn set_cors(e: &Env, rules: &str) {
    let body = format!("<StorageServiceProperties><Cors>{rules}</Cors></StorageServiceProperties>");
    let r = e.send("PUT", PROPS, &[XML], &body).await;
    assert_eq!(r.status, 202, "{}", r.text());
}

fn rule(origin: &str, methods: &str) -> String {
    format!("<CorsRule><AllowedOrigins>{origin}</AllowedOrigins><AllowedMethods>{methods}</AllowedMethods><AllowedHeaders>x-ms-*,content-type,dataserviceversion</AllowedHeaders><ExposedHeaders>etag,x-ms-*</ExposedHeaders><MaxAgeInSeconds>600</MaxAgeInSeconds></CorsRule>")
}

/// A preflight without any credentials.
async fn preflight(e: &Env, path: &str, origin: &str, method: &str, headers: &str) -> azemu_core::testing::TestResponse {
    let mut h = vec![("Origin", origin), ("Access-Control-Request-Method", method)];
    if !headers.is_empty() {
        h.push(("Access-Control-Request-Headers", headers));
    }
    e.srv.send("OPTIONS", &format!("{}{}", e.base, path), &h, String::new()).await
}

#[tokio::test]
async fn preflight_without_rules_is_403() {
    let e = env().await;
    let r = preflight(&e, "/Tables", "http://a.example", "GET", "").await;
    assert_eq!(r.status, 403, "{}", r.text());
    assert_eq!(r.header("x-ms-error-code"), Some("CorsPreflightFailure"));
    assert!(r.text().contains("<Code>CorsPreflightFailure</Code>"));
}

#[tokio::test]
async fn preflight_follows_the_rules() {
    let e = env().await;
    set_cors(&e, &format!("{}{}", rule("http://a.example", "GET,POST"), rule("http://*.contoso.com", "PUT,MERGE"))).await;
    let r =
        preflight(&e, "/mytable(PartitionKey='p',RowKey='r')", "http://a.example", "POST", "Content-Type, DataServiceVersion, x-ms-date")
            .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("access-control-allow-origin"), Some("http://a.example"));
    assert_eq!(r.header("access-control-allow-methods"), Some("POST"));
    assert_eq!(r.header("access-control-allow-headers"), Some("Content-Type,DataServiceVersion,x-ms-date"));
    assert_eq!(r.header("access-control-max-age"), Some("600"));
    assert_eq!(r.header("access-control-allow-credentials"), Some("true"));
    // a subdomain wildcard in the second rule
    let r = preflight(&e, "/Tables", "http://app.contoso.com", "MERGE", "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("access-control-allow-origin"), Some("http://app.contoso.com"));
    // origins are compared with case; a method only the other rule has; a header nobody allows
    for (origin, method, headers) in [
        ("http://A.example", "GET", ""),
        ("http://a.example", "MERGE", ""),
        ("http://contoso.com", "PUT", ""),
        ("http://a.example", "GET", "authorization"),
    ] {
        let r = preflight(&e, "/Tables", origin, method, headers).await;
        assert_eq!((r.status, r.header("x-ms-error-code")), (403, Some("CorsPreflightFailure")), "{origin} {method} {headers}");
    }
}

#[tokio::test]
async fn preflight_missing_required_headers_is_400() {
    let e = env().await;
    set_cors(&e, &rule("*", "GET")).await;
    let r = e.srv.send("OPTIONS", &format!("{}/Tables", e.base), &[("Access-Control-Request-Method", "GET")], String::new()).await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (400, Some("MissingRequiredHeader")));
    let r = e.srv.send("OPTIONS", &format!("{}/Tables", e.base), &[("Origin", "http://a.example")], String::new()).await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (400, Some("MissingRequiredHeader")));
}

#[tokio::test]
async fn actual_requests_with_origin_get_the_headers() {
    let e = env().await;
    e.create_table("corst").await;
    let r = e.send("GET", "/Tables", &[("Origin", "http://a.example")], "").await;
    assert!(r.header("access-control-allow-origin").is_none(), "no rules, no headers");
    set_cors(&e, &rule("http://a.example", "GET,POST")).await;
    let r = e.send("GET", "/Tables", &[("Origin", "http://a.example")], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("access-control-allow-origin"), Some("http://a.example"));
    assert_eq!(r.header("access-control-expose-headers"), Some("etag,x-ms-*"));
    assert_eq!(r.header("vary"), Some("Origin"));
    // another origin: no Allow-Origin, GET still says Vary
    let r = e.send("GET", "/Tables", &[("Origin", "http://z.example")], "").await;
    assert!(r.header("access-control-allow-origin").is_none());
    assert_eq!(r.header("vary"), Some("Origin"));
    // an error response carries them as well
    let r = e.send("GET", "/nope()", &[("Origin", "http://a.example")], "").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.header("access-control-allow-origin"), Some("http://a.example"));
    // POST has no Vary
    let r = e.insert("corst", r#"{"PartitionKey":"p","RowKey":"r"}"#).await;
    assert_eq!(r.status, 201);
    let r = e.send("POST", "/corst", &[JSON, ("Origin", "http://a.example")], r#"{"PartitionKey":"p","RowKey":"r2"}"#).await;
    assert_eq!(r.header("access-control-allow-origin"), Some("http://a.example"));
    assert!(r.header("vary").is_none());
}
