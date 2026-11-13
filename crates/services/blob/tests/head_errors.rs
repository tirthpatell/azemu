//! HEAD error answers of Blob and Data Lake (the .NET SDK): `x-ms-error-code` and nothing about a body. Azure sends
//! neither `Content-Type` nor `Content-Length` (a recording: `test_if_blob_exists_vid`); the .NET SDK parses the empty body
//! as an XML error when it sees `application/xml`.

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::testing::TestServer;

const AUTH: (&str, &str) = ("Authorization", "SharedKey acct:c2ln");

#[tokio::test]
async fn head_of_something_missing_answers_with_the_error_code_header_only() {
    let blob = BlobService::new();
    let dfs = blob.datalake();
    let srv = TestServer::start(vec![Arc::new(blob), Arc::new(dfs)]).await;
    let b = srv.url("blob", "acct");
    let d = srv.url("dfs", "acct");
    let r = srv.send("PUT", &format!("{b}/cont1?restype=container"), &[AUTH], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    for (what, url, code) in [
        ("missing blob", format!("{b}/cont1/nothing"), "BlobNotFound"),
        ("missing container", format!("{b}/nocontainer?restype=container"), "ContainerNotFound"),
        ("missing blob in a missing container", format!("{b}/nocontainer/x"), "ContainerNotFound"),
        ("missing path (dfs)", format!("{d}/cont1/nothing"), "PathNotFound"),
    ] {
        let r = srv.send("HEAD", &url, &[AUTH], "").await;
        assert_eq!(r.status, 404, "{what}");
        assert_eq!(r.header("x-ms-error-code"), Some(code), "{what}");
        assert_eq!(r.header("content-type"), None, "{what}");
        assert_eq!(r.header("content-length"), None, "{what}");
        assert!(r.body.is_empty(), "{what}");
        // the GET of the same thing still has the XML error
        let g = srv.send("GET", &url.replace("?action=getProperties", ""), &[AUTH], "").await;
        assert_eq!(g.status, 404, "{what}");
        assert!(
            g.header("content-type").unwrap_or("").contains("xml") || g.header("content-type").unwrap_or("").contains("json"),
            "{what}"
        );
    }
    // a HEAD that works still has its length and type, and a failed condition is an error answer too
    assert_eq!(srv.send("PUT", &format!("{b}/cont1/x"), &[AUTH, ("x-ms-blob-type", "BlockBlob")], "hello").await.status, 201);
    let ok = srv.send("HEAD", &format!("{b}/cont1/x"), &[AUTH], "").await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.header("content-length"), Some("5"));
    let failed = srv.send("HEAD", &format!("{b}/cont1/x"), &[AUTH, ("If-Match", "\"nope\"")], "").await;
    assert_eq!(failed.status, 412);
    assert_eq!(failed.header("x-ms-error-code"), Some("ConditionNotMet"));
    assert_eq!(failed.header("content-type"), None);
    assert_eq!(failed.header("content-length"), None);
}
