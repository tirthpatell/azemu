//! A HEAD error answer carries no body, so it must not say `Content-Type: application/xml` or a `Content-Length` for the error
//! text that is not sent (.NET SDK: the SDK parsed the empty body as the XML error and threw an `XmlException`).

use std::sync::Arc;

use azemu_core::resp::Resp;
use azemu_core::storage;
use azemu_core::testing::TestServer;
use azemu_core::{async_trait, Mount, Request, Response, Service};

struct Svc;

#[async_trait]
impl Service for Svc {
    fn name(&self) -> &'static str {
        "errs"
    }
    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("errs")]
    }
    async fn handle(&self, req: Request) -> Response {
        match req.path.as_str() {
            "/missing" => storage::error(&req.ctx, 404u16, "BlobNotFound", "The specified blob does not exist."),
            "/found" => Resp::new(200u16).header("Content-Type", "application/octet-stream").header("Content-Length", 1234).build(),
            // a service that sets the length itself on an error keeps it
            "/own-length" => Resp::new(412u16).header("Content-Length", 77).header("x-ms-error-code", "ConditionNotMet").build(),
            "/server-error" => storage::error(&req.ctx, 503u16, "ServerBusy", "busy"),
            _ => Resp::new(204u16).build(),
        }
    }
    fn reset(&self) {}
}

#[tokio::test]
async fn head_errors_have_no_content_type_and_no_computed_content_length() {
    let srv = TestServer::start(vec![Arc::new(Svc)]).await;
    let base = srv.url("errs", "acct");
    // GET keeps the XML error as before
    let r = srv.send("GET", &format!("{base}/missing"), &[], "").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.header("content-type"), Some("application/xml"));
    assert!(r.header("content-length").is_some_and(|l| l.parse::<usize>().unwrap() > 100));
    assert!(r.text().contains("<Code>BlobNotFound</Code>"));
    // HEAD of the same: the error code header and nothing about a body
    for path in ["/missing", "/server-error"] {
        let r = srv.send("HEAD", &format!("{base}{path}"), &[], "").await;
        assert!(r.status == 404 || r.status == 503, "{path}");
        assert!(r.header("x-ms-error-code").is_some(), "{path}: the error code stays in x-ms-error-code");
        assert_eq!(r.header("content-type"), None, "{path}");
        assert_eq!(r.header("content-length"), None, "{path}");
        assert!(r.body.is_empty());
    }
    // a HEAD success keeps its headers, and a length the service set on an error stays
    let r = srv.send("HEAD", &format!("{base}/found"), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
    assert_eq!(r.header("content-length"), Some("1234"));
    let r = srv.send("HEAD", &format!("{base}/own-length"), &[], "").await;
    assert_eq!(r.status, 412);
    assert_eq!(r.header("content-length"), Some("77"));
}
