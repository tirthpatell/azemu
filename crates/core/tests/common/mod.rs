#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use azemu_core::resp::Resp;
use azemu_core::{async_trait, Emulator, Mount, Request, Response, RunningEmulator, Service};

pub fn loopback0() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

pub fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("azemu-it-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A configurable service that echoes what it received, plus a few special paths.
pub struct Probe {
    pub name: &'static str,
    pub mounts: Vec<Mount>,
    pub path_style: bool,
    pub default_account: &'static str,
    pub resets: AtomicUsize,
    /// Set by `with_account_reset`: this service implements `reset_account`.
    pub supports_account_reset: bool,
    /// The accounts `reset_account` was called with.
    pub account_resets: std::sync::Mutex<Vec<String>>,
}

impl Probe {
    pub fn new(name: &'static str, mounts: Vec<Mount>) -> Probe {
        Probe {
            name,
            mounts,
            path_style: false,
            default_account: "default",
            resets: AtomicUsize::new(0),
            supports_account_reset: false,
            account_resets: Default::default(),
        }
    }
    pub fn with_account_reset(mut self) -> Probe {
        self.supports_account_reset = true;
        self
    }
    pub fn path_style(mut self, default_account: &'static str) -> Probe {
        self.path_style = true;
        self.default_account = default_account;
        self
    }
}

#[async_trait]
impl Service for Probe {
    fn name(&self) -> &'static str {
        self.name
    }
    fn mounts(&self) -> Vec<Mount> {
        self.mounts.clone()
    }
    fn path_style_on_pinned(&self) -> bool {
        self.path_style
    }
    fn default_account(&self) -> &'static str {
        self.default_account
    }
    async fn handle(&self, req: Request) -> Response {
        match req.path.as_str() {
            "/head-cl" => {
                // like Get Blob Properties: empty body, Content-Length of the resource
                return Resp::new(200u16).header("Content-Length", 1234).build();
            }
            "/set-cl" => {
                return Resp::new(200u16).header("Content-Length", 5).body("hello").build();
            }
            "/chunked" => {
                // the service chose chunked framing itself; core must not add Content-Length
                return Resp::new(200u16).header("Transfer-Encoding", "chunked").body("abc").build();
            }
            "/big" => {
                // `n` bytes of a repeating pattern: a large download
                let n: usize = req.query.get("n").and_then(|v| v.parse().ok()).unwrap_or(0);
                let body: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
                return Resp::new(200u16).header("Content-Type", "application/octet-stream").body(body).build();
            }
            "/headers" => {
                // the header names the service saw, and the value of x-ms-probe
                let mut names: Vec<String> = req.headers.keys().map(|k| k.as_str().to_string()).collect();
                names.sort();
                return Resp::new(200u16)
                    .header("X-Ms-Echo-Probe", req.header("x-ms-probe").unwrap_or(""))
                    .json(&serde_json::json!({"names": names, "host": req.header("host")}))
                    .build();
            }
            "/panic" => panic!("boom"),
            // a 501 the way most services make it, and one that names its operation
            "/unbuilt" => return azemu_core::json_error::response(501, "NotImplemented", "probe: GET /unbuilt"),
            "/namedunbuilt" => {
                return azemu_core::unsupported::mark(
                    azemu_core::json_error::response(501, "NotImplemented", "probe: GetThing for acct-123"),
                    "GetThing",
                )
            }
            // a resource type ARM stored without emulating it
            "/stored" => {
                let first = azemu_core::unsupported::note_stored_only("Microsoft.Web/sites");
                return Resp::new(200u16).body(first.to_string()).build();
            }
            "/set-date" => {
                return Resp::new(200u16).header("Date", "Thu, 01 Jan 2099 00:00:00 GMT").build();
            }
            "/no-content" => return Resp::new(204u16).build(),
            _ => {}
        }
        let e = &req.ctx.endpoint;
        Resp::new(200u16)
            .json(&serde_json::json!({
                "service": self.name,
                "method": req.method.as_str(),
                "path": req.path,
                "raw_query": req.query.raw(),
                "account": req.account,
                "body_len": req.body.len(),
                "endpoint": {
                    "scheme": e.scheme,
                    "authority": e.authority,
                    "path_style": e.path_style,
                    "account": e.account,
                    "base_url": e.base_url(),
                },
            }))
            .build()
    }
    fn reset(&self) {
        self.resets.fetch_add(1, Ordering::SeqCst);
    }
    fn reset_account(&self, account: &str) -> bool {
        if self.supports_account_reset {
            self.account_resets.lock().unwrap().push(account.to_string());
        }
        self.supports_account_reset
    }
}

pub async fn start_gateway(services: Vec<Arc<dyn Service>>) -> (RunningEmulator, PathBuf) {
    let dir = temp_dir("gw");
    let mut b = Emulator::builder().gateway(loopback0()).cert_dir(dir.clone());
    for s in services {
        b = b.service(s);
    }
    (b.start().await.unwrap(), dir)
}

pub fn gw_port(e: &RunningEmulator) -> u16 {
    e.gateway_addrs()[0].port()
}

/// Read one HTTP/1.1 response (head + Content-Length body) from a buffered stream.
pub async fn read_response<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> (String, Vec<u8>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).await.unwrap();
        assert!(n > 0, "connection closed while reading response head: {head:?}");
        head.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let lower = head.to_ascii_lowercase();
    let len = lower.lines().find_map(|l| l.strip_prefix("content-length:")).map(|v| v.trim().parse::<usize>().unwrap()).unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.unwrap();
    (head, body)
}

/// One GET with a Host header of our choosing: status and body.
pub async fn raw_get(port: u16, host: &str, path: &str, extra: &[(&str, &str)]) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0), body.to_string())
}
