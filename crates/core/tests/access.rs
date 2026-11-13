//! The host check (DNS rebinding) and the cross-site check (web pages), over real sockets: HTTP/1.1, HTTP/2, TLS,
//! pinned listeners and the WebSocket upgrade. The rules are in `crates/core/src/access.rs`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use azemu_core::resp::Resp;
use azemu_core::storage::cors::{self, CorsRule};
use azemu_core::testing::TestServer;
use azemu_core::{async_trait, Emulator, Mount, Request, Response, Service, WebSocketProtocol, WsAccept, WsConn, WsRequest};
use bytes::Bytes;
use common::*;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// One raw HTTP/1.1 exchange: the status, the lower-cased head and the body.
struct Answer {
    status: u16,
    head: String,
    body: String,
}

impl Answer {
    fn has_header(&self, name: &str) -> bool {
        self.head.lines().any(|l| l.to_ascii_lowercase().starts_with(&format!("{}:", name.to_ascii_lowercase())))
    }
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string()))
    }
    fn cors_headers(&self) -> Vec<String> {
        self.head.lines().filter(|l| l.to_ascii_lowercase().starts_with("access-control-")).map(String::from).collect()
    }
}

async fn http1(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Answer {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Answer { status, head: head.to_string(), body: body.to_string() }
}

/// A request to `host` (the Host header) with extra headers.
async fn get(port: u16, host: &str, path: &str, extra: &[(&str, &str)]) -> Answer {
    let mut h = vec![("Host", host)];
    h.extend_from_slice(extra);
    http1(port, "GET", path, &h, "").await
}

fn echo() -> Arc<dyn Service> {
    Arc::new(Probe::new("echo", vec![Mount::AccountHost("echo")]))
}

// ---- the host check -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn only_the_documented_hosts_are_served() {
    let srv = TestServer::start(vec![echo()]).await;
    let port = srv.port();
    let served = [
        format!("127.0.0.1:{port}"),
        "127.0.0.1".to_string(),
        format!("[::1]:{port}"),
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
        format!("acct.echo.localhost:{port}"),
        format!("acct.blob.core.localhost:{port}"),
        format!("acct.dfs.core.windows.net.localhost:{port}"),
        format!("azemu:{port}"),
        "my_compose_service".to_string(),
        format!("host.docker.internal:{port}"),
        format!("gateway.docker.internal:{port}"),
        format!("10.20.30.40:{port}"),
        format!("192.168.1.2:{port}"),
        format!("[fd00::5]:{port}"),
    ];
    for host in served {
        let r = get(port, &host, "/_azemu/health", &[]).await;
        assert_eq!(r.status, 200, "{host}: {}", r.body);
    }
    let refused = [
        "evil.example",
        "evil.example:4577",
        "acct.blob.attacker.example",
        "acct.echo.attacker.example",
        "localhost.evil.com",
        "127.0.0.1.evil.com",
        "evil.com.",
        "acct.blob.core.windows.net", // real Azure names need --allow-host core.windows.net
        "management.azure.com",
        "host.docker.internal.evil.com",
        "a b",
        "user@evil.com",
        "evil.com/path",
    ];
    for host in refused {
        // the health path (served before routing), a service path and an admin path: all refused
        for path in ["/_azemu/health", "/c1/b1", "/_azemu/reset"] {
            let r = get(port, host, path, &[]).await;
            assert_eq!(r.status, 403, "{host} {path}: {}", r.body);
            let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
            assert_eq!(v["error"]["code"], "HostNotAllowed");
            assert!(r.cors_headers().is_empty(), "no CORS headers on a refusal");
        }
    }
    // the answer names the host and the option
    let r = get(port, "acct.blob.attacker.example:4577", "/x", &[]).await;
    assert!(r.body.contains("acct.blob.attacker.example:4577"), "{}", r.body);
    assert!(r.body.contains("--allow-host"), "{}", r.body);
    assert!(r.body.contains("AZEMU_ALLOW_HOST"), "{}", r.body);
    // a request with no Host at all (HTTP/1.0) is addressed to the listener's own IP address
    let r = http1(port, "GET", "/_azemu/health", &[], "").await;
    assert_ne!(r.status, 403);
    // the refusal did not touch the state, and the server still works
    assert_eq!(get(port, "localhost", "/_azemu/health", &[]).await.status, 200);
}

#[tokio::test]
async fn the_options_add_names_and_suffixes() {
    let srv = TestServer::start_configured(vec![echo()], vec![], |b| {
        b.allow_host("core.windows.net").allow_host("corp.example,other.example").tls_name("vault.lab").tls_name("10.0.0.0/24")
    })
    .await;
    let port = srv.port();
    for host in
        ["acct.echo.core.windows.net", "core.windows.net", "x.corp.example", "other.example", "a.b.vault.lab", "vault.lab", "10.0.0.7"]
    {
        let r = get(port, host, "/_azemu/health", &[]).await;
        assert_eq!(r.status, 200, "{host}");
    }
    for host in ["windows.net", "evil.core.windows.net.evil.com", "lab", "notcorp.example", "example"] {
        // `lab` and `example` are single labels: served; the others are not
        let r = get(port, host, "/_azemu/health", &[]).await;
        let want = if host == "lab" || host == "example" { 200 } else { 403 };
        assert_eq!(r.status, want, "{host}");
    }
}

#[tokio::test]
async fn a_public_suffix_or_a_huge_range_is_refused_at_start() {
    for bad in ["com", "*.com", "co.uk", "0.0.0.0/0", "::/0", "10.0.0.0/7", "fd00::/31"] {
        let r = Emulator::builder().service(echo()).gateway(loopback0()).cert_dir(temp_dir("bad")).tls_name(bad).start().await;
        assert!(r.is_err(), "--tls-name {bad} must be refused");
        let msg = format!("{:#}", r.err().unwrap());
        assert!(msg.contains("--tls-name"), "{msg}");
    }
    for bad in ["com", "*.com", "co.uk", "io", "*", "a..b"] {
        let r = Emulator::builder().service(echo()).gateway(loopback0()).cert_dir(temp_dir("bad")).allow_host(bad).start().await;
        assert!(r.is_err(), "--allow-host {bad} must be refused");
        assert!(format!("{:#}", r.err().unwrap()).contains("--allow-host"));
    }
    let r = Emulator::builder().service(echo()).gateway(loopback0()).cert_dir(temp_dir("bad")).allow_origin("not a host").start().await;
    assert!(r.is_err());
}

#[tokio::test]
async fn pinned_listeners_check_the_host_too() {
    let dir = temp_dir("pinned-access");
    let emu = Emulator::builder().service(echo()).gateway(loopback0()).pinned(loopback0(), "echo").cert_dir(dir).start().await.unwrap();
    let p = emu.pinned_addr("echo").unwrap().port();
    for host in [format!("127.0.0.1:{p}"), format!("localhost:{p}"), format!("acct.blob.core.localhost:{p}")] {
        assert_eq!(get(p, &host, "/devstoreaccount1/c", &[]).await.status, 200, "{host}");
    }
    let r = get(p, "evil.example", "/devstoreaccount1/c", &[]).await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(get(p, "evil.example", "/_azemu/reset", &[]).await.status, 403);
    emu.shutdown().await;
}

// ---- HTTP/2 and TLS --------------------------------------------------------------------------------------------------

type Body = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

async fn h2_status(
    io: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    authority: &str,
    path: &str,
    extra: &[(&str, &str)],
) -> u16 {
    let (mut sender, conn) = hyper::client::conn::http2::handshake::<_, _, Body>(TokioExecutor::new(), TokioIo::new(io)).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut b = http::Request::builder().method("GET").uri(format!("http://{authority}{path}"));
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    let req = b.body(Full::new(Bytes::new()).map_err(|e| match e {}).boxed()).unwrap();
    sender.send_request(req).await.unwrap().status().as_u16()
}

#[tokio::test]
async fn http2_checks_the_authority() {
    let srv = TestServer::start(vec![echo()]).await;
    let port = srv.port();
    let tcp = || async { TcpStream::connect(("127.0.0.1", port)).await.unwrap() };
    assert_eq!(h2_status(tcp().await, &format!("acct.echo.localhost:{port}"), "/c", &[]).await, 200);
    assert_eq!(h2_status(tcp().await, &format!("127.0.0.1:{port}"), "/_azemu/health", &[]).await, 200);
    assert_eq!(h2_status(tcp().await, &format!("acct.echo.attacker.example:{port}"), "/c", &[]).await, 403);
    assert_eq!(h2_status(tcp().await, &format!("evil.example:{port}"), "/_azemu/reset", &[]).await, 403);
    // a `Host` header that disagrees with the authority is checked as well
    assert_eq!(h2_status(tcp().await, &format!("localhost:{port}"), "/_azemu/health", &[("host", "evil.example")]).await, 403);
    // cross-site over HTTP/2
    assert_eq!(h2_status(tcp().await, &format!("localhost:{port}"), "/_azemu/health", &[("sec-fetch-site", "cross-site")]).await, 403);
    assert_eq!(h2_status(tcp().await, &format!("localhost:{port}"), "/_azemu/health", &[("origin", "http://evil.example")]).await, 403);
    assert_eq!(h2_status(tcp().await, &format!("localhost:{port}"), "/_azemu/health", &[("origin", "http://localhost:3000")]).await, 200);
}

#[tokio::test]
async fn tls_listener_checks_the_host_whatever_the_sni_says() {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let srv = TestServer::start(vec![echo()]).await;
    let mut roots = rustls::RootCertStore::empty();
    for der in CertificateDer::pem_slice_iter(srv.ca_pem().as_bytes()) {
        roots.add(der.unwrap()).unwrap();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    let cfg = Arc::new(cfg);
    let port = srv.port();
    for (authority, want) in [("acct.echo.localhost", 200), ("evil.example", 403)] {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        // the TLS name is always one the CA signs; the host of the request is what the check looks at
        let sni = rustls::pki_types::ServerName::try_from("acct.echo.localhost".to_string()).unwrap();
        let tls = tokio_rustls::TlsConnector::from(cfg.clone()).connect(sni, tcp).await.unwrap();
        assert_eq!(h2_status(tls, &format!("{authority}:{port}"), "/c", &[]).await, want, "{authority}");
    }
}

// ---- cross-site requests -------------------------------------------------------------------------------------------

fn probe_with_idp() -> (Arc<Probe>, Vec<Arc<dyn Service>>) {
    let echo = Arc::new(Probe::new("echo", vec![Mount::AccountHost("echo")]));
    let idp: Arc<dyn Service> = Arc::new(Probe::new("idp", vec![Mount::PathPrefix("/_azemu/identity")]));
    (echo.clone(), vec![echo, idp])
}

#[tokio::test]
async fn admin_paths_refuse_every_cross_site_request_and_never_answer_a_page() {
    let (echo, services) = probe_with_idp();
    let srv = TestServer::start(services).await;
    let port = srv.port();
    let host = format!("localhost:{port}");
    let resets = || echo.resets.load(std::sync::atomic::Ordering::SeqCst);

    // what a hostile page can send: a form POST with a simple content type, a fetch, a link, an <img>
    type Attack<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>);
    let attacks: Vec<Attack> = vec![
        (
            "POST",
            "/_azemu/reset",
            vec![("Origin", "http://evil.example"), ("Content-Type", "text/plain"), ("Sec-Fetch-Site", "cross-site")],
        ),
        ("POST", "/_azemu/reset", vec![("Origin", "http://evil.example"), ("Content-Type", "application/x-www-form-urlencoded")]),
        ("POST", "/_azemu/reset", vec![("Origin", "null")]),
        ("POST", "/_azemu/reset", vec![("Sec-Fetch-Site", "cross-site")]), // a request with no Origin at all
        ("POST", "/_azemu/reset", vec![("Referer", "https://evil.example/page.html")]), // an old browser
        ("GET", "/_azemu/health", vec![("Sec-Fetch-Site", "cross-site"), ("Sec-Fetch-Mode", "navigate")]),
        ("GET", "/_azemu/ca.pem", vec![("Origin", "https://evil.example")]),
        ("GET", "//_azemu/health", vec![("Origin", "https://evil.example")]),
        ("GET", "/_azemu/identity/msi/token", vec![("Origin", "https://evil.example")]), // a service's own admin API
        ("OPTIONS", "/_azemu/reset", vec![("Origin", "http://evil.example"), ("Access-Control-Request-Method", "POST")]),
    ];
    for (method, path, headers) in attacks {
        let mut h = vec![("Host", host.as_str())];
        h.extend(headers.iter().copied());
        let r = http1(port, method, path, &h, "").await;
        assert_eq!(r.status, 403, "{method} {path} {headers:?}: {}", r.body);
        assert!(r.cors_headers().is_empty(), "{method} {path}: no CORS headers, got {:?}", r.cors_headers());
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["error"]["code"], "CrossSiteRequestRefused");
        assert!(r.body.contains("--allow-origin"));
    }
    assert_eq!(resets(), 0, "nothing was reset");

    // the same calls from a program (no browser headers) and from a page on the developer's own machine work
    for h in [
        vec![],
        vec![("Origin", "http://localhost:3000")],
        vec![("Origin", "http://127.0.0.1:5173"), ("Sec-Fetch-Site", "same-site")],
        vec![("Sec-Fetch-Site", "same-origin")],
        vec![("Sec-Fetch-Site", "none")],
    ] {
        let mut hh = vec![("Host", host.as_str())];
        hh.extend(h.iter().copied());
        let r = http1(port, "POST", "/_azemu/reset", &hh, "").await;
        assert_eq!(r.status, 200, "{h:?}: {}", r.body);
    }
    assert_eq!(resets(), 5);
    // ... and a page does not even get a CORS answer for them
    let r = http1(port, "GET", "/_azemu/health", &[("Host", host.as_str()), ("Origin", "http://localhost:3000")], "").await;
    assert!(r.cors_headers().is_empty());
}

#[tokio::test]
async fn allow_origin_opens_service_requests_but_not_admin_paths() {
    let srv =
        TestServer::start_configured(vec![echo()], vec![], |b| b.allow_origin("http://evil.example").allow_origin("*.partner.test")).await;
    let port = srv.port();
    let host = format!("acct.echo.localhost:{port}");
    let cross = |origin: &'static str| {
        vec![("Host", host.as_str().to_string()), ("Origin", origin.to_string()), ("Sec-Fetch-Site", "cross-site".to_string())]
    };
    let call = |method: &'static str, path: &'static str, origin: &'static str| {
        let h = cross(origin);
        async move {
            let refs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
            http1(port, method, path, &refs, "").await
        }
    };
    // a service request from an allowed origin goes through, from another one it does not
    assert_eq!(call("GET", "/c1/b1", "http://evil.example").await.status, 200);
    assert_eq!(call("GET", "/c1/b1", "https://a.partner.test").await.status, 200);
    assert_eq!(call("GET", "/c1/b1", "http://evil.example:8080").await.status, 403, "the port is part of an origin");
    assert_eq!(call("PUT", "/c1/b1", "http://other.example").await.status, 403);
    // an allowed origin still gets nothing from the admin API
    assert_eq!(call("POST", "/_azemu/reset", "http://evil.example").await.status, 403);
    assert_eq!(call("GET", "/_azemu/health", "https://a.partner.test").await.status, 403);
}

#[tokio::test]
async fn star_turns_the_cross_site_check_off_for_everything() {
    let srv = TestServer::start_configured(vec![echo()], vec![], |b| b.allow_origin("*")).await;
    let port = srv.port();
    let h = [("Host", "localhost"), ("Origin", "http://evil.example"), ("Sec-Fetch-Site", "cross-site")];
    assert_eq!(http1(port, "POST", "/_azemu/reset", &h, "").await.status, 200);
    let h = [("Host", "acct.echo.localhost"), ("Sec-Fetch-Site", "cross-site")];
    assert_eq!(http1(port, "GET", "/c1", &h, "").await.status, 200);
    // the host check is not part of it
    let h = [("Host", "evil.example"), ("Origin", "http://evil.example")];
    assert_eq!(http1(port, "GET", "/c1", &h, "").await.status, 403);
}

/// A service with CORS rules, like Blob, Queue and Table: it answers a preflight from them.
struct CorsService {
    rules: Vec<CorsRule>,
}

#[async_trait]
impl Service for CorsService {
    fn name(&self) -> &'static str {
        "cors"
    }
    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("cors")]
    }
    async fn handle(&self, req: Request) -> Response {
        if req.method == http::Method::OPTIONS {
            return cors::preflight(&req, &self.rules);
        }
        let mut resp = Resp::new(200u16).text("data").build();
        cors::apply(&mut resp, &req, &self.rules);
        resp
    }
    fn reset(&self) {}
}

#[tokio::test]
async fn a_service_with_cors_rules_decides_for_its_own_requests() {
    let rule = |origins: &str| CorsRule {
        allowed_origins: origins.into(),
        allowed_methods: "GET,PUT".into(),
        allowed_headers: "x-ms-*,content-type".into(),
        exposed_headers: "*".into(),
        max_age: 60,
    };
    let svc: Arc<dyn Service> = Arc::new(CorsService { rules: vec![rule("https://app.example.test")] });
    let srv = TestServer::start(vec![svc]).await;
    let port = srv.port();
    let host = format!("acct.cors.localhost:{port}");
    let page = |origin: &'static str| {
        vec![("Host", host.as_str().to_string()), ("Origin", origin.to_string()), ("Sec-Fetch-Site", "cross-site".to_string())]
    };
    let send = |method: &'static str, extra: Vec<(&'static str, &'static str)>, origin: &'static str| {
        let mut h = page(origin);
        h.extend(extra.into_iter().map(|(k, v)| (k, v.to_string())));
        async move {
            let refs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
            http1(port, method, "/c1/b1", &refs, "").await
        }
    };
    // the origin the rule names: a simple GET, a PUT with x-ms headers, and the preflight itself
    let r = send("GET", vec![], "https://app.example.test").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("access-control-allow-origin").as_deref(), Some("https://app.example.test"));
    let r =
        send("PUT", vec![("x-ms-blob-type", "BlockBlob"), ("Content-Type", "application/octet-stream")], "https://app.example.test").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = send(
        "OPTIONS",
        vec![("Access-Control-Request-Method", "PUT"), ("Access-Control-Request-Headers", "x-ms-blob-type")],
        "https://app.example.test",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.has_header("access-control-allow-methods"));
    // a method or header the rule does not allow, and another origin: refused by core, no CORS headers
    let r = send("DELETE", vec![], "https://app.example.test").await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(r.body.contains("CrossSiteRequestRefused"), "{}", r.body);
    assert!(r.cors_headers().is_empty());
    let r = send("GET", vec![("x-custom-header", "1")], "https://app.example.test").await;
    assert_eq!(r.status, 403, "a header the rule does not allow: {}", r.body);
    let r = send("GET", vec![], "https://evil.example").await;
    assert_eq!(r.status, 403, "{}", r.body);
    let r = send("OPTIONS", vec![("Access-Control-Request-Method", "GET")], "https://evil.example").await;
    assert_eq!(r.status, 403);
    // an Origin alone (a program that sets it, such as a CORS test) is not a browser saying "cross-site": the service
    // answers as it does on Azure, with no CORS headers for an origin its rules do not name
    let r = http1(port, "GET", "/c1/b1", &[("Host", host.as_str()), ("Origin", "https://evil.example")], "").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(!r.has_header("access-control-allow-origin"));
    // no Origin header (a no-cors GET from an <img>) cannot be approved
    let r = http1(port, "GET", "/c1/b1", &[("Host", host.as_str()), ("Sec-Fetch-Site", "cross-site")], "").await;
    assert_eq!(r.status, 403);
    // a program with no browser headers is not asked anything
    let r = http1(port, "PUT", "/c1/b1", &[("Host", host.as_str())], "").await;
    assert_eq!(r.status, 200);
    // an origin that passes the host check and is same-site needs no rule
    let r =
        http1(port, "PUT", "/c1/b1", &[("Host", host.as_str()), ("Origin", "http://localhost:3000"), ("Sec-Fetch-Site", "same-site")], "")
            .await;
    assert_eq!(r.status, 200);
}

// ---- the WebSocket upgrade ---------------------------------------------------------------------------------------------

struct Accepting;

#[async_trait]
impl WebSocketProtocol for Accepting {
    fn accept(&self, _req: &WsRequest) -> Option<WsAccept> {
        Some(WsAccept { subprotocol: None })
    }
    async fn serve(&self, conn: WsConn) {
        let mut io = conn.io;
        let _ = io.shutdown().await;
    }
}

fn upgrade(host: &str, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut h = vec![
        ("Host".to_string(), host.to_string()),
        ("Upgrade".to_string(), "websocket".to_string()),
        ("Connection".to_string(), "Upgrade".to_string()),
        ("Sec-WebSocket-Key".to_string(), "dGhlIHNhbXBsZSBub25jZQ==".to_string()),
        ("Sec-WebSocket-Version".to_string(), "13".to_string()),
    ];
    h.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    h
}

async fn ws_status(port: u16, headers: Vec<(String, String)>) -> u16 {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut req = String::from("GET /x HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await.unwrap().unwrap();
    String::from_utf8_lossy(&buf[..n]).split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

#[tokio::test]
async fn the_websocket_upgrade_checks_host_and_origin() {
    for (allow, accepted_extra) in [(None, "http://evil.example"), (Some("http://evil.example"), "")] {
        let dir = temp_dir("ws-access");
        let mut b = Emulator::builder().gateway(loopback0()).cert_dir(dir).websocket_protocol(Arc::new(Accepting));
        if let Some(o) = allow {
            b = b.allow_origin(o);
        }
        let emu = b.start().await.unwrap();
        let port = gw_port(&emu);
        let local = format!("localhost:{port}");
        // programs send no Origin; a page on the developer's own machine passes the host check
        assert_eq!(ws_status(port, upgrade(&local, &[])).await, 101);
        assert_eq!(ws_status(port, upgrade(&local, &[("Origin", "http://localhost:3000")])).await, 101);
        assert_eq!(ws_status(port, upgrade(&local, &[("Origin", "http://127.0.0.1:3000"), ("Sec-Fetch-Site", "same-site")])).await, 101);
        // DNS rebinding: Host and Origin are the attacker's name
        assert_eq!(ws_status(port, upgrade(&format!("evil.example:{port}"), &[("Origin", "http://evil.example")])).await, 403);
        assert_eq!(ws_status(port, upgrade(&format!("evil.example:{port}"), &[])).await, 403);
        // a page on another site connecting to the loopback address
        let from_evil = ws_status(port, upgrade(&local, &[("Origin", "http://evil.example"), ("Sec-Fetch-Site", "cross-site")])).await;
        assert_eq!(from_evil, if allow.is_some() { 101 } else { 403 }, "{accepted_extra}");
        assert_eq!(ws_status(port, upgrade(&local, &[("Origin", "null")])).await, 403);
        assert_eq!(ws_status(port, upgrade(&local, &[("Origin", "https://other.example")])).await, 403);
        emu.shutdown().await;
    }
}
