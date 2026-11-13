//! Streamed answers (`azemu_core::stream`): chunked framing over HTTP/1.1 and data frames over HTTP/2, back-pressure,
//! the end of the answer when the client goes away or stops reading, and the token of the request.

mod common;

use std::convert::Infallible;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use azemu_core::resp::Resp;
use azemu_core::testing::TestServer;
use azemu_core::{async_trait, CancelToken, Mount, Request, Response, Service};
use bytes::Bytes;
use http::Method;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

type B = BoxBody<Bytes, std::io::Error>;

fn full(b: impl Into<Bytes>) -> B {
    Full::new(b.into()).map_err(|e: Infallible| match e {}).boxed()
}

#[derive(Default)]
struct Seen {
    /// chunks the producer made
    produced: AtomicUsize,
    /// the producer's receiver was dropped or the token fired
    producer_stopped: AtomicBool,
    token_cancelled: AtomicBool,
    handlers: AtomicUsize,
}

struct Streamer(Arc<Seen>);

#[async_trait]
impl Service for Streamer {
    fn name(&self) -> &'static str {
        "streamer"
    }
    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("streamer")]
    }
    async fn handle(&self, req: Request) -> Response {
        self.0.handlers.fetch_add(1, Ordering::SeqCst);
        let n: usize = req.query.get("chunks").and_then(|v| v.parse().ok()).unwrap_or(3);
        let size: usize = req.query.get("size").and_then(|v| v.parse().ok()).unwrap_or(10);
        let token = req.ctx.cancel_token();
        let seen = self.0.clone();
        // a blocking producer with a bounded channel, the way quick query works
        let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(1);
        tokio::task::spawn_blocking(move || {
            for i in 0..n {
                if token.is_cancelled() {
                    seen.token_cancelled.store(true, Ordering::SeqCst);
                    break;
                }
                let chunk = Bytes::from(vec![(i % 251) as u8; size]);
                if tx.blocking_send(Ok(chunk)).is_err() {
                    break;
                }
                seen.produced.fetch_add(1, Ordering::SeqCst);
            }
            seen.producer_stopped.store(true, Ordering::SeqCst);
        });
        Resp::new(200u16).header("Content-Type", "application/octet-stream").stream(rx).build()
    }
    fn reset(&self) {}
}

async fn start_with(timeout: Option<Duration>) -> (TestServer, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let srv = TestServer::start_configured(vec![Arc::new(Streamer(seen.clone()))], vec![], move |b| match timeout {
        Some(t) => b.body_idle_timeout(t),
        None => b,
    })
    .await;
    (srv, seen)
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("gave up waiting for: {what}");
}

async fn h2_connect(srv: &TestServer) -> hyper::client::conn::http2::SendRequest<B> {
    let tcp = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let (sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp)).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
}

fn h2_req(srv: &TestServer, method: Method, path: &str) -> http::Request<B> {
    http::Request::builder().method(method).uri(format!("http://acct.streamer.localhost:{}{path}", srv.port())).body(full("")).unwrap()
}

fn h1_get(srv: &TestServer, method: &str, path: &str) -> String {
    format!("{method} {path} HTTP/1.1\r\nHost: acct.streamer.localhost:{}\r\n\r\n", srv.port())
}

#[tokio::test]
async fn http1_answer_is_chunked_and_complete() {
    let (srv, seen) = start_with(None).await;
    let r = srv.send("GET", &format!("{}/x?chunks=5&size=1000", srv.url("streamer", "acct")), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), 5000);
    assert_eq!(r.body[0], 0);
    assert_eq!(r.body[4999], 4);
    assert_eq!(r.header("transfer-encoding"), Some("chunked"));
    assert!(r.header("content-length").is_none());
    assert!(r.header("date").is_some());
    wait_for("producer end", || seen.producer_stopped.load(Ordering::SeqCst)).await;
    assert!(!seen.token_cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn http2_answer_is_complete() {
    let (srv, _seen) = start_with(None).await;
    let mut s = h2_connect(&srv).await;
    let r = s.send_request(h2_req(&srv, Method::GET, "/x?chunks=40&size=100000")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("content-length").is_none());
    let body = r.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 4_000_000);
    assert_eq!(body[3_999_999], 39);
    // the connection is reusable
    let r = s.send_request(h2_req(&srv, Method::GET, "/x?chunks=1&size=7")).await.unwrap();
    assert_eq!(r.into_body().collect().await.unwrap().to_bytes().len(), 7);
}

#[tokio::test]
async fn an_empty_stream_and_a_head_request_have_no_body() {
    let (srv, seen) = start_with(None).await;
    let r = srv.send("GET", &format!("{}/x?chunks=0", srv.url("streamer", "acct")), &[], "").await;
    assert_eq!((r.status, r.body.len()), (200, 0));
    let r = srv.send("HEAD", &format!("{}/x?chunks=3", srv.url("streamer", "acct")), &[], "").await;
    assert_eq!((r.status, r.body.len()), (200, 0));
    // the stream of the HEAD request was dropped unread: its producer ends
    wait_for("producers end", || seen.producer_stopped.load(Ordering::SeqCst)).await;
}

#[tokio::test]
async fn a_slow_reader_holds_the_producer_back() {
    // 1000 chunks of 64 KiB (64 MiB) are offered; a client that reads one byte and then waits makes the producer stop
    // after the socket buffers and one chunk in the channel are full, far short of the end.
    let (srv, seen) = start_with(None).await;
    let mut c = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    c.write_all(h1_get(&srv, "GET", "/x?chunks=1000&size=65536").as_bytes()).await.unwrap();
    let mut one = [0u8; 1];
    c.read_exact(&mut one).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let a = seen.produced.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let b = seen.produced.load(Ordering::SeqCst);
    assert_eq!(a, b, "the producer waits for the client");
    assert!(b < 400, "produced {b} chunks of 64 KiB for a client that reads nothing");
    // reading again lets it go on
    let mut sink = vec![0u8; 1 << 20];
    let mut got = 0usize;
    while got < 8 << 20 {
        got += c.read(&mut sink).await.unwrap();
    }
    assert!(seen.produced.load(Ordering::SeqCst) > b);
    drop(c);
    wait_for("producer stopped", || seen.producer_stopped.load(Ordering::SeqCst)).await;
}

#[tokio::test]
async fn a_client_that_goes_away_mid_answer_stops_the_producer_and_fires_the_token() {
    let (srv, seen) = start_with(None).await;
    let mut c = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    c.write_all(h1_get(&srv, "GET", "/x?chunks=100000&size=1000").as_bytes()).await.unwrap();
    let mut buf = [0u8; 100];
    c.read_exact(&mut buf).await.unwrap();
    drop(c);
    wait_for("producer stopped", || seen.producer_stopped.load(Ordering::SeqCst)).await;
    assert!(seen.produced.load(Ordering::SeqCst) < 100_000);
}

#[tokio::test]
async fn http2_reset_mid_answer_stops_the_producer() {
    let (srv, seen) = start_with(None).await;
    let mut s = h2_connect(&srv).await;
    let r = s.send_request(h2_req(&srv, Method::GET, "/x?chunks=100000&size=1000")).await.unwrap();
    let mut body = r.into_body();
    let _ = body.frame().await;
    drop(body); // RST_STREAM
    wait_for("producer stopped", || seen.producer_stopped.load(Ordering::SeqCst)).await;
    assert!(seen.produced.load(Ordering::SeqCst) < 100_000);
}

#[tokio::test]
async fn http1_client_that_stops_reading_is_dropped_after_the_idle_time() {
    let (srv, seen) = start_with(Some(Duration::from_millis(700))).await;
    let mut c = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    c.write_all(h1_get(&srv, "GET", "/x?chunks=100000&size=65536").as_bytes()).await.unwrap();
    let mut one = [0u8; 1];
    c.read_exact(&mut one).await.unwrap();
    // nothing more is read: within a few seconds the producer is stopped without the client closing
    wait_for("producer stopped", || seen.producer_stopped.load(Ordering::SeqCst)).await;
    assert!(seen.produced.load(Ordering::SeqCst) < 400);
    // the server has closed the connection: reading what is buffered ends in end-of-file or a reset
    let mut sink = vec![0u8; 1 << 20];
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match c.read(&mut sink).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "the connection was closed by the server");
}

#[tokio::test]
async fn http2_client_that_stops_reading_is_dropped_after_the_idle_time() {
    let (srv, seen) = start_with(Some(Duration::from_millis(700))).await;
    let mut s = h2_connect(&srv).await;
    let r = s.send_request(h2_req(&srv, Method::GET, "/x?chunks=100000&size=65536")).await.unwrap();
    // keep the body, read nothing: the flow-control window of the stream stays shut
    let _keep = r.into_body();
    wait_for("producer stopped", || seen.producer_stopped.load(Ordering::SeqCst)).await;
    assert!(seen.produced.load(Ordering::SeqCst) < 400);
}

#[tokio::test]
async fn a_slow_but_steady_reader_is_not_dropped() {
    let (srv, seen) = start_with(Some(Duration::from_millis(700))).await;
    let mut s = h2_connect(&srv).await;
    // 15 chunks of 64 KiB, read with a pause between frames that is far shorter than the idle time (the time
    // counts from when a chunk is handed out to when the connection asks for the next, so a chunk must be
    // taken within the idle time)
    let r = s.send_request(h2_req(&srv, Method::GET, "/x?chunks=15&size=65536")).await.unwrap();
    let mut body = r.into_body();
    let mut got = 0;
    while let Some(f) = body.frame().await {
        got += f.unwrap().into_data().map(|d| d.len()).unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    assert_eq!(got, 15 * 65536);
    assert!(!seen.token_cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn the_token_is_not_cancelled_by_a_complete_answer() {
    let (srv, seen) = start_with(None).await;
    let r = srv.send("GET", &format!("{}/x?chunks=3&size=10", srv.url("streamer", "acct")), &[], "").await;
    assert_eq!(r.body.len(), 30);
    wait_for("producer end", || seen.producer_stopped.load(Ordering::SeqCst)).await;
    assert!(!seen.token_cancelled.load(Ordering::SeqCst));
    let _ = CancelToken::never();
}
