//! HTTP/2: prior knowledge on the plain listener, ALPN `h2` on TLS, and everything that must behave the
//! same as over HTTP/1.1 (routing by authority, limits, timeouts, HEAD, header names, the WebSocket
//! upgrade, raw protocols in TLS).

mod common;

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use azemu_core::testing::{EchoService, TestServer};
use azemu_core::{Emulator, Mount, Service};
use bytes::Bytes;
use common::*;
use http::{Method, Request, Version};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame};
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

type B = BoxBody<Bytes, std::io::Error>;

fn full(b: impl Into<Bytes>) -> B {
    Full::new(b.into()).map_err(|e: Infallible| match e {}).boxed()
}

fn client_config(ca_pem: &str, alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let mut roots = rustls::RootCertStore::empty();
    for der in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        roots.add(der.unwrap()).unwrap();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(cfg)
}

async fn tls_stream(port: u16, ca_pem: &str, name: &str, alpn: &[&[u8]]) -> tokio_rustls::client::TlsStream<TcpStream> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let sni = rustls::pki_types::ServerName::try_from(name.to_string()).unwrap();
    tokio_rustls::TlsConnector::from(client_config(ca_pem, alpn)).connect(sni, tcp).await.unwrap()
}

async fn handshake<I>(io: I) -> SendRequest<B>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(io)).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
}

/// HTTP/2 with prior knowledge over plain TCP.
async fn plain_h2(port: u16) -> SendRequest<B> {
    handshake(TcpStream::connect(("127.0.0.1", port)).await.unwrap()).await
}

/// HTTP/2 over TLS, ALPN `h2` only (what the Rust Cosmos SDK offers for its first request).
async fn tls_h2(port: u16, ca_pem: &str, name: &str) -> SendRequest<B> {
    let tls = tls_stream(port, ca_pem, name, &[b"h2"]).await;
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]), "the server answered ALPN h2");
    handshake(tls).await
}

struct Reply {
    status: u16,
    version: Version,
    headers: http::HeaderMap,
    body: Bytes,
}

async fn call(s: &mut SendRequest<B>, req: Request<B>) -> Reply {
    let resp = s.send_request(req).await.expect("send_request");
    let (parts, body) = resp.into_parts();
    let body = body.collect().await.expect("response body").to_bytes();
    Reply { status: parts.status.as_u16(), version: parts.version, headers: parts.headers, body }
}

fn req(method: Method, scheme: &str, authority: &str, path: &str, body: B) -> Request<B> {
    Request::builder().method(method).uri(format!("{scheme}://{authority}{path}")).body(body).unwrap()
}

fn json(r: &Reply) -> serde_json::Value {
    serde_json::from_slice(&r.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&r.body)))
}

fn probe() -> Arc<dyn Service> {
    Arc::new(Probe::new("echo", vec![Mount::AccountHost("echo")]))
}

#[tokio::test]
async fn plain_listener_serves_http2_with_prior_knowledge() {
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let mut c = plain_h2(srv.port()).await;
    let authority = format!("acct.echo.localhost:{}", srv.port());
    let r = call(&mut c, req(Method::GET, "http", &authority, "/p/q?x=1", full(""))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.version, Version::HTTP_2);
    let v = json(&r);
    assert_eq!(v["account"], "acct", "routed by :authority, there is no Host header");
    assert_eq!(v["path"], "/p/q");
    assert_eq!(v["raw_query"], "x=1");
    assert_eq!(v["endpoint"]["scheme"], "http");
    assert_eq!(v["endpoint"]["authority"], authority);
    assert_eq!(v["endpoint"]["base_url"], format!("http://{authority}"));
    assert!(r.headers.contains_key("date"));
    // the same connection carries a second request, on another account
    let other = format!("second.echo.localhost:{}", srv.port());
    let r = call(&mut c, req(Method::POST, "http", &other, "/x", full("hello"))).await;
    assert_eq!(json(&r)["account"], "second");
    assert_eq!(json(&r)["body_len"], 5);
}

#[tokio::test]
async fn tls_listener_negotiates_h2_and_serves_it() {
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let name = "acct.echo.localhost";
    let mut c = tls_h2(srv.port(), srv.ca_pem(), name).await;
    let authority = format!("{name}:{}", srv.port());
    let r = call(&mut c, req(Method::GET, "https", &authority, "/p", full(""))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.version, Version::HTTP_2);
    let v = json(&r);
    assert_eq!(v["endpoint"]["scheme"], "https");
    assert_eq!(v["endpoint"]["base_url"], format!("https://{authority}"));
    assert_eq!(v["account"], "acct");
}

#[tokio::test]
async fn alpn_is_answered_for_http_clients_and_ignored_for_others() {
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let name = "acct.echo.localhost";
    let want = |offer: &[&[u8]]| {
        let ca = srv.ca_pem().to_string();
        let port = srv.port();
        let offer: Vec<Vec<u8>> = offer.iter().map(|p| p.to_vec()).collect();
        async move {
            let offer: Vec<&[u8]> = offer.iter().map(|v| v.as_slice()).collect();
            tls_stream(port, &ca, name, &offer).await.get_ref().1.alpn_protocol().map(<[u8]>::to_vec)
        }
    };
    assert_eq!(want(&[b"h2", b"http/1.1"]).await.as_deref(), Some(&b"h2"[..]), "h2 first: h2");
    assert_eq!(want(&[b"http/1.1", b"h2"]).await.as_deref(), Some(&b"h2"[..]), "the server prefers h2");
    assert_eq!(want(&[b"http/1.1"]).await.as_deref(), Some(&b"http/1.1"[..]));
    assert_eq!(want(&[b"h2"]).await.as_deref(), Some(&b"h2"[..]));
    // no ALPN: nothing answered; a client that offers only something else is not refused either
    assert_eq!(want(&[]).await, None);
    assert_eq!(want(&[b"amqp"]).await, None, "an unknown protocol list is ignored, as before");
    assert_eq!(want(&[b"amqp", b"h2"]).await.as_deref(), Some(&b"h2"[..]));
}

#[tokio::test]
async fn http_1_1_over_tls_still_works_with_and_without_alpn() {
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let name = "acct.echo.localhost";
    for alpn in [vec![], vec![&b"http/1.1"[..]], vec![&b"amqp"[..]]] {
        let mut tls = tls_stream(srv.port(), srv.ca_pem(), name, &alpn).await;
        tls.write_all(format!("GET /p HTTP/1.1\r\nHost: {name}:{}\r\nConnection: close\r\n\r\n", srv.port()).as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tls.read_to_end(&mut out).await;
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 200"), "{alpn:?}: {text}");
        assert!(text.contains("\"account\":\"acct\""), "{text}");
    }
    // the plain-text client of the same crate, too
    let r = srv.send("GET", &format!("{}/p", srv.https_url("echo", "acct")), &[], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn large_upload_over_h2_plain_and_tls() {
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let size = 24 * 1024 * 1024;
    let data: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
    let digest: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    let name = "acct.echo.localhost";
    let authority = format!("{name}:{}", srv.port());

    let mut plain = plain_h2(srv.port()).await;
    let mut tls = tls_h2(srv.port(), srv.ca_pem(), name).await;
    for (label, c, scheme) in [("plain", &mut plain, "http"), ("tls", &mut tls, "https")] {
        let up = req(Method::PUT, scheme, &authority, "/upload", full(data.clone()));
        let r = call(c, up).await;
        assert_eq!(r.status, 200, "{label}");
        let v = json(&r);
        assert_eq!(v["body_len"], size, "{label}");
        assert_eq!(v["body_sha256"], digest, "{label}");
    }
}

#[tokio::test]
async fn large_download_over_h2() {
    let srv = TestServer::start(vec![probe()]).await;
    let size = 24 * 1024 * 1024usize;
    let name = "acct.echo.localhost";
    let authority = format!("{name}:{}", srv.port());
    let mut plain = plain_h2(srv.port()).await;
    let mut tls = tls_h2(srv.port(), srv.ca_pem(), name).await;
    for (label, c, scheme) in [("plain", &mut plain, "http"), ("tls", &mut tls, "https")] {
        let r = call(c, req(Method::GET, scheme, &authority, &format!("/big?n={size}"), full(""))).await;
        assert_eq!(r.status, 200, "{label}");
        assert_eq!(r.body.len(), size, "{label}");
        assert!(r.body.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8), "{label}: content");
        assert_eq!(r.headers["content-length"], size.to_string(), "{label}");
    }
}

#[tokio::test]
async fn an_oversize_content_length_is_refused_before_the_body_is_read() {
    let dir = temp_dir("h2-limit");
    let emu = Emulator::builder().gateway(loopback0()).cert_dir(dir).service(probe()).max_body_bytes(1000).start().await.unwrap();
    let port = gw_port(&emu);
    let authority = format!("a.echo.localhost:{port}");
    let mut c = plain_h2(port).await;

    // declared length over the limit: 413 at once, whatever follows
    let mut r = req(Method::PUT, "http", &authority, "/x", full(vec![0u8; 100_000]));
    r.headers_mut().insert("content-length", "100000".parse().unwrap());
    let r = call(&mut c, r).await;
    assert_eq!(r.status, 413);
    assert!(String::from_utf8_lossy(&r.body).contains("RequestBodyTooLarge"));
    // the connection is still good for the next request
    let r = call(&mut c, req(Method::PUT, "http", &authority, "/x", full(vec![0u8; 900]))).await;
    assert_eq!(r.status, 200);
    assert_eq!(json(&r)["body_len"], 900);

    // no Content-Length (a streamed body): cut when it passes the limit
    let chunks =
        StreamBody { chunks: vec![Bytes::from(vec![1u8; 600]), Bytes::from(vec![1u8; 600]), Bytes::from(vec![1u8; 600])], stall: false };
    let r = call(&mut c, req(Method::PUT, "http", &authority, "/x", chunks.boxed())).await;
    assert_eq!(r.status, 413);
    emu.shutdown().await;
}

/// A request body that sends its chunks and then either ends or never sends another byte.
struct StreamBody {
    chunks: Vec<Bytes>,
    stall: bool,
}

impl Body for StreamBody {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        if !self.chunks.is_empty() {
            return Poll::Ready(Some(Ok(Frame::data(self.chunks.remove(0)))));
        }
        if self.stall {
            return Poll::Pending;
        }
        Poll::Ready(None)
    }
}

#[tokio::test]
async fn a_request_body_that_stops_arriving_gets_408() {
    let dir = temp_dir("h2-stall");
    let emu = Emulator::builder()
        .gateway(loopback0())
        .cert_dir(dir)
        .service(probe())
        .body_idle_timeout(Duration::from_millis(300))
        .start()
        .await
        .unwrap();
    let port = gw_port(&emu);
    let authority = format!("a.echo.localhost:{port}");
    let mut c = plain_h2(port).await;
    let body = StreamBody { chunks: vec![Bytes::from_static(b"some")], stall: true };
    let started = std::time::Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(10), call(&mut c, req(Method::PUT, "http", &authority, "/x", body.boxed())))
        .await
        .expect("answered");
    assert_eq!(r.status, 408);
    assert!(started.elapsed() < Duration::from_secs(5));
    // other requests on the connection are not affected
    let r = call(&mut c, req(Method::GET, "http", &authority, "/x", full(""))).await;
    assert_eq!(r.status, 200);
    emu.shutdown().await;
}

#[tokio::test]
async fn authority_decides_the_route_and_wins_over_a_host_header() {
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let mut c = plain_h2(srv.port()).await;
    let a = format!("first.echo.localhost:{}", srv.port());
    let mut r = req(Method::GET, "http", &a, "/p", full(""));
    r.headers_mut().insert("host", format!("second.echo.localhost:{}", srv.port()).parse().unwrap());
    let r = call(&mut c, r).await;
    assert_eq!(json(&r)["account"], "first", "RFC 9113 8.3.1: :authority takes precedence");

    // an authority that names no service is the gateway's own 404, as over HTTP/1.1
    let r = call(&mut c, req(Method::GET, "http", &format!("nothing.localhost:{}", srv.port()), "/p", full(""))).await;
    assert_eq!(r.status, 404);
    assert!(String::from_utf8_lossy(&r.body).contains("NoServiceMatched"));
    // the admin API answers on any authority
    let r = call(&mut c, req(Method::GET, "http", &format!("127.0.0.1:{}", srv.port()), "/_azemu/health", full(""))).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn services_see_a_host_header_and_lower_case_names_like_over_http_1_1() {
    let srv = TestServer::start(vec![probe()]).await;
    let authority = format!("a.echo.localhost:{}", srv.port());
    // HTTP/2
    let mut c = plain_h2(srv.port()).await;
    let mut r = req(Method::GET, "http", &authority, "/headers", full(""));
    r.headers_mut().insert("X-Ms-Probe", "Value-Kept-As-Sent".parse().unwrap());
    r.headers_mut().insert("x-ms-Version", "2024-11-04".parse().unwrap());
    let h2 = call(&mut c, r).await;
    // HTTP/1.1
    let h1 = srv
        .send("GET", &format!("http://{authority}/headers"), &[("X-Ms-Probe", "Value-Kept-As-Sent"), ("x-ms-Version", "2024-11-04")], "")
        .await;
    assert_eq!(h2.status, 200);
    let (v2, v1) = (json(&h2), h1.json());
    assert_eq!(v2["host"], authority, "no Host over HTTP/2: the authority stands in");
    assert_eq!(v1["host"], authority);
    let names =
        |v: &serde_json::Value| -> Vec<String> { v["names"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect() };
    for n in names(&v2) {
        assert_eq!(n, n.to_ascii_lowercase(), "names reach services in lower case");
    }
    for want in ["x-ms-probe", "x-ms-version", "host"] {
        assert!(names(&v2).iter().any(|n| n == want), "{want} in {:?}", names(&v2));
        assert!(names(&v1).iter().any(|n| n == want), "{want} in {:?}", names(&v1));
    }
    // a header value goes through unchanged and the response header comes back lower case
    assert_eq!(h2.headers["x-ms-echo-probe"], "Value-Kept-As-Sent");
    assert_eq!(h1.header("x-ms-echo-probe"), Some("Value-Kept-As-Sent"));
}

#[tokio::test]
async fn head_keeps_the_content_length_and_has_no_body_over_h2() {
    let srv = TestServer::start(vec![probe()]).await;
    let authority = format!("a.echo.localhost:{}", srv.port());
    let mut c = plain_h2(srv.port()).await;
    let r = call(&mut c, req(Method::HEAD, "http", &authority, "/head-cl", full(""))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.headers["content-length"], "1234");
    assert!(r.body.is_empty());
    // 204 and a service-chosen chunked framing, which HTTP/2 does not have
    let r = call(&mut c, req(Method::GET, "http", &authority, "/no-content", full(""))).await;
    assert_eq!(r.status, 204);
    let r = call(&mut c, req(Method::GET, "http", &authority, "/chunked", full(""))).await;
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], b"abc");
    assert!(!r.headers.contains_key("transfer-encoding"));
    // a panic in a service is a 500, and the connection lives on
    let r = call(&mut c, req(Method::GET, "http", &authority, "/panic", full(""))).await;
    assert_eq!(r.status, 500);
    let r = call(&mut c, req(Method::GET, "http", &authority, "/x", full(""))).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn many_requests_at_once_share_one_connection() {
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let authority = format!("acct.echo.localhost:{}", srv.port());
    let c = plain_h2(srv.port()).await;
    let mut tasks = Vec::new();
    // more than the 128 streams the server allows at once: the client queues the rest
    for i in 0..300usize {
        let mut c = c.clone();
        let authority = authority.clone();
        tasks.push(tokio::spawn(async move {
            let body = vec![(i % 256) as u8; 1000 + i];
            let r = call(&mut c, req(Method::PUT, "http", &authority, &format!("/n/{i}"), full(body))).await;
            (i, r.status, json(&r))
        }));
    }
    for t in tasks {
        let (i, status, v) = t.await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(v["path"], format!("/n/{i}"));
        assert_eq!(v["body_len"], 1000 + i);
    }
}

// ---- the server's own limits, checked on the wire ----------------------------------------------

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![(payload.len() >> 16) as u8, (payload.len() >> 8) as u8, payload.len() as u8, kind, flags];
    f.extend_from_slice(&stream.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// HEADERS for `GET /_azemu/health` with END_STREAM | END_HEADERS on `stream`.
fn get_health(stream: u32, authority: &str) -> Vec<u8> {
    // :method GET, :scheme http, :path (literal, name index 4)
    let mut block = vec![0x82, 0x86, 0x44, 14];
    block.extend_from_slice(b"/_azemu/health");
    block.push(0x41); // :authority, literal, name index 1
    block.push(authority.len() as u8);
    block.extend_from_slice(authority.as_bytes());
    frame(0x1, 0x5, stream, &block)
}

async fn read_frame(s: &mut TcpStream) -> Option<(u8, u8, u32, Vec<u8>)> {
    let mut h = [0u8; 9];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut h)).await.ok()?.ok()?;
    let len = ((h[0] as usize) << 16) | ((h[1] as usize) << 8) | h[2] as usize;
    let mut payload = vec![0u8; len];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut payload)).await.ok()?.ok()?;
    Some((h[3], h[4], u32::from_be_bytes([h[5] & 0x7f, h[6], h[7], h[8]]), payload))
}

#[tokio::test]
async fn the_server_announces_its_limits() {
    let srv = TestServer::start(vec![probe()]).await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let mut hello = PREFACE.to_vec();
    hello.extend(frame(0x4, 0, 0, &[]));
    s.write_all(&hello).await.unwrap();
    let mut settings = std::collections::HashMap::new();
    for _ in 0..5 {
        let Some((kind, flags, _, payload)) = read_frame(&mut s).await else { break };
        if kind == 0x4 && flags & 1 == 0 {
            for c in payload.chunks(6) {
                settings.insert(u16::from_be_bytes([c[0], c[1]]), u32::from_be_bytes([c[2], c[3], c[4], c[5]]));
            }
            break;
        }
    }
    assert_eq!(settings.get(&3), Some(&128), "MAX_CONCURRENT_STREAMS: {settings:?}");
    assert_eq!(settings.get(&6), Some(&(128 * 1024)), "MAX_HEADER_LIST_SIZE: {settings:?}");
    assert_eq!(settings.get(&5), Some(&(64 * 1024)), "MAX_FRAME_SIZE: {settings:?}");
    assert_eq!(settings.get(&4), Some(&(2 * 1024 * 1024)), "INITIAL_WINDOW_SIZE: {settings:?}");
}

#[tokio::test]
async fn a_header_list_over_the_limit_is_refused_and_the_server_goes_on() {
    let srv = TestServer::start(vec![probe()]).await;
    let authority = format!("a.echo.localhost:{}", srv.port());
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let mut out = PREFACE.to_vec();
    out.extend(frame(0x4, 0, 0, &[]));
    // GET /x with 1000 extra headers of 100 bytes (about 140 KiB by the HTTP/2 rule, limit 128 KiB)
    let mut block = vec![0x82, 0x86, 0x84, 0x41, authority.len() as u8];
    block.extend_from_slice(authority.as_bytes());
    for i in 0..1000 {
        let name = format!("x-big-{i:04}");
        block.push(0x00);
        block.push(name.len() as u8);
        block.extend_from_slice(name.as_bytes());
        block.push(100);
        block.extend_from_slice(&[b'v'; 100]);
    }
    let mut chunks = block.chunks(16_000).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let end = if chunks.peek().is_none() { 0x4 } else { 0 };
        if first {
            out.extend(frame(0x1, 0x1 | end, 1, chunk));
            first = false;
        } else {
            out.extend(frame(0x9, end, 1, chunk));
        }
    }
    let _ = s.write_all(&out).await;
    let mut ok_answer = false;
    while let Some((kind, _, stream, payload)) = read_frame(&mut s).await {
        // an answer for stream 1 whose first header is `:status: 200` (HPACK 0x88) would be wrong
        if kind == 0x1 && stream == 1 && payload.first() == Some(&0x88) {
            ok_answer = true;
        }
        if kind == 0x7 || kind == 0x3 {
            break;
        }
    }
    assert!(!ok_answer, "a request with an oversize header list was served");
    // a new connection is served
    let mut c2 = plain_h2(srv.port()).await;
    let r = call(&mut c2, req(Method::GET, "http", &authority, "/x", full(""))).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn a_flood_of_reset_streams_closes_the_connection_and_the_server_survives() {
    let srv = TestServer::start(vec![probe()]).await;
    let authority = format!("a.echo.localhost:{}", srv.port());
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let mut burst = PREFACE.to_vec();
    burst.extend(frame(0x4, 0, 0, &[]));
    // 4000 requests, each followed at once by RST_STREAM(CANCEL): the "rapid reset" pattern
    for i in 0..4000u32 {
        let id = 1 + 2 * i;
        burst.extend(get_health(id, &authority));
        burst.extend(frame(0x3, 0, id, &8u32.to_be_bytes()));
    }
    let _ = s.write_all(&burst).await;
    // the server must end the connection (GOAWAY ENHANCE_YOUR_CALM or just a close) before it has
    // answered all of them
    let mut goaway = None;
    let mut responses = 0;
    while let Some((kind, _, _, payload)) = read_frame(&mut s).await {
        if kind == 0x7 {
            goaway = Some(u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]));
            break;
        }
        if kind == 0x1 {
            responses += 1;
        }
    }
    assert!(goaway.is_some() || responses < 4000, "the flood was answered in full: {responses} responses, no GOAWAY");
    // the server is fine
    let mut c = plain_h2(srv.port()).await;
    let r = call(&mut c, req(Method::GET, "http", &authority, "/_azemu/health", full(""))).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn an_idle_http_2_connection_is_closed_after_the_header_read_timeout() {
    let dir = temp_dir("h2-idle");
    let emu = Emulator::builder()
        .gateway(loopback0())
        .cert_dir(dir)
        .service(probe())
        .header_read_timeout(Duration::from_millis(400))
        .start()
        .await
        .unwrap();
    let port = gw_port(&emu);
    let authority = format!("a.echo.localhost:{port}");
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap()))
            .await
            .unwrap();
    let closed = tokio::spawn(async move {
        let _ = conn.await;
    });
    let r = call(&mut sender, req(Method::GET, "http", &authority, "/x", full(""))).await;
    assert_eq!(r.status, 200);
    tokio::time::timeout(Duration::from_secs(10), closed).await.expect("the idle connection was closed").unwrap();
    emu.shutdown().await;
}

#[tokio::test]
async fn pinned_listener_serves_http2_too() {
    let dir = temp_dir("h2-pinned");
    let emu = Emulator::builder().pinned(loopback0(), "echo").cert_dir(dir).service(Arc::new(EchoService::new())).start().await.unwrap();
    let port = emu.pinned_addr("echo").unwrap().port();
    let mut c = plain_h2(port).await;
    // path style on a pinned listener: the account is the first path segment
    let r = call(&mut c, req(Method::GET, "http", &format!("127.0.0.1:{port}"), "/myacct/blob/x", full(""))).await;
    assert_eq!(r.status, 200);
    let v = json(&r);
    assert_eq!(v["account"], "myacct");
    assert_eq!(v["path"], "/blob/x");
    assert_eq!(v["endpoint"]["base_url"], format!("http://127.0.0.1:{port}/myacct"));
    let mut c = tls_h2(port, emu.ca_pem(), "localhost").await;
    let r = call(&mut c, req(Method::GET, "https", &format!("localhost:{port}"), "/myacct/blob/x", full(""))).await;
    assert_eq!(json(&r)["account"], "myacct");
    emu.shutdown().await;
}

#[tokio::test]
async fn http_1_1_and_http_2_share_the_listener() {
    // (the WebSocket upgrade, which is HTTP/1.1 only, is covered in tests/websocket.rs)
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let mut c = plain_h2(srv.port()).await;
    let authority = format!("acct.echo.localhost:{}", srv.port());
    let r = call(&mut c, req(Method::GET, "http", &authority, "/p", full(""))).await;
    assert_eq!(r.status, 200);
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.write_all(format!("GET /p HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
}

/// Starts with `AMQP`, answers with the 8-byte AMQP header.
struct Amqpish;

#[azemu_core::async_trait]
impl azemu_core::RawProtocol for Amqpish {
    fn sniff(&self, first: &[u8]) -> bool {
        first.starts_with(b"AMQP")
    }
    async fn serve(&self, conn: azemu_core::RawConn) {
        let mut io = conn.io;
        let mut hdr = [0u8; 8];
        if io.read_exact(&mut hdr).await.is_err() {
            return;
        }
        let _ = io.write_all(b"AMQP\x00\x01\x00\x00").await;
        let _ = io.shutdown().await;
    }
}

#[tokio::test]
async fn raw_protocols_http_1_1_and_http_2_share_the_port() {
    let srv = TestServer::start_with(vec![Arc::new(EchoService::new())], vec![Arc::new(Amqpish)]).await;
    let name = "acct.echo.localhost";
    let authority = format!("{name}:{}", srv.port());

    // AMQP in TLS: no ALPN, and a client that offers only `amqp`
    for alpn in [vec![], vec![&b"amqp"[..]]] {
        let mut tls = tls_stream(srv.port(), srv.ca_pem(), name, &alpn).await;
        assert_eq!(tls.get_ref().1.alpn_protocol(), None);
        tls.write_all(b"AMQP\x00\x01\x00\x00").await.unwrap();
        let mut reply = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), tls.read_to_end(&mut reply)).await;
        assert_eq!(reply, b"AMQP\x00\x01\x00\x00", "{alpn:?}");
    }
    // plain AMQP
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.write_all(b"AMQP\x00\x01\x00\x00").await.unwrap();
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut reply)).await.unwrap().unwrap();
    assert_eq!(reply, b"AMQP\x00\x01\x00\x00");

    // HTTP/2 over TLS (ALPN) and with prior knowledge on the plain port, with the raw protocol registered
    let mut c = tls_h2(srv.port(), srv.ca_pem(), name).await;
    assert_eq!(call(&mut c, req(Method::GET, "https", &authority, "/p", full(""))).await.status, 200);
    let mut c = plain_h2(srv.port()).await;
    assert_eq!(call(&mut c, req(Method::GET, "http", &authority, "/p", full(""))).await.status, 200);

    // HTTP/2 prior knowledge over TLS without ALPN (the preface is recognised from its first bytes)
    let mut tls = tls_stream(srv.port(), srv.ca_pem(), name, &[]).await;
    let mut hello = PREFACE.to_vec();
    hello.extend(frame(0x4, 0, 0, &[]));
    hello.extend(get_health(1, &authority));
    tls.write_all(&hello).await.unwrap();
    let mut saw_headers = false;
    for _ in 0..6 {
        let mut h = [0u8; 9];
        if tokio::time::timeout(Duration::from_secs(3), tls.read_exact(&mut h)).await.ok().and_then(|r| r.ok()).is_none() {
            break;
        }
        let len = ((h[0] as usize) << 16) | ((h[1] as usize) << 8) | h[2] as usize;
        let mut payload = vec![0u8; len];
        tls.read_exact(&mut payload).await.unwrap();
        if h[3] == 0x1 && payload.first() == Some(&0x88) {
            saw_headers = true;
            break;
        }
    }
    assert!(saw_headers, "no 200 answer to the HTTP/2 request sent without ALPN");

    // the preface arriving in small pieces
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.set_nodelay(true).unwrap();
    s.write_all(&PREFACE[..2]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let mut rest = PREFACE[2..].to_vec();
    rest.extend(frame(0x4, 0, 0, &[]));
    s.write_all(&rest).await.unwrap();
    let (kind, _, _, _) = read_frame(&mut s).await.expect("the server's SETTINGS");
    assert_eq!(kind, 0x4);
}
