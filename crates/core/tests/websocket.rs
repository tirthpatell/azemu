//! The WebSocket upgrade: handshake, framing over real sockets, TLS, pinned listeners,
//! registered protocols and the raw-protocol fallback.

mod common;

use std::sync::Arc;
use std::time::Duration;

use azemu_core::testing::TestServer;
use azemu_core::{async_trait, Emulator, Mount, RawConn, RawProtocol, Service, WebSocketProtocol, WsAccept, WsConn, WsRequest};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use common::*;
use parking_lot::Mutex;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const EXPECTED_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
const AMQP_HEADER: &[u8] = b"AMQP\x00\x01\x00\x00";

// -- a raw protocol that echoes -------------------------------------------------------------------
#[derive(Default)]
struct AmqpEcho {
    seen: Mutex<Vec<(bool, Option<String>)>>,
}

#[async_trait]
impl RawProtocol for AmqpEcho {
    fn sniff(&self, first: &[u8]) -> bool {
        first.starts_with(b"AMQP")
    }
    async fn serve(&self, conn: RawConn) {
        self.seen.lock().push((conn.tls, conn.server_name.clone()));
        let mut io = conn.io;
        let mut buf = [0u8; 4096];
        loop {
            match io.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if io.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = io.shutdown().await;
    }
}

fn http_svc() -> Arc<dyn Service> {
    Arc::new(Probe::new("echo", vec![Mount::AccountHost("echo")]))
}

// -- a small WebSocket client ---------------------------------------------------------------------
fn masked(op: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
    let key = [0x12u8, 0x34, 0x56, 0x78];
    let mut f = vec![if fin { 0x80 } else { 0 } | op];
    let n = payload.len();
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n <= 65535 {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(&key);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
    f
}

fn upgrade_request(path: &str, host: &str, extra: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 13\r\n{extra}\r\n"
    )
}

/// Read the response head byte by byte (so no frame bytes are swallowed).
async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> String {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut b)).await.unwrap().unwrap();
        assert!(n > 0, "closed while reading the head: {:?}", String::from_utf8_lossy(&head));
        head.push(b[0]);
    }
    String::from_utf8(head).unwrap()
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

async fn frame<S: AsyncRead + Unpin>(s: &mut S) -> (u8, bool, Vec<u8>) {
    let mut h = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut h)).await.unwrap().unwrap();
    assert_eq!(h[1] & 0x80, 0, "server frames must not be masked");
    let len = match h[1] & 0x7f {
        126 => {
            let mut b = [0u8; 2];
            s.read_exact(&mut b).await.unwrap();
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            s.read_exact(&mut b).await.unwrap();
            u64::from_be_bytes(b) as usize
        }
        n => n as usize,
    };
    let mut p = vec![0u8; len];
    s.read_exact(&mut p).await.unwrap();
    (h[0] & 0x0f, h[0] & 0x80 != 0, p)
}

/// Read binary frames until `n` bytes arrived.
async fn read_bytes<S: AsyncRead + Unpin>(s: &mut S, n: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < n {
        let (op, _, p) = frame(s).await;
        assert_eq!(op, 2, "expected a binary frame");
        got.extend(p);
    }
    got
}

async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, path: &str, host: &str, protos: &str) -> String {
    let extra = if protos.is_empty() { String::new() } else { format!("Sec-WebSocket-Protocol: {protos}\r\n") };
    s.write_all(upgrade_request(path, host, &extra).as_bytes()).await.unwrap();
    read_head(s).await
}

async fn tls_connect(srv: &TestServer, name: &str) -> tokio_rustls::client::TlsStream<TcpStream> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
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
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tcp = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(ServerName::try_from(name.to_string()).unwrap(), tcp).await.unwrap()
}

async fn amqp_server() -> (TestServer, Arc<AmqpEcho>) {
    let amqp = Arc::new(AmqpEcho::default());
    (TestServer::start_with(vec![http_svc()], vec![amqp.clone()]).await, amqp)
}

// -- tests ----------------------------------------------------------------------------------------
#[tokio::test]
async fn handshake_answers_101_with_accept_key_and_the_offered_subprotocol() {
    let (srv, _) = amqp_server().await;
    for (offer, echoed) in [("AMQPWSB10", "AMQPWSB10"), ("amqp", "amqp"), ("mqtt, amqp", "amqp"), ("AMQP", "AMQP")] {
        let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
        let head = handshake(&mut s, "/$servicebus/websocket", "ns.servicebus.localhost:5671", offer).await;
        assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
        assert_eq!(header(&head, "sec-websocket-accept"), Some(EXPECTED_ACCEPT));
        assert_eq!(header(&head, "sec-websocket-protocol"), Some(echoed), "{offer}");
        assert_eq!(header(&head, "upgrade").map(str::to_lowercase).as_deref(), Some("websocket"));
        assert!(header(&head, "connection").unwrap().eq_ignore_ascii_case("upgrade"));
    }
    // no subprotocol offered: accepted without one
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let head = handshake(&mut s, "/", "127.0.0.1", "").await;
    assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
    assert_eq!(header(&head, "sec-websocket-protocol"), None);
    // the accept key is the SHA-1 of key + GUID, for any key
    let key2 = B64.encode([7u8; 16]);
    let mut h = Sha1::new();
    h.update(key2.as_bytes());
    h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let want = B64.encode(h.finalize());
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let req = upgrade_request("/", "x", "").replace(KEY, &key2);
    s.write_all(req.as_bytes()).await.unwrap();
    assert_eq!(header(&read_head(&mut s).await, "sec-websocket-accept"), Some(want.as_str()));
}

#[tokio::test]
async fn amqp_bytes_flow_both_ways_across_frame_boundaries() {
    let (srv, amqp) = amqp_server().await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.set_nodelay(true).unwrap();
    handshake(&mut s, "/", "ns.servicebus.localhost:1234", "AMQPWSB10").await;

    // the 8 byte header in two frames (the first has only 3 bytes, so the sniffer must wait),
    // then a message in three fragments with a ping between them, sent in tiny pieces
    s.write_all(&masked(2, true, &AMQP_HEADER[..3])).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.write_all(&masked(2, true, &AMQP_HEADER[3..])).await.unwrap();
    assert_eq!(read_bytes(&mut s, 8).await, AMQP_HEADER);

    let mut wire = masked(2, false, b"one-");
    wire.extend(masked(9, true, b"ping!"));
    wire.extend(masked(0, false, b"two-"));
    wire.extend(masked(0, true, b"three"));
    for piece in wire.chunks(5) {
        s.write_all(piece).await.unwrap();
        tokio::task::yield_now().await;
    }
    // the echo and the pong may come in either order
    let mut got = Vec::new();
    let mut pong = None;
    while got.len() < 13 || pong.is_none() {
        let (op, fin, p) = frame(&mut s).await;
        match op {
            2 => {
                assert!(fin);
                got.extend(p)
            }
            10 => pong = Some(p),
            other => panic!("unexpected opcode {other}"),
        }
    }
    assert_eq!(got, b"one-two-three");
    assert_eq!(pong.unwrap(), b"ping!");

    // plain connection: the server name is the Host without the port
    assert_eq!(amqp.seen.lock().as_slice(), &[(false, Some("ns.servicebus.localhost".to_string()))]);
}

#[tokio::test]
async fn a_large_write_arrives_as_many_frames() {
    let (srv, _) = amqp_server().await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    handshake(&mut s, "/", "h", "amqp").await;
    let mut data = AMQP_HEADER.to_vec();
    data.extend((0..300_000u32).map(|i| (i * 7 % 256) as u8));
    let mut wire = Vec::new();
    for chunk in data.chunks(40_000) {
        wire.extend(masked(2, true, chunk));
    }
    let (mut rd, mut wr) = tokio::io::split(s);
    let writer = tokio::spawn(async move { wr.write_all(&wire).await.unwrap() });
    let mut frames = 0;
    let mut got = Vec::new();
    while got.len() < data.len() {
        let (op, _, p) = frame(&mut rd).await;
        assert_eq!(op, 2);
        assert!(p.len() <= 32 * 1024);
        got.extend(p);
        frames += 1;
    }
    writer.await.unwrap();
    assert_eq!(got, data);
    assert!(frames >= 10, "{frames} frames");
}

#[tokio::test]
async fn wss_works_on_the_same_port_and_reports_tls_and_the_sni_name() {
    let (srv, amqp) = amqp_server().await;
    let mut s = tls_connect(&srv, "ns.eventhub.localhost").await;
    let head = handshake(&mut s, "/$servicebus/websocket", "ns.eventhub.localhost:1", "AMQPWSB10").await;
    assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
    s.write_all(&masked(2, true, AMQP_HEADER)).await.unwrap();
    assert_eq!(read_bytes(&mut s, 8).await, AMQP_HEADER);
    assert_eq!(amqp.seen.lock().as_slice(), &[(true, Some("ns.eventhub.localhost".to_string()))]);
    // the server closes properly when the client does
    s.write_all(&masked(8, true, &1000u16.to_be_bytes())).await.unwrap();
    let (op, _, p) = frame(&mut s).await;
    assert_eq!((op, p), (8, 1000u16.to_be_bytes().to_vec()));
}

#[tokio::test]
async fn plain_http_and_https_still_work_beside_websocket() {
    let (srv, _) = amqp_server().await;
    for url in [srv.url("echo", "a"), srv.https_url("echo", "a")] {
        let r = srv.send("GET", &format!("{url}/x"), &[], "").await;
        assert_eq!(r.status, 200);
    }
    // a client that talks HTTP on a keep-alive connection and never upgrades is left alone
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.write_all(b"GET /_azemu/health HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n").await.unwrap();
    let head = read_head(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

#[tokio::test]
async fn upgrade_without_raw_protocols_or_with_other_subprotocols_is_ordinary_http() {
    // no raw protocol registered: nobody wants the upgrade, the gateway answers 404 as before
    let srv = TestServer::start(vec![http_svc()]).await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let head = handshake(&mut s, "/", "127.0.0.1", "amqp").await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");

    // a subprotocol that is not AMQP is not taken either
    let (srv, _) = amqp_server().await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    let head = handshake(&mut s, "/", "127.0.0.1", "mqtt").await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");

    // a request that only mentions "Upgrade" for another protocol
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.write_all(b"GET /_azemu/health HTTP/1.1\r\nHost: x\r\nUpgrade: h2c\r\nConnection: Upgrade\r\n\r\n").await.unwrap();
    assert!(read_head(&mut s).await.starts_with("HTTP/1.1 200"));
}

#[tokio::test]
async fn bad_handshakes_get_400_or_426() {
    let (srv, _) = amqp_server().await;
    let send = |req: String| {
        let port = srv.port();
        async move {
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(req.as_bytes()).await.unwrap();
            read_head(&mut s).await
        }
    };
    let good = upgrade_request("/", "h", "Sec-WebSocket-Protocol: amqp\r\n");
    let head = send(good.replace("Version: 13", "Version: 8")).await;
    assert!(head.starts_with("HTTP/1.1 426"), "{head}");
    assert_eq!(header(&head, "sec-websocket-version"), Some("13"));
    let head = send(good.replace(KEY, "not-base64!")).await;
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    let head = send(good.replace(KEY, &B64.encode([1u8; 5]))).await;
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    let head = send(good.replace("Sec-WebSocket-Key", "X-Other")).await;
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    let head = send(good.replace("GET", "POST")).await;
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
}

#[tokio::test]
async fn data_that_no_raw_protocol_knows_is_closed_with_1003() {
    let (srv, _) = amqp_server().await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    handshake(&mut s, "/", "h", "amqp").await;
    s.write_all(&masked(2, true, b"GET / HTTP/1.1")).await.unwrap();
    let (op, _, p) = frame(&mut s).await;
    assert_eq!(op, 8);
    assert_eq!(&p[..2], 1003u16.to_be_bytes());
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn a_client_that_upgrades_and_sends_nothing_is_dropped_without_hanging_the_server() {
    let (srv, _) = amqp_server().await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    handshake(&mut s, "/", "h", "amqp").await;
    drop(s);
    // another connection is served at once
    let r = srv.send("GET", &format!("{}/x", srv.url("echo", "a")), &[], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn unmasked_client_frames_and_oversize_frames_close_the_connection() {
    let amqp = Arc::new(AmqpEcho::default());
    let dir = temp_dir("ws-limit");
    let emu =
        Emulator::builder().gateway(loopback0()).cert_dir(dir).raw_protocol(amqp).websocket_max_frame_bytes(1000).start().await.unwrap();
    let port = gw_port(&emu);

    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    handshake(&mut s, "/", "h", "amqp").await;
    s.write_all(&masked(2, true, AMQP_HEADER)).await.unwrap();
    read_bytes(&mut s, 8).await;
    s.write_all(&masked(2, true, &vec![1u8; 1001])).await.unwrap();
    let (op, _, p) = frame(&mut s).await;
    assert_eq!((op, &p[..2]), (8, &1009u16.to_be_bytes()[..]));

    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    handshake(&mut s, "/", "h", "amqp").await;
    s.write_all(&masked(2, true, AMQP_HEADER)).await.unwrap();
    read_bytes(&mut s, 8).await;
    s.write_all(&[0x82, 0x02, b'h', b'i']).await.unwrap(); // not masked
    let (op, _, p) = frame(&mut s).await;
    assert_eq!((op, &p[..2]), (8, &1002u16.to_be_bytes()[..]));
    emu.shutdown().await;
}

#[tokio::test]
async fn pinned_listeners_take_the_upgrade_too() {
    let amqp = Arc::new(AmqpEcho::default());
    let dir = temp_dir("ws-pinned");
    let emu = Emulator::builder().service(http_svc()).pinned(loopback0(), "echo").cert_dir(dir).raw_protocol(amqp).start().await.unwrap();
    let pin = emu.pinned_addr("echo").unwrap();
    let mut s = TcpStream::connect(pin).await.unwrap();
    let head = handshake(&mut s, "/$servicebus/websocket", "127.0.0.1", "AMQPWSB10").await;
    assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
    s.write_all(&masked(2, true, AMQP_HEADER)).await.unwrap();
    assert_eq!(read_bytes(&mut s, 8).await, AMQP_HEADER);
    emu.shutdown().await;
}

// -- a registered protocol ------------------------------------------------------------------------
struct Shout {
    seen: Mutex<Vec<WsRequest>>,
}

#[async_trait]
impl WebSocketProtocol for Shout {
    fn accept(&self, req: &WsRequest) -> Option<WsAccept> {
        if req.path != "/shout" {
            return None;
        }
        self.seen.lock().push(req.clone());
        Some(WsAccept { subprotocol: req.subprotocols.first().cloned() })
    }
    async fn serve(&self, mut conn: WsConn) {
        let mut buf = [0u8; 256];
        loop {
            match conn.io.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let up = buf[..n].to_ascii_uppercase();
                    if conn.io.write_all(&up).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = conn.io.shutdown().await;
    }
}

#[tokio::test]
async fn a_registered_protocol_wins_over_the_raw_fallback_and_sees_the_request() {
    let shout = Arc::new(Shout { seen: Mutex::new(vec![]) });
    let amqp = Arc::new(AmqpEcho::default());
    let dir = temp_dir("ws-reg");
    let emu = Emulator::builder()
        .gateway(loopback0())
        .cert_dir(dir)
        .allow_host("h.example")
        .raw_protocol(amqp.clone())
        .websocket_protocol(shout.clone())
        .start()
        .await
        .unwrap();
    let port = gw_port(&emu);

    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(upgrade_request("/shout?a=b", "h.example:99", "Sec-WebSocket-Protocol: chat\r\nX-Thing: 1\r\n").as_bytes()).await.unwrap();
    let head = read_head(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
    assert_eq!(header(&head, "sec-websocket-protocol"), Some("chat"));
    s.write_all(&masked(1, true, b"hello")).await.unwrap();
    let (op, _, p) = frame(&mut s).await;
    assert_eq!((op, p.as_slice()), (2, &b"HELLO"[..]));
    {
        let seen = shout.seen.lock();
        assert_eq!(seen[0].query, "a=b");
        assert_eq!(seen[0].host, "h.example:99");
        assert_eq!(seen[0].headers.get("x-thing").unwrap(), "1");
        assert!(!seen[0].tls);
    }

    // other paths still reach the raw protocols
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    handshake(&mut s, "/other", "h", "amqp").await;
    s.write_all(&masked(2, true, AMQP_HEADER)).await.unwrap();
    assert_eq!(read_bytes(&mut s, 8).await, AMQP_HEADER);
    assert_eq!(amqp.seen.lock().len(), 1);
    emu.shutdown().await;
}

#[tokio::test]
async fn shutdown_ends_open_websocket_connections() {
    let amqp = Arc::new(AmqpEcho::default());
    let dir = temp_dir("ws-shutdown");
    let emu = Emulator::builder().gateway(loopback0()).cert_dir(dir).raw_protocol(amqp).start().await.unwrap();
    let port = gw_port(&emu);
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    handshake(&mut s, "/", "h", "amqp").await;
    s.write_all(&masked(2, true, AMQP_HEADER)).await.unwrap();
    read_bytes(&mut s, 8).await;
    emu.shutdown().await;
    let mut rest = Vec::new();
    let r = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut rest)).await;
    assert!(r.is_ok(), "the connection stayed open after shutdown");
}
