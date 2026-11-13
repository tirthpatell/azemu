//! CORS preflight (the shared code in `azemu_core::storage::cors`, as Queue and Table use it) on the blob and dfs
//! endpoints, and the error for metadata that is not ASCII.

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::testing::TestServer;

const AUTH: (&str, &str) = ("Authorization", "SharedKey acct:c2ln");

async fn start() -> TestServer {
    let blob = BlobService::new();
    let dfs = blob.datalake();
    TestServer::start(vec![Arc::new(blob), Arc::new(dfs)]).await
}

const RULES: &str = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>https://example.com</AllowedOrigins>\
<AllowedMethods>GET,PUT</AllowedMethods><AllowedHeaders>x-ms-meta-*,content-type</AllowedHeaders>\
<ExposedHeaders>x-ms-request-id</ExposedHeaders><MaxAgeInSeconds>120</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";

#[tokio::test]
async fn preflight_follows_the_cors_rules_on_blob_and_dfs() {
    let srv = start().await;
    let r = srv.send("PUT", &format!("{}/?restype=service&comp=properties", srv.url("blob", "acct")), &[AUTH], RULES).await;
    assert_eq!(r.status, 202, "{}", r.text());

    for label in ["blob", "dfs"] {
        let url = format!("{}/c/b", srv.url(label, "acct"));
        let ok = srv
            .send(
                "OPTIONS",
                &url,
                &[
                    ("Origin", "https://example.com"),
                    ("Access-Control-Request-Method", "PUT"),
                    ("Access-Control-Request-Headers", "x-ms-meta-a, content-type"),
                ],
                "",
            )
            .await;
        assert_eq!(ok.status, 200, "{label}: {}", ok.text());
        assert_eq!(ok.header("access-control-allow-origin"), Some("https://example.com"));
        assert_eq!(ok.header("access-control-max-age"), Some("120"));
        assert!(ok.header("access-control-allow-methods").unwrap_or("").contains("PUT"));
        assert!(ok.header("access-control-allow-headers").is_some());
        assert!(ok.header("x-ms-request-id").is_some());

        // no credentials are needed, and a method or origin no rule allows is refused
        let bad_method =
            srv.send("OPTIONS", &url, &[("Origin", "https://example.com"), ("Access-Control-Request-Method", "DELETE")], "").await;
        assert_eq!(bad_method.status, 403, "{label}");
        assert_eq!(bad_method.header("x-ms-error-code"), Some("CorsPreflightFailure"));
        let bad_origin =
            srv.send("OPTIONS", &url, &[("Origin", "https://evil.example"), ("Access-Control-Request-Method", "GET")], "").await;
        assert_eq!(bad_origin.status, 403, "{label}");
        // the origin is compared as written (Azure: case sensitive)
        let other_case =
            srv.send("OPTIONS", &url, &[("Origin", "https://EXAMPLE.com"), ("Access-Control-Request-Method", "GET")], "").await;
        assert_eq!(other_case.status, 403, "{label}");
        let missing = srv.send("OPTIONS", &url, &[("Origin", "https://example.com")], "").await;
        assert_eq!(missing.status, 400, "{label}");
        assert_eq!(missing.header("x-ms-error-code"), Some("MissingRequiredHeader"));
    }

    // an actual request carrying a matching Origin gets the headers
    let r = srv.send("GET", &format!("{}/?comp=list", srv.url("blob", "acct")), &[AUTH, ("Origin", "https://example.com")], "").await;
    assert_eq!(r.header("access-control-allow-origin"), Some("https://example.com"));
    assert_eq!(r.header("access-control-expose-headers"), Some("x-ms-request-id"));
    let r = srv.send("GET", &format!("{}/?comp=list", srv.url("blob", "acct")), &[AUTH, ("Origin", "https://other.example")], "").await;
    assert!(r.header("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn preflight_without_rules_is_refused() {
    let srv = start().await;
    let r = srv
        .send(
            "OPTIONS",
            &format!("{}/c/b", srv.url("blob", "acct")),
            &[("Origin", "https://example.com"), ("Access-Control-Request-Method", "GET")],
            "",
        )
        .await;
    assert_eq!(r.status, 403);
    assert_eq!(r.header("x-ms-error-code"), Some("CorsPreflightFailure"));
}

#[tokio::test]
async fn metadata_that_is_not_ascii_is_invalid_metadata() {
    let srv = start().await;
    let c = format!("{}/cont1", srv.url("blob", "acct"));
    assert_eq!(srv.send("PUT", &format!("{c}?restype=container"), &[AUTH], "").await.status, 201);
    // Put Blob, Set Blob Metadata, Create Container
    let put = srv.send("PUT", &format!("{c}/b"), &[AUTH, ("x-ms-blob-type", "BlockBlob"), ("x-ms-meta-name", "caf\u{e9}")], "x").await;
    assert_eq!(put.status, 400, "{}", put.text());
    assert_eq!(put.header("x-ms-error-code"), Some("InvalidMetadata"));
    assert!(put.text().contains("<Code>InvalidMetadata</Code>"));
    assert_eq!(srv.send("PUT", &format!("{c}/b"), &[AUTH, ("x-ms-blob-type", "BlockBlob")], "x").await.status, 201);
    let meta = srv.send("PUT", &format!("{c}/b?comp=metadata"), &[AUTH, ("x-ms-meta-name", "caf\u{e9}")], "").await;
    assert_eq!(meta.status, 400, "{}", meta.text());
    assert_eq!(meta.header("x-ms-error-code"), Some("InvalidMetadata"));
    let cont = srv
        .send("PUT", &format!("{}/cont2?restype=container", srv.url("blob", "acct")), &[AUTH, ("x-ms-meta-name", "caf\u{e9}")], "")
        .await;
    assert_eq!(cont.status, 400, "{}", cont.text());
    assert_eq!(cont.header("x-ms-error-code"), Some("InvalidMetadata"));
    // ASCII still works
    let ok = srv.send("PUT", &format!("{c}/b?comp=metadata"), &[AUTH, ("x-ms-meta-name", "cafe")], "").await;
    assert_eq!(ok.status, 200, "{}", ok.text());
}
