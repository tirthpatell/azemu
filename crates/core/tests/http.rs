mod common;

use std::sync::Arc;
use std::time::Duration;

use azemu_core::testing::{EchoService, TestServer};
use azemu_core::{Emulator, Mount, Service};
use common::*;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn probe() -> Arc<dyn Service> {
    Arc::new(Probe::new("echo", vec![Mount::AccountHost("echo")]))
}

#[tokio::test]
async fn head_keeps_service_content_length() {
    let srv = TestServer::start(vec![probe()]).await;
    let url = format!("{}/head-cl", srv.url("echo", "a"));
    let r = srv.send("HEAD", &url, &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-length"), Some("1234"));
    assert!(r.body.is_empty());

    // on the wire, too: Content-Length 1234 in the head, and nothing after it
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.write_all(format!("HEAD /head-cl HTTP/1.1\r\nHost: a.echo.localhost:{}\r\n\r\n", srv.port()).as_bytes()).await.unwrap();
    let mut r = BufReader::new(s);
    let mut head = String::new();
    loop {
        let mut l = String::new();
        r.read_line(&mut l).await.unwrap();
        head.push_str(&l);
        if l == "\r\n" {
            break;
        }
    }
    assert!(head.to_ascii_lowercase().contains("content-length: 1234\r\n"), "{head}");
}

#[tokio::test]
async fn head_of_a_service_that_returns_a_body() {
    // EchoService answers HEAD with a JSON body; core must drop it but keep the length.
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let url = format!("{}/x", srv.url("echo", "a"));
    let get = srv.send("GET", &url, &[], "").await;
    let head = srv.send("HEAD", &url, &[], "").await;
    assert!(head.body.is_empty());
    assert!(get.body.len() > 10);
    // method differs, so the echoed bodies differ in length by a few bytes; check shape only
    assert!(head.header("content-length").unwrap().parse::<usize>().unwrap() > 10);
}

#[tokio::test]
async fn content_length_date_and_framing() {
    let srv = TestServer::start(vec![probe()]).await;
    let base = srv.url("echo", "a");

    let r = srv.send("GET", &format!("{base}/x"), &[], "").await;
    let cl: usize = r.header("content-length").unwrap().parse().unwrap();
    assert_eq!(cl, r.body.len());
    let date = r.header("date").expect("Date header");
    assert!(date.ends_with(" GMT"), "{date}");
    azemu_core::time::parse_rfc1123(date).expect("Date parses");

    // service-set Date is kept
    let r = srv.send("GET", &format!("{base}/set-date"), &[], "").await;
    assert_eq!(r.header("date"), Some("Thu, 01 Jan 2099 00:00:00 GMT"));

    // service-set Content-Length is left alone
    let r = srv.send("GET", &format!("{base}/set-cl"), &[], "").await;
    assert_eq!(r.header("content-length"), Some("5"));
    assert_eq!(r.text(), "hello");

    // service-set Transfer-Encoding is left alone, no Content-Length added
    let r = srv.send("GET", &format!("{base}/chunked"), &[], "").await;
    assert_eq!(r.header("transfer-encoding"), Some("chunked"));
    assert!(r.header("content-length").is_none());
    assert_eq!(r.text(), "abc");

    let r = srv.send("DELETE", &format!("{base}/no-content"), &[], "").await;
    assert_eq!(r.status, 204);
    assert!(r.body.is_empty());
}

#[tokio::test]
async fn panicking_service_gives_500_and_server_survives() {
    let srv = TestServer::start(vec![probe()]).await;
    let base = srv.url("echo", "a");
    let r = srv.send("GET", &format!("{base}/panic"), &[], "").await;
    assert_eq!(r.status, 500);
    assert_eq!(r.json()["error"]["code"], "InternalError");
    let r = srv.send("GET", &format!("{base}/ok"), &[], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn big_put_arrives_intact() {
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let n = 64 * 1024 * 1024;
    let mut body = vec![0u8; n];
    let mut x: u64 = 0x9E3779B97F4A7C15;
    for chunk in body.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let b = x.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
    let mut want = String::new();
    for b in Sha256::digest(&body) {
        want.push_str(&format!("{b:02x}"));
    }
    let r = srv.send("PUT", &format!("{}/c/b", srv.url("echo", "a")), &[], body).await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["body_len"], n);
    assert_eq!(v["body_sha256"], want);
}

#[tokio::test]
async fn body_over_limit_is_413() {
    let dir = temp_dir("limit");
    let emu = Emulator::builder().service(probe()).gateway(loopback0()).cert_dir(dir).max_body_bytes(1000).start().await.unwrap();
    let port = gw_port(&emu);
    let c = azemu_core::testing::Client::new();
    let url = format!("http://a.echo.localhost:{port}/x");
    let r = c.send("PUT", &url, &[], vec![1u8; 1000]).await;
    assert_eq!(r.status, 200);
    let r = c.send("PUT", &url, &[], vec![1u8; 1001]).await;
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["error"]["code"], "RequestBodyTooLarge");

    // chunked body without Content-Length, over the limit
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let chunk = "x".repeat(600);
    let req = format!(
        "PUT /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nTransfer-Encoding: chunked\r\n\r\n{l:x}\r\n{chunk}\r\n{l:x}\r\n{chunk}\r\n0\r\n\r\n",
        l = chunk.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut r = BufReader::new(s);
    let (head, _) = read_response(&mut r).await;
    assert!(head.starts_with("HTTP/1.1 413"), "{head}");
    emu.shutdown().await;
}

#[tokio::test]
async fn keep_alive_two_requests_on_one_connection() {
    let srv = TestServer::start(vec![probe()]).await;
    let port = srv.port();
    let s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut r = BufReader::new(s);
    for i in 0..3 {
        let req = format!("GET /one?i={i} HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\n\r\n");
        r.get_mut().write_all(req.as_bytes()).await.unwrap();
        let (head, body) = read_response(&mut r).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["raw_query"], format!("i={i}"));
    }
    let req = format!("PUT /two HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 4\r\n\r\nabcd");
    r.get_mut().write_all(req.as_bytes()).await.unwrap();
    let (_, body) = read_response(&mut r).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["body_len"], 4);
}

#[tokio::test]
async fn expect_100_continue() {
    let srv = TestServer::start(vec![probe()]).await;
    let port = srv.port();
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(
        format!("PUT /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    // the interim response must arrive before we send the body
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await.expect("no 100 Continue").unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 100 Continue"), "{:?}", String::from_utf8_lossy(&buf[..n]));
    s.write_all(b"hello").await.unwrap();
    let mut r = BufReader::new(s);
    let (head, body) = read_response(&mut r).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["body_len"], 5);
}

#[tokio::test]
async fn hundred_concurrent_requests() {
    let srv = Arc::new(TestServer::start(vec![probe()]).await);
    let mut hs = Vec::new();
    for i in 0..100usize {
        let srv = srv.clone();
        hs.push(tokio::spawn(async move {
            let url = format!("{}/c{i}?n={i}", srv.url("echo", &format!("acct{}", i % 7)));
            let body = vec![b'x'; i * 10];
            let r = srv.send("PUT", &url, &[], body).await;
            assert_eq!(r.status, 200);
            let v = r.json();
            assert_eq!(v["path"], format!("/c{i}"));
            assert_eq!(v["body_len"], i * 10);
            assert_eq!(v["account"], format!("acct{}", i % 7));
        }));
    }
    for h in hs {
        h.await.unwrap();
    }
}

async fn raw(port: u16, bytes: &[u8]) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(bytes).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn malformed_requests_do_not_kill_the_server() {
    let srv = TestServer::start(vec![probe()]).await;
    let port = srv.port();

    // Host header that is not UTF-8: no match, 404 (gateway), never a panic
    let r = raw(port, b"GET /x HTTP/1.1\r\nHost: \xff\xfe.echo.localhost\r\nConnection: close\r\n\r\n").await;
    assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    // no Host at all (HTTP/1.0)
    let r = raw(port, b"GET /x HTTP/1.0\r\n\r\n").await;
    assert!(r.contains(" 404 "), "{r}");
    // raw non-ASCII in the path: rejected by the parser
    let r = raw(port, b"GET /\xff\xfe HTTP/1.1\r\nHost: a.echo.localhost\r\nConnection: close\r\n\r\n").await;
    assert!(r.starts_with("HTTP/1.1 400") || r.is_empty(), "{r}");
    // percent-encoded invalid UTF-8 in the path is passed through untouched
    let r = raw(port, format!("GET /%FF%FE/x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nConnection: close\r\n\r\n").as_bytes()).await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    assert!(r.contains("\"path\":\"/%FF%FE/x\""), "{r}");
    // giant header
    let big = "a".repeat(2 * 1024 * 1024);
    let r = raw(port, format!("GET /x HTTP/1.1\r\nHost: a.echo.localhost\r\nX-Big: {big}\r\nConnection: close\r\n\r\n").as_bytes()).await;
    assert!(r.starts_with("HTTP/1.1 431") || r.starts_with("HTTP/1.1 400") || r.is_empty(), "{}", &r[..r.len().min(100)]);
    // garbage
    let r = raw(port, b"\x00\x01\x02 garbage\r\n\r\n").await;
    assert!(r.starts_with("HTTP/1.1 400") || r.is_empty(), "{r}");
    // client vanishes mid-body
    {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("PUT /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 1000\r\n\r\nabc").as_bytes())
            .await
            .unwrap();
    }

    // still healthy
    let r = srv.send("GET", &format!("{}/ok", srv.url("echo", "a")), &[], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn idle_connection_does_not_block_others() {
    let srv = TestServer::start(vec![probe()]).await;
    let _idle = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(3), srv.send("GET", &format!("{}/ok", srv.url("echo", "a")), &[], ""))
        .await
        .expect("request blocked behind idle connection");
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn shutdown_stops_listeners() {
    let (emu, _d) = start_gateway(vec![probe()]).await;
    let addr = emu.gateway_addrs()[0];
    assert!(TcpStream::connect(addr).await.is_ok());
    emu.shutdown().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(TcpStream::connect(addr).await.is_err());
}

// --- limits and stalled clients -----------------------------------------------------------

async fn start_with_timeouts(header: Duration, body: Duration) -> (azemu_core::RunningEmulator, std::path::PathBuf) {
    let dir = temp_dir("limits");
    let emu = Emulator::builder()
        .service(probe())
        .gateway(loopback0())
        .cert_dir(dir.clone())
        .header_read_timeout(header)
        .body_idle_timeout(body)
        .max_body_bytes(1000)
        .start()
        .await
        .unwrap();
    (emu, dir)
}

/// Send raw bytes and read until a complete response head (and what came with it) is in, or the server
/// closes; `None` when nothing complete arrived within `wait`.
async fn raw_exchange(port: u16, data: &[u8], wait: Duration) -> Option<String> {
    let s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (mut s, mut w) = tokio::io::split(s);
    // write while reading: the server may answer (and close) before it has taken all of a big request
    let data = data.to_vec();
    tokio::spawn(async move {
        let _ = w.write_all(&data).await;
        // keep the write half open: closing it early would end a request the server wants to judge
        tokio::time::sleep(Duration::from_secs(10)).await;
    });
    let mut out = Vec::new();
    let read = async {
        let mut buf = [0u8; 4096];
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if out.windows(4).any(|w| w == b"\r\n\r\n") {
                        // give the body a moment to follow
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(50), s.read(&mut buf)).await {
                            out.extend_from_slice(&buf[..n]);
                        }
                        return;
                    }
                }
            }
        }
    };
    match tokio::time::timeout(wait, read).await {
        Ok(()) if out.is_empty() => Some(String::new()),
        Ok(()) => Some(String::from_utf8_lossy(&out).into_owned()),
        Err(_) => None,
    }
}

#[tokio::test]
async fn a_body_that_stalls_gets_408() {
    let (emu, _d) = start_with_timeouts(Duration::from_secs(30), Duration::from_millis(300)).await;
    let port = gw_port(&emu);
    let head = format!("POST /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 100\r\n\r\nabc");
    let started = std::time::Instant::now();
    let r = raw_exchange(port, head.as_bytes(), Duration::from_secs(5)).await.expect("the server answers");
    assert!(r.starts_with("HTTP/1.1 408"), "{r}");
    assert!(r.contains("RequestTimeout"), "{r}");
    assert!(started.elapsed() < Duration::from_secs(3));
    // a body that keeps arriving is not cut off: three pieces, each within the idle time
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(format!("POST /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 6\r\n\r\n").as_bytes()).await.unwrap();
    for piece in ["ab", "cd", "ef"] {
        tokio::time::sleep(Duration::from_millis(200)).await;
        s.write_all(piece.as_bytes()).await.unwrap();
    }
    let mut buf = vec![0u8; 512];
    let n = s.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    emu.shutdown().await;
}

#[tokio::test]
async fn a_client_that_never_finishes_the_request_head_is_disconnected() {
    let (emu, _d) = start_with_timeouts(Duration::from_millis(300), Duration::from_secs(30)).await;
    let port = gw_port(&emu);
    let r = raw_exchange(port, b"GET /x HTTP/1.1\r\nHost: a", Duration::from_secs(5)).await;
    assert!(r.is_some(), "the connection should have been closed by the server");
    emu.shutdown().await;
}

#[tokio::test]
async fn oversize_and_odd_requests_are_refused_without_reading_the_body() {
    let (emu, _d) = start_with_timeouts(Duration::from_secs(30), Duration::from_secs(30)).await;
    let port = gw_port(&emu);
    // a declared 10 GB body: 413 at once, nothing is read (and nothing is allocated)
    let head = format!("POST /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 10737418240\r\n\r\n");
    let r = raw_exchange(port, head.as_bytes(), Duration::from_secs(5)).await.expect("answered");
    assert!(r.starts_with("HTTP/1.1 413"), "{r}");
    // a chunked body past the limit, no Content-Length to check beforehand
    let mut body = String::new();
    for _ in 0..20 {
        body.push_str(&format!("64\r\n{}\r\n", "x".repeat(100)));
    }
    body.push_str("0\r\n\r\n");
    let req = format!("POST /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nTransfer-Encoding: chunked\r\n\r\n{body}");
    let r = raw_exchange(port, req.as_bytes(), Duration::from_secs(5)).await.expect("answered");
    assert!(r.starts_with("HTTP/1.1 413"), "{r}");
    // more than 100 headers
    let mut req = format!("GET /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\n");
    for i in 0..150 {
        req.push_str(&format!("X-H{i}: v\r\n"));
    }
    req.push_str("\r\n");
    let r = raw_exchange(port, req.as_bytes(), Duration::from_secs(5)).await.expect("answered");
    assert!(r.starts_with("HTTP/1.1 431"), "{r}");
    // a head of two megabytes (hyper stops reading a head at about 400 KB; one read that happens to
    // hold more than that, as on a loaded machine, can still complete a head of 500 KB, so the test
    // head is well above that)
    let req = format!("GET /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nX-Big: {}\r\n\r\n", "a".repeat(2_000_000));
    // The server answers and closes while the client is still sending; the answer must arrive every
    // time (the server reads and discards what is still coming before it closes).
    let r = raw_exchange(port, req.as_bytes(), Duration::from_secs(5)).await;
    let r = r.expect("answered");
    assert!(r.starts_with("HTTP/1.1 431"), "{r}");
    emu.shutdown().await;
}

// --- early answers: the client still sending, Expect: 100-continue --------------------------

/// Send `head` and then `extra` bytes straight after it, without waiting for the answer, and read until the
/// server closes. Returns what arrived, or the error that ended the read before the answer was complete.
async fn send_everything_then_read(port: u16, head: &str, extra: usize) -> std::io::Result<String> {
    let s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (mut rd, mut wr) = tokio::io::split(s);
    let mut data = head.as_bytes().to_vec();
    data.extend(std::iter::repeat_n(b'x', extra));
    let writer = tokio::spawn(async move {
        let _ = wr.write_all(&data).await;
        // keep the write side open: the client is "still sending"
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let res = loop {
        match tokio::time::timeout(Duration::from_secs(8), rd.read(&mut buf)).await {
            Ok(Ok(0)) => break Ok(()),
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
            Ok(Err(e)) => break Err(e),
            Err(_) => break Ok(()),
        }
    };
    writer.abort();
    match res {
        Ok(()) => Ok(String::from_utf8_lossy(&out).into_owned()),
        Err(e) if out.windows(4).any(|w| w == b"\r\n\r\n") => {
            // got the answer and then the reset: also acceptable, but not the other way round
            let _ = e;
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
        Err(e) => Err(e),
    }
}

#[tokio::test]
async fn an_early_413_reaches_a_client_that_is_still_sending_and_says_connection_close() {
    let (emu, _d) = start_with_timeouts(Duration::from_secs(30), Duration::from_secs(30)).await;
    let port = gw_port(&emu);
    let head = format!("POST /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 104857600\r\n\r\n");
    for i in 0..30 {
        // 900 KB follow the head at once: they sit unread in the server's socket when it closes
        let out = send_everything_then_read(port, &head, 900_000).await.unwrap_or_else(|e| panic!("run {i}: the answer was lost: {e}"));
        assert!(out.starts_with("HTTP/1.1 413"), "run {i}: {out}");
        assert!(out.to_ascii_lowercase().contains("connection: close"), "run {i}: {out}");
    }
    emu.shutdown().await;
}

/// What the .NET HTTP client does: write the whole request body, and only then read the answer. The write must not fail
/// because the server answered early and closed (finding CO1: "Broken pipe" for a 2.2 MB Cosmos item).
async fn write_everything_then_read(port: u16, head: &str, extra: usize) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(head.as_bytes()).await.map_err(|e| format!("writing the head: {e}"))?;
    let chunk = vec![b'x'; 64 * 1024];
    let mut left = extra;
    while left > 0 {
        let n = left.min(chunk.len());
        s.write_all(&chunk[..n]).await.map_err(|e| format!("writing the body, {left} bytes to go: {e}"))?;
        left -= n;
    }
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out)).await;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[tokio::test]
async fn a_client_that_writes_a_whole_big_body_before_reading_gets_the_413() {
    let (emu, _d) = start_with_timeouts(Duration::from_secs(30), Duration::from_secs(30)).await;
    let port = gw_port(&emu);
    let head = format!("POST /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 104857600\r\n\r\n");
    for (i, size) in [2_200_000usize, 4_000_000, 20_000_000].into_iter().enumerate() {
        let out = write_everything_then_read(port, &head, size).await.unwrap_or_else(|e| panic!("run {i} ({size} bytes): {e}"));
        assert!(out.starts_with("HTTP/1.1 413"), "run {i}: {out}");
    }
    emu.shutdown().await;
}

#[tokio::test]
async fn an_early_answer_for_an_unknown_route_with_a_big_body_arrives_too() {
    let (emu, _d) = start_with_timeouts(Duration::from_secs(30), Duration::from_secs(30)).await;
    let port = gw_port(&emu);
    // no service answers for this host: 404 before the body is looked at
    let head = "POST /x HTTP/1.1\r\nHost: nothing\r\nContent-Length: 600000\r\n\r\n";
    for i in 0..10 {
        let out = send_everything_then_read(port, head, 500_000).await.unwrap_or_else(|e| panic!("run {i}: {e}"));
        assert!(out.starts_with("HTTP/1.1 404"), "run {i}: {out}");
    }
    emu.shutdown().await;
}

#[tokio::test]
async fn the_lingering_close_is_bounded() {
    // the server stops reading after 32 MiB (or 5 s) and closes: a client that never stops sending does
    // not hold the connection open for ever
    let (emu, _d) = start_with_timeouts(Duration::from_secs(30), Duration::from_secs(30)).await;
    let port = gw_port(&emu);
    let head = format!("POST /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 104857600\r\n\r\n");
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    let started = std::time::Instant::now();
    let chunk = vec![b'x'; 64 * 1024];
    let mut closed = false;
    let mut buf = [0u8; 1024];
    while started.elapsed() < Duration::from_secs(10) {
        if s.write_all(&chunk).await.is_err() {
            closed = true;
            break;
        }
        // the answer (and then the close) is what we find when we read
        if let Ok(Ok(0)) | Ok(Err(_)) = tokio::time::timeout(Duration::from_millis(1), s.read(&mut buf)).await {
            closed = true;
            break;
        }
    }
    assert!(closed, "the server went on reading a stream that never ends");
    assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
    emu.shutdown().await;
}

#[tokio::test]
async fn expect_100_continue_for_a_body_that_is_refused_gets_the_final_status() {
    let (emu, _d) = start_with_timeouts(Duration::from_secs(30), Duration::from_secs(30)).await;
    let port = gw_port(&emu);
    // declared over the limit: the answer is 413, never "100 Continue"
    let head = format!("PUT /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 5000\r\nExpect: 100-continue\r\n\r\n");
    let r = raw_exchange(port, head.as_bytes(), Duration::from_secs(5)).await.expect("answered");
    assert!(r.starts_with("HTTP/1.1 413"), "{r}");
    assert!(!r.contains("100 Continue"), "{r}");
    // an unknown host: 404
    let head = "PUT /x HTTP/1.1\r\nHost: nothing\r\nContent-Length: 10\r\nExpect: 100-continue\r\n\r\n";
    let r = raw_exchange(port, head.as_bytes(), Duration::from_secs(5)).await.expect("answered");
    assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    emu.shutdown().await;
}

#[tokio::test]
async fn expect_100_continue_for_an_accepted_body_gets_100_at_once_and_then_the_answer() {
    let (emu, _d) = start_with_timeouts(Duration::from_secs(30), Duration::from_secs(30)).await;
    let port = gw_port(&emu);
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let head = format!("PUT /x HTTP/1.1\r\nHost: a.echo.localhost:{port}\r\nContent-Length: 10\r\nExpect: 100-continue\r\n\r\n");
    s.write_all(head.as_bytes()).await.unwrap();
    // the body is held back until the server says to go on
    let mut buf = [0u8; 512];
    let started = std::time::Instant::now();
    let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await.expect("100 Continue arrives without the body").unwrap();
    let first = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(first.starts_with("HTTP/1.1 100 Continue"), "{first}");
    assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
    s.write_all(b"0123456789").await.unwrap();
    let mut rest = Vec::new();
    while !String::from_utf8_lossy(&rest).contains("200 OK") {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await.expect("final answer").unwrap();
        assert!(n > 0, "closed before the answer");
        rest.extend_from_slice(&buf[..n]);
    }
    emu.shutdown().await;
}
