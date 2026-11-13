//! No log line, at any level, carries the value of a credential. This file holds one test because it
//! installs the process-wide logger at trace level, the most that can be asked for, and reads the output back.

mod common;

use std::convert::Infallible;
use std::io::Write;
use std::sync::{Arc, Mutex};

use azemu_core::testing::TestServer;
use azemu_core::Service;
use bytes::Bytes;
use common::*;
use http::Method;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every value below must never appear in the log.
const SECRETS: &[&str] = &[
    "BEARER-SECRET-9f3a",
    "COPYSRC-SECRET-77c1",
    "ENCKEY-SECRET-1d42",
    "AEGKEY-SECRET-5b09",
    "AEGTOKEN-SECRET-8e66",
    "SBSUPP-SECRET-2a31",
    "SBDLQ-SECRET-6c85",
    "SIG-SECRET-aa11",
    "CODE-SECRET-bb22",
    "ENCODEDNAME-SECRET-cc33",
    "H2SIG-SECRET-dd44",
    "COOKIE-SECRET-ee55",
];

#[tokio::test]
async fn no_credential_reaches_the_log_at_any_level() {
    let sink = Sink::default();
    let writer = sink.clone();
    assert!(azemu_core::init_logging_with(tracing::Level::TRACE, move || writer.clone(), false), "another test installed a logger");

    let probe = Arc::new(Probe::new("echo", vec![azemu_core::Mount::AccountHost("echo")]));
    let srv = TestServer::start(vec![probe as Arc<dyn Service>]).await;
    let url = srv.url("echo", "acct");

    // HTTP/1.1: the headers and query parameters the review lists, plus a percent-encoded parameter name
    let r = srv
        .send(
            "GET",
            &format!("{url}/c?restype=container&sig=SIG-SECRET-aa11&sv=2021-08-06&code=CODE-SECRET-bb22&s%69g=ENCODEDNAME-SECRET-cc33"),
            &[
                ("Authorization", "Bearer BEARER-SECRET-9f3a"),
                ("x-ms-copy-source-authorization", "Bearer COPYSRC-SECRET-77c1"),
                ("x-ms-encryption-key", "ENCKEY-SECRET-1d42"),
                ("aeg-sas-key", "AEGKEY-SECRET-5b09"),
                ("aeg-sas-token", "AEGTOKEN-SECRET-8e66"),
                ("ServiceBusSupplementaryAuthorization", "SBSUPP-SECRET-2a31"),
                ("ServiceBusDlqSupplementaryAuthorization", "SBDLQ-SECRET-6c85"),
                ("Cookie", "session=COOKIE-SECRET-ee55"),
                ("x-ms-version", "2021-08-06"),
            ],
            "",
        )
        .await;
    assert_eq!(r.status, 200);

    // HTTP/2 with prior knowledge: the libraries underneath log the request line at trace level
    let tcp = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp)).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let body: BoxBody<Bytes, std::io::Error> = Full::new(Bytes::new()).map_err(|e: Infallible| match e {}).boxed();
    let req = http::Request::builder()
        .method(Method::GET)
        .uri(format!("{url}/c?sig=H2SIG-SECRET-dd44&comp=list"))
        .header("authorization", "Bearer BEARER-SECRET-9f3a")
        .body(body)
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await;

    // let the server write its lines, then read them back
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let log = String::from_utf8_lossy(&sink.0.lock().unwrap()).to_string();
    for secret in SECRETS {
        assert!(!log.contains(secret), "the log holds {secret}:\n{log}");
    }
    // the log still says what the request was: the point is to hide values, not lines
    assert!(log.contains("request detail"), "no debug line at all:\n{log}");
    assert!(log.contains("restype=container") && log.contains("sv=2021-08-06"), "{log}");
    assert!(log.contains("x-ms-version"), "{log}");
    assert!(log.contains("<redacted>"), "{log}");
}
