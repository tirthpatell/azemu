mod common;

use std::sync::Arc;
use std::time::Duration;

use azemu_core::testing::TestServer;
use azemu_core::{async_trait, Mount, RawConn, RawProtocol, Service};
use common::*;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Default)]
struct Amqpish {
    seen: Mutex<Vec<Vec<u8>>>,
    last_tls: Mutex<Option<(bool, Option<String>)>>,
}

#[async_trait]
impl RawProtocol for Amqpish {
    fn sniff(&self, first: &[u8]) -> bool {
        first.starts_with(b"AMQP")
    }
    async fn serve(&self, conn: RawConn) {
        let mut stream = conn.io;
        *self.last_tls.lock() = Some((conn.tls, conn.server_name.clone()));
        // the sniffed bytes must still be unread on the socket
        let mut hdr = [0u8; 8];
        if stream.read_exact(&mut hdr).await.is_err() {
            return;
        }
        self.seen.lock().push(hdr.to_vec());
        let _ = stream.write_all(b"AMQP\x00\x01\x00\x00").await;
        let _ = stream.shutdown().await;
    }
}

fn http_svc() -> Arc<dyn Service> {
    Arc::new(Probe::new("echo", vec![Mount::AccountHost("echo")]))
}

#[tokio::test]
async fn raw_protocol_gets_the_socket_with_its_bytes() {
    let amqp = Arc::new(Amqpish::default());
    let srv = TestServer::start_with(vec![http_svc()], vec![amqp.clone()]).await;

    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.write_all(b"AMQP\x00\x01\x00\x00").await.unwrap();
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut reply)).await.unwrap().unwrap();
    assert_eq!(reply, b"AMQP\x00\x01\x00\x00");
    assert_eq!(amqp.seen.lock().as_slice(), &[b"AMQP\x00\x01\x00\x00".to_vec()]);

    // bytes that arrive in pieces are still sniffed
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.set_nodelay(true).unwrap();
    s.write_all(b"AM").await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    s.write_all(b"QP\x00\x01\x00\x00").await.unwrap();
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut reply)).await.unwrap().unwrap();
    assert_eq!(reply, b"AMQP\x00\x01\x00\x00");
    assert_eq!(amqp.seen.lock().len(), 2);

    // HTTP and HTTPS still work on the same port
    let r = srv.send("GET", &format!("{}/x", srv.url("echo", "a")), &[], "").await;
    assert_eq!(r.status, 200);
    let r = srv.send("GET", &format!("{}/x", srv.https_url("echo", "a")), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["endpoint"]["scheme"], "https");
}

#[tokio::test]
async fn http_with_raw_protocols_registered_is_not_delayed() {
    let amqp = Arc::new(Amqpish::default());
    let srv = TestServer::start_with(vec![http_svc()], vec![amqp]).await;
    let started = std::time::Instant::now();
    for _ in 0..5 {
        srv.send("GET", &format!("{}/x", srv.url("echo", "a")), &[], "").await;
    }
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_protocol_also_works_inside_tls() {
    let amqp = Arc::new(Amqpish::default());
    let srv = TestServer::start_with(vec![http_svc()], vec![amqp.clone()]).await;
    let ca = std::env::temp_dir().join(format!("azemu-raw-tls-{}.pem", srv.port()));
    std::fs::write(&ca, srv.ca_pem()).unwrap();

    // openssl s_client speaks TLS for us; the payload goes in on stdin.
    let mut child = std::process::Command::new("openssl")
        .args(["s_client", "-quiet", "-connect"])
        .arg(format!("127.0.0.1:{}", srv.port()))
        .args(["-servername", "ns.servicebus.localhost", "-CAfile"])
        .arg(&ca)
        .args(["-verify_return_error"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    use std::io::{Read, Write};
    child.stdin.as_mut().unwrap().write_all(b"AMQP\x00\x01\x00\x00").unwrap();
    let mut reply = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut reply).unwrap();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&ca);

    assert_eq!(reply, b"AMQP\x00\x01\x00\x00");
    assert_eq!(amqp.last_tls.lock().clone(), Some((true, Some("ns.servicebus.localhost".to_string()))));

    // HTTPS on the same port is unaffected by the prefix read
    let r = srv.send("GET", &format!("{}/x", srv.https_url("echo", "a")), &[], "").await;
    assert_eq!(r.status, 200);
}
