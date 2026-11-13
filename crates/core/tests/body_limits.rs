//! A service can lower the request body limit for a request line (`Service::max_body_bytes`) and shape the
//! refusal (`Service::body_too_large`). Core refuses before it reads the body.

mod common;

use std::sync::Arc;
use std::time::Duration;

use azemu_core::resp::Resp;
use azemu_core::{async_trait, Emulator, Mount, Request, Response, Service};
use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `/small…` takes 1000 bytes, `/huge…` asks for more than core allows, everything else has no own limit.
struct Limited {
    custom_refusal: bool,
}

#[async_trait]
impl Service for Limited {
    fn name(&self) -> &'static str {
        "lim"
    }
    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("lim")]
    }
    fn max_body_bytes(&self, method: &http::Method, path: &str) -> Option<usize> {
        if path.starts_with("/small") {
            Some(1000)
        } else if path.starts_with("/huge") {
            Some(usize::MAX)
        } else if path.starts_with("/get-only") && method == http::Method::GET {
            Some(0)
        } else {
            None
        }
    }
    fn body_too_large(&self, limit: usize) -> Option<Response> {
        self.custom_refusal.then(|| {
            Resp::new(413u16).json(&serde_json::json!({"code": "RequestEntityTooLarge", "message": format!("over {limit}")})).build()
        })
    }
    async fn handle(&self, req: Request) -> Response {
        Resp::new(200u16).json(&serde_json::json!({"body_len": req.body.len(), "path": req.path})).build()
    }
    fn reset(&self) {}
}

async fn start(custom: bool, global_max: usize) -> (azemu_core::RunningEmulator, u16) {
    let emu = Emulator::builder()
        .gateway(loopback0())
        .cert_dir(temp_dir("body-limits"))
        .service(Arc::new(Limited { custom_refusal: custom }))
        .max_body_bytes(global_max)
        .start()
        .await
        .unwrap();
    let port = gw_port(&emu);
    (emu, port)
}

async fn raw(port: u16, request: &[u8]) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(request).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = [0u8; 4096];
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if out.windows(4).any(|w| w == b"\r\n\r\n")
                        && String::from_utf8_lossy(&out).to_ascii_lowercase().contains("content-length")
                    {
                        // the body of a small answer follows the head in the same write
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(100), s.read(&mut buf)).await {
                            out.extend_from_slice(&buf[..n]);
                        }
                        break;
                    }
                }
            }
        }
    })
    .await;
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn a_declared_length_over_the_service_limit_is_refused_without_reading_the_body() {
    let (emu, port) = start(false, 1 << 30).await;
    // the head says 5000 bytes and no body follows: the 413 comes anyway
    let head = format!("PUT /small HTTP/1.1\r\nHost: a.lim.localhost:{port}\r\nContent-Length: 5000\r\n\r\n");
    let out = raw(port, head.as_bytes()).await;
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
    assert!(out.contains("RequestBodyTooLarge") && out.contains("1000 bytes"), "{out}");
    emu.shutdown().await;
}

#[tokio::test]
async fn a_chunked_body_over_the_service_limit_is_cut_and_other_paths_keep_the_global_limit() {
    let (emu, port) = start(false, 1 << 30).await;
    let chunk = "a".repeat(600);
    let body = format!("{:x}\r\n{chunk}\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n", 600, 600);
    let req = format!("PUT /small HTTP/1.1\r\nHost: a.lim.localhost:{port}\r\nTransfer-Encoding: chunked\r\n\r\n{body}");
    let out = raw(port, req.as_bytes()).await;
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
    // exactly at the limit is fine
    let r = format!("PUT /small HTTP/1.1\r\nHost: a.lim.localhost:{port}\r\nContent-Length: 1000\r\n\r\n{}", "b".repeat(1000));
    assert!(raw(port, r.as_bytes()).await.starts_with("HTTP/1.1 200"));
    // a path with no limit of its own takes far more
    let r = format!("PUT /elsewhere HTTP/1.1\r\nHost: a.lim.localhost:{port}\r\nContent-Length: 300000\r\n\r\n{}", "c".repeat(300_000));
    let out = raw(port, r.as_bytes()).await;
    assert!(out.starts_with("HTTP/1.1 200") && out.contains("\"body_len\":300000"), "{out}");
    // the limit depends on the method too
    let r = format!("GET /get-only HTTP/1.1\r\nHost: a.lim.localhost:{port}\r\nContent-Length: 10\r\n\r\n0123456789");
    assert!(raw(port, r.as_bytes()).await.starts_with("HTTP/1.1 413"));
    let r = format!("PUT /get-only HTTP/1.1\r\nHost: a.lim.localhost:{port}\r\nContent-Length: 10\r\n\r\n0123456789");
    assert!(raw(port, r.as_bytes()).await.starts_with("HTTP/1.1 200"));
    emu.shutdown().await;
}

#[tokio::test]
async fn a_service_can_only_lower_the_limit_not_raise_it() {
    let (emu, port) = start(false, 2000).await;
    let r = format!("PUT /huge HTTP/1.1\r\nHost: a.lim.localhost:{port}\r\nContent-Length: 3000\r\n\r\n{}", "d".repeat(3000));
    let out = raw(port, r.as_bytes()).await;
    assert!(out.starts_with("HTTP/1.1 413") && out.contains("2000 bytes"), "{out}");
    emu.shutdown().await;
}

#[tokio::test]
async fn the_service_shapes_its_own_refusal() {
    let (emu, port) = start(true, 1 << 30).await;
    let head = format!("PUT /small HTTP/1.1\r\nHost: a.lim.localhost:{port}\r\nContent-Length: 5000\r\n\r\n");
    let out = raw(port, head.as_bytes()).await;
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
    assert!(out.contains("\"code\":\"RequestEntityTooLarge\"") && out.contains("over 1000"), "{out}");
    assert!(!out.contains("RequestBodyTooLarge"));
    emu.shutdown().await;
}
