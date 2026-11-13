//! A request whose client goes away is cancelled when its service asks for it (`Service::abort_on_disconnect`): the
//! handler's future is dropped (HTTP/1.1, HTTP/2 stream reset and HTTP/2 connection close) and a cancellation token
//! reaches work that runs on a blocking thread. A service that did not ask has its handler run to its end, and its
//! token stays quiet. Requests that finish normally, long uploads and downloads, WebSocket and raw connections are not
//! touched.

mod common;

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use azemu_core::resp::Resp;
use azemu_core::testing::TestServer;
use azemu_core::{async_trait, Mount, Request, Response, Service};
use bytes::Bytes;
use http::Method;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;

type B = BoxBody<Bytes, std::io::Error>;

fn full(b: impl Into<Bytes>) -> B {
    Full::new(b.into()).map_err(|e: Infallible| match e {}).boxed()
}

/// Counts what happened to the handlers it ran.
#[derive(Default)]
struct Seen {
    started: AtomicUsize,
    finished: AtomicUsize,
    dropped: AtomicUsize,
    /// Set by a blocking thread that saw the cancellation token fire.
    blocking_saw_cancel: AtomicBool,
    blocking_done: AtomicBool,
    started_note: Notify,
}

struct DropCount(Arc<Seen>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

/// The paths this service asks core to cancel when the client leaves (`abort_on_disconnect`).
struct Slow(Arc<Seen>, &'static [&'static str]);

#[async_trait]
impl Service for Slow {
    fn name(&self) -> &'static str {
        "slow"
    }
    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("slow")]
    }
    fn abort_on_disconnect(&self, _method: &Method, path: &str) -> bool {
        self.1.contains(&path)
    }
    async fn handle(&self, req: Request) -> Response {
        let seen = self.0.clone();
        match req.path.as_str() {
            "/sleep" => {
                let _d = DropCount(seen.clone());
                seen.started.fetch_add(1, Ordering::SeqCst);
                seen.started_note.notify_one();
                tokio::time::sleep(Duration::from_secs(60)).await;
                seen.finished.fetch_add(1, Ordering::SeqCst);
                Resp::new(200u16).body("late").build()
            }
            "/quick-sleep" => {
                let _d = DropCount(seen.clone());
                seen.started.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(300)).await;
                seen.finished.fetch_add(1, Ordering::SeqCst);
                Resp::new(200u16).body("done").build()
            }
            "/blocking" => {
                // like a quick query: the work runs on a blocking thread and checks the token
                let token = req.ctx.cancel_token();
                let s2 = seen.clone();
                seen.started.fetch_add(1, Ordering::SeqCst);
                seen.started_note.notify_one();
                let job = tokio::task::spawn_blocking(move || {
                    for _ in 0..600 {
                        if token.is_cancelled() {
                            s2.blocking_saw_cancel.store(true, Ordering::SeqCst);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    s2.blocking_done.store(true, Ordering::SeqCst);
                });
                let _ = job.await;
                Resp::new(200u16).body("blocking done").build()
            }
            "/blocking-short" => {
                // the same, but it ends by itself after half a second
                let token = req.ctx.cancel_token();
                let s2 = seen.clone();
                seen.started.fetch_add(1, Ordering::SeqCst);
                let job = tokio::task::spawn_blocking(move || {
                    for _ in 0..10 {
                        if token.is_cancelled() {
                            s2.blocking_saw_cancel.store(true, Ordering::SeqCst);
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    s2.blocking_done.store(true, Ordering::SeqCst);
                });
                let _ = job.await;
                seen.finished.fetch_add(1, Ordering::SeqCst);
                Resp::new(200u16).body("blocking done").build()
            }
            "/len" => Resp::new(200u16).body(req.body.len().to_string()).build(),
            "/big" => {
                let n: usize = req.query.get("n").and_then(|v| v.parse().ok()).unwrap_or(0);
                Resp::new(200u16).body(vec![7u8; n]).build()
            }
            _ => Resp::new(404u16).build(),
        }
    }
    fn reset(&self) {}
}

/// A service that asks for the cancellation of `/sleep` and `/blocking`.
async fn start() -> (TestServer, Arc<Seen>) {
    start_asking(&["/sleep", "/blocking"]).await
}

async fn start_asking(paths: &'static [&'static str]) -> (TestServer, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let srv = TestServer::start(vec![Arc::new(Slow(seen.clone(), paths))]).await;
    (srv, seen)
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("gave up waiting for: {what}");
}

async fn h1_connect(srv: &TestServer) -> TcpStream {
    TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap()
}

fn h1_get(srv: &TestServer, path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: acct.slow.localhost:{}\r\n\r\n", srv.port())
}

#[tokio::test]
async fn http1_client_that_closes_cancels_the_handler() {
    let (srv, seen) = start().await;
    let mut c = h1_connect(&srv).await;
    c.write_all(h1_get(&srv, "/sleep").as_bytes()).await.unwrap();
    wait_for("handler start", || seen.started.load(Ordering::SeqCst) == 1).await;
    drop(c);
    wait_for("handler dropped", || seen.dropped.load(Ordering::SeqCst) == 1).await;
    assert_eq!(seen.finished.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_token_reaches_a_blocking_thread_when_the_client_closes() {
    let (srv, seen) = start().await;
    let mut c = h1_connect(&srv).await;
    c.write_all(h1_get(&srv, "/blocking").as_bytes()).await.unwrap();
    wait_for("handler start", || seen.started.load(Ordering::SeqCst) == 1).await;
    drop(c);
    wait_for("blocking thread done", || seen.blocking_done.load(Ordering::SeqCst)).await;
    assert!(seen.blocking_saw_cancel.load(Ordering::SeqCst), "the blocking thread saw the cancellation");
}

async fn h2_connect(srv: &TestServer) -> (hyper::client::conn::http2::SendRequest<B>, tokio::task::JoinHandle<()>) {
    let tcp = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let (sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp)).await.unwrap();
    let task = tokio::spawn(async move {
        let _ = conn.await;
    });
    (sender, task)
}

fn h2_req(srv: &TestServer, path: &str) -> http::Request<B> {
    http::Request::builder().method(Method::GET).uri(format!("http://acct.slow.localhost:{}{path}", srv.port())).body(full("")).unwrap()
}

#[tokio::test]
async fn http2_stream_reset_cancels_the_handler() {
    let (srv, seen) = start().await;
    let (mut s, _task) = h2_connect(&srv).await;
    let fut = s.send_request(h2_req(&srv, "/sleep"));
    let call = tokio::spawn(fut);
    wait_for("handler start", || seen.started.load(Ordering::SeqCst) == 1).await;
    // dropping the response future sends RST_STREAM(CANCEL); the connection stays open
    call.abort();
    wait_for("handler dropped", || seen.dropped.load(Ordering::SeqCst) == 1).await;
    // the same connection still works
    let r = s.send_request(h2_req(&srv, "/quick-sleep")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.into_body().collect().await.unwrap().to_bytes().as_ref(), b"done");
    assert_eq!(seen.finished.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn http2_connection_close_cancels_the_handler() {
    let (srv, seen) = start().await;
    let (mut s, task) = h2_connect(&srv).await;
    let call = tokio::spawn(s.send_request(h2_req(&srv, "/sleep")));
    wait_for("handler start", || seen.started.load(Ordering::SeqCst) == 1).await;
    call.abort();
    drop(s);
    task.abort();
    wait_for("handler dropped", || seen.dropped.load(Ordering::SeqCst) == 1).await;
}

#[tokio::test]
async fn http2_blocking_work_sees_the_token() {
    let (srv, seen) = start().await;
    let (mut s, task) = h2_connect(&srv).await;
    let call = tokio::spawn(s.send_request(h2_req(&srv, "/blocking")));
    wait_for("handler start", || seen.started.load(Ordering::SeqCst) == 1).await;
    call.abort();
    drop(s);
    task.abort();
    wait_for("blocking thread done", || seen.blocking_done.load(Ordering::SeqCst)).await;
    assert!(seen.blocking_saw_cancel.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_request_that_finishes_is_unaffected() {
    let (srv, seen) = start().await;
    let r = srv.send("GET", &format!("{}/quick-sleep", srv.url("slow", "acct")), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "done");
    // the connection is closed by the client right after: nothing is counted as cancelled
    assert_eq!(seen.finished.load(Ordering::SeqCst), 1);
    assert_eq!(seen.dropped.load(Ordering::SeqCst), 1, "the guard of a finished handler is dropped once, at its end");
}

#[tokio::test]
async fn keep_alive_requests_one_after_another_are_unaffected() {
    let (srv, seen) = start().await;
    let mut c = h1_connect(&srv).await;
    let mut buf = Vec::new();
    for _ in 0..3 {
        c.write_all(h1_get(&srv, "/quick-sleep").as_bytes()).await.unwrap();
        // "HTTP/1.1 200 ... content-length: 4 ... done"
        loop {
            let mut tmp = [0u8; 1024];
            let n = c.read(&mut tmp).await.unwrap();
            assert!(n > 0, "connection closed early");
            buf.extend_from_slice(&tmp[..n]);
            if buf.ends_with(b"done") {
                break;
            }
        }
        buf.clear();
    }
    assert_eq!(seen.finished.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_long_upload_and_a_long_download_are_unaffected() {
    let (srv, _seen) = start().await;
    // 24 MB up, sent in pieces over a few hundred milliseconds
    let mut c = h1_connect(&srv).await;
    let total = 24 * 1024 * 1024;
    let head = format!("PUT /len HTTP/1.1\r\nHost: acct.slow.localhost:{}\r\nContent-Length: {total}\r\n\r\n", srv.port());
    c.write_all(head.as_bytes()).await.unwrap();
    let piece = vec![1u8; 1024 * 1024];
    for i in 0..24 {
        c.write_all(&piece).await.unwrap();
        if i % 6 == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let mut out = Vec::new();
    let mut tmp = [0u8; 4096];
    while !out.ends_with(total.to_string().as_bytes()) {
        let n = c.read(&mut tmp).await.unwrap();
        assert!(n > 0, "closed: {}", String::from_utf8_lossy(&out));
        out.extend_from_slice(&tmp[..n]);
    }
    // 24 MB down, read slowly at first, over HTTP/2 as well
    let n = 24 * 1024 * 1024;
    let r = srv.send("GET", &format!("{}/big?n={n}", srv.url("slow", "acct")), &[], "").await;
    assert_eq!(r.body.len(), n);
    let (mut s, _task) = h2_connect(&srv).await;
    let resp = s.send_request(h2_req(&srv, &format!("/big?n={n}"))).await.unwrap();
    let mut body = resp.into_body();
    let mut got = 0;
    let mut first = true;
    while let Some(frame) = body.frame().await {
        got += frame.unwrap().into_data().map(|d| d.len()).unwrap_or(0);
        if first {
            first = false;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    assert_eq!(got, n);
}

#[tokio::test]
async fn a_service_that_did_not_ask_runs_its_handler_to_the_end_after_the_client_left() {
    let (srv, seen) = start_asking(&[]).await;
    let mut c = h1_connect(&srv).await;
    c.write_all(h1_get(&srv, "/quick-sleep").as_bytes()).await.unwrap();
    wait_for("handler start", || seen.started.load(Ordering::SeqCst) == 1).await;
    drop(c);
    // the handler is not dropped while it still has 300 ms to go ...
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(seen.dropped.load(Ordering::SeqCst), 0, "dropped before its end");
    // ... and it finishes, so whatever it changed happened
    wait_for("handler finished", || seen.finished.load(Ordering::SeqCst) == 1).await;
    assert_eq!(seen.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_same_over_http2_for_a_reset_stream_and_a_closed_connection() {
    let (srv, seen) = start_asking(&[]).await;
    let (mut s, task) = h2_connect(&srv).await;
    let call = tokio::spawn(s.send_request(h2_req(&srv, "/quick-sleep")));
    wait_for("handler start", || seen.started.load(Ordering::SeqCst) == 1).await;
    call.abort(); // RST_STREAM
    wait_for("first handler finished", || seen.finished.load(Ordering::SeqCst) == 1).await;
    let call = tokio::spawn(s.send_request(h2_req(&srv, "/quick-sleep")));
    wait_for("second handler start", || seen.started.load(Ordering::SeqCst) == 2).await;
    call.abort();
    drop(s);
    task.abort(); // the connection goes
    wait_for("second handler finished", || seen.finished.load(Ordering::SeqCst) == 2).await;
}

#[tokio::test]
async fn the_token_of_a_service_that_did_not_ask_stays_quiet() {
    let (srv, seen) = start_asking(&[]).await;
    let mut c = h1_connect(&srv).await;
    c.write_all(h1_get(&srv, "/blocking-short").as_bytes()).await.unwrap();
    wait_for("handler start", || seen.started.load(Ordering::SeqCst) == 1).await;
    drop(c);
    wait_for("handler finished", || seen.finished.load(Ordering::SeqCst) == 1).await;
    assert!(seen.blocking_done.load(Ordering::SeqCst));
    assert!(!seen.blocking_saw_cancel.load(Ordering::SeqCst), "the token fired although the service did not ask");
}

#[tokio::test]
async fn the_service_chooses_per_request_by_method_and_path() {
    // asks for /sleep only: a client that leaves during /quick-sleep does not stop it
    let (srv, seen) = start_asking(&["/sleep"]).await;
    let mut c = h1_connect(&srv).await;
    c.write_all(h1_get(&srv, "/quick-sleep").as_bytes()).await.unwrap();
    wait_for("handler start", || seen.started.load(Ordering::SeqCst) == 1).await;
    drop(c);
    wait_for("handler finished", || seen.finished.load(Ordering::SeqCst) == 1).await;
    // and /sleep is dropped
    let mut c = h1_connect(&srv).await;
    c.write_all(h1_get(&srv, "/sleep").as_bytes()).await.unwrap();
    wait_for("sleep start", || seen.started.load(Ordering::SeqCst) == 2).await;
    drop(c);
    wait_for("sleep dropped", || seen.dropped.load(Ordering::SeqCst) == 2).await;
    assert_eq!(seen.finished.load(Ordering::SeqCst), 1);
}
