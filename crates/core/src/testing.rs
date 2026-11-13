//! Test support (cargo feature `testing`): an in-process emulator on a free port, a
//! small HTTP client that works for `*.localhost` names without DNS, and
//! [`EchoService`] for routing tests.
//!
//! ```ignore
//! let srv = TestServer::start(vec![Arc::new(MyService::new())]).await;
//! let r = srv.send("PUT", &format!("{}/c1", srv.url("blob", "acct")), &[("x-ms-version", "2021-08-06")], "body").await;
//! assert_eq!(r.status, 201);
//! ```

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;

use crate::resp::Resp;
use crate::{Emulator, Mount, RawProtocol, Request, Response, RunningEmulator, Service};

/// An emulator bound to `127.0.0.1:0` with a temporary cert dir. Stops and cleans up
/// when dropped.
pub struct TestServer {
    emu: Option<RunningEmulator>,
    port: u16,
    dir: PathBuf,
    client: Client,
}

impl TestServer {
    /// Start a gateway on a free port with the given services.
    pub async fn start(services: Vec<Arc<dyn Service>>) -> TestServer {
        TestServer::start_with(services, vec![]).await
    }

    /// Same, with raw (non-HTTP) protocols.
    pub async fn start_with(services: Vec<Arc<dyn Service>>, raw: Vec<Arc<dyn RawProtocol>>) -> TestServer {
        TestServer::start_configured(services, raw, |b| b).await
    }

    /// Same, and `configure` may change the builder first (`allow_host`, `allow_origin`, `tls_name`, ...).
    pub async fn start_configured(
        services: Vec<Arc<dyn Service>>,
        raw: Vec<Arc<dyn RawProtocol>>,
        configure: impl FnOnce(crate::EmulatorBuilder) -> crate::EmulatorBuilder,
    ) -> TestServer {
        let dir = test_cert_dir();
        let mut b = configure(Emulator::builder().gateway(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).cert_dir(dir.clone()));
        for s in services {
            b = b.service(s);
        }
        for r in raw {
            b = b.raw_protocol(r);
        }
        let emu = b.start().await.expect("TestServer: emulator failed to start");
        let port = emu.gateway_addrs()[0].port();
        let client = Client::with_ca(emu.ca_pem());
        TestServer { emu: Some(emu), port, dir, client }
    }

    /// Start with a data directory (`--data-dir`) and no timed saves: state is written by `save_now` and
    /// `shutdown`. Start another server on the same directory after `shutdown` to look at what survived.
    /// The directory is the caller's; it is not removed.
    pub async fn start_persistent(services: Vec<Arc<dyn Service>>, data_dir: &std::path::Path) -> TestServer {
        let dir = data_dir.to_path_buf();
        TestServer::start_configured(services, vec![], move |b| b.data_dir(dir).save_interval(std::time::Duration::ZERO)).await
    }

    /// Like `start_persistent`, but returns the error instead of panicking (a directory in use, unreadable
    /// state, ...). `configure` may change the builder further.
    pub async fn try_start_persistent(
        services: Vec<Arc<dyn Service>>,
        data_dir: &std::path::Path,
        configure: impl FnOnce(crate::EmulatorBuilder) -> crate::EmulatorBuilder,
    ) -> anyhow::Result<TestServer> {
        let dir = data_dir.to_path_buf();
        let cert = test_cert_dir();
        let mut b = configure(
            Emulator::builder()
                .gateway(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .cert_dir(cert.clone())
                .data_dir(dir)
                .save_interval(std::time::Duration::ZERO),
        );
        for s in services {
            b = b.service(s);
        }
        let emu = match b.start().await {
            Ok(e) => e,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&cert);
                return Err(e);
            }
        };
        let port = emu.gateway_addrs()[0].port();
        let client = Client::with_ca(emu.ca_pem());
        Ok(TestServer { emu: Some(emu), port, dir: cert, client })
    }

    /// Save what changed now (see [`RunningEmulator::save_now`]).
    pub async fn save_now(&self) -> Vec<crate::ServiceSave> {
        self.emu().save_now().await
    }

    /// Stop the server the way the binary does on SIGTERM, saving to the data directory if there is one.
    /// Dropping a `TestServer` does not save (it behaves like a crash).
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        match self.emu.take() {
            Some(e) => e.shutdown_checked().await,
            None => Ok(()),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>`
    pub fn gateway_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// `http://{account}.{label}.localhost:{port}`
    pub fn url(&self, label: &str, account: &str) -> String {
        format!("http://{account}.{label}.localhost:{}", self.port)
    }

    /// Same as [`url`](Self::url) over TLS. The test client trusts the server's CA.
    pub fn https_url(&self, label: &str, account: &str) -> String {
        format!("https://{account}.{label}.localhost:{}", self.port)
    }

    pub fn ca_pem(&self) -> &str {
        self.emu().ca_pem()
    }

    pub fn emulator(&self) -> &RunningEmulator {
        self.emu()
    }

    fn emu(&self) -> &RunningEmulator {
        self.emu.as_ref().expect("emulator already shut down")
    }

    /// Call `reset()` on every service.
    pub fn reset(&self) {
        self.emu().reset();
    }

    /// The client this server uses (trusts this server's CA for `https://`).
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// One request; see [`Client::send`].
    pub async fn send(&self, method: &str, url: &str, headers: &[(&str, &str)], body: impl Into<Bytes>) -> TestResponse {
        self.client.send(method, url, headers, body).await
    }

    /// Request builder; see [`Client::request`].
    pub fn request(&self, method: &str, url: &str) -> RequestBuilder<'_> {
        self.client.request(method, url)
    }
}

fn test_cert_dir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "azemu-test-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ))
}

impl Drop for TestServer {
    fn drop(&mut self) {
        drop(self.emu.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A response from the test client.
#[derive(Debug, Clone)]
pub struct TestResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl TestResponse {
    /// Body as text (invalid UTF-8 becomes U+FFFD).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Header value, case-insensitive; `None` if absent or not UTF-8.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Body parsed as JSON. Panics (it is a test helper) when it is not JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.text()))
    }
}

/// Minimal HTTP/1.1 client. It connects to `127.0.0.1:<port from the URL>` whatever
/// the host in the URL is, and sends that host in the `Host` header, so
/// `http://acct.blob.localhost:1234` works without DNS. One connection per request.
#[derive(Clone, Default)]
pub struct Client {
    ca_pem: Option<String>,
}

impl Client {
    /// A client for `http://` URLs (and `https://` with no trusted CA, which fails).
    pub fn new() -> Client {
        Client::default()
    }

    /// A client that also verifies `https://` servers against this CA certificate.
    pub fn with_ca(ca_pem: &str) -> Client {
        Client { ca_pem: Some(ca_pem.to_string()) }
    }

    pub fn request(&self, method: &str, url: &str) -> RequestBuilder<'_> {
        RequestBuilder { client: self, method: method.to_string(), url: url.to_string(), headers: Vec::new(), body: Bytes::new() }
    }

    /// Send one request and read the whole response. Panics (it is a test helper) on
    /// transport errors; use [`try_send`](Self::try_send) to handle them.
    pub async fn send(&self, method: &str, url: &str, headers: &[(&str, &str)], body: impl Into<Bytes>) -> TestResponse {
        self.try_send(method, url, headers, body).await.unwrap_or_else(|e| panic!("{method} {url} failed: {e:#}"))
    }

    pub async fn try_send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: impl Into<Bytes>,
    ) -> anyhow::Result<TestResponse> {
        use anyhow::Context;
        let uri: Uri = url.parse().with_context(|| format!("bad URL {url}"))?;
        let scheme = uri.scheme_str().unwrap_or("http").to_string();
        let authority = uri.authority().context("URL has no host")?.clone();
        let https = scheme == "https";
        let port = authority.port_u16().unwrap_or(if https { 443 } else { 80 });
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await?;
        stream.set_nodelay(true)?;

        let mut req = http::Request::builder()
            .method(Method::from_bytes(method.as_bytes())?)
            .uri(uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"))
            .header(http::header::HOST, authority.as_str());
        for (k, v) in headers {
            req = req.header(HeaderName::from_bytes(k.as_bytes())?, HeaderValue::from_str(v)?);
        }
        let req = req.body(Full::new(body.into()))?;

        if https {
            let ca = self.ca_pem.as_deref().context("https needs a client with a CA")?;
            let cfg = client_tls_config(ca)?;
            let name = rustls::pki_types::ServerName::try_from(authority.host().to_string())?;
            let tls = tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(name, stream).await?;
            roundtrip(TokioIo::new(tls), req).await
        } else {
            roundtrip(TokioIo::new(stream), req).await
        }
    }
}

fn client_tls_config(ca_pem: &str) -> anyhow::Result<rustls::ClientConfig> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let mut roots = rustls::RootCertStore::empty();
    for der in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        roots.add(der?)?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

async fn roundtrip<I>(io: TokioIo<I>, req: http::Request<Full<Bytes>>) -> anyhow::Result<TestResponse>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender.send_request(req).await?;
    let (parts, body) = resp.into_parts();
    let body = body.collect().await?.to_bytes();
    Ok(TestResponse { status: parts.status.as_u16(), headers: parts.headers, body })
}

/// Builder returned by `request`.
pub struct RequestBuilder<'a> {
    client: &'a Client,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Bytes,
}

impl RequestBuilder<'_> {
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }

    pub fn body(mut self, b: impl Into<Bytes>) -> Self {
        self.body = b.into();
        self
    }

    /// JSON body plus `Content-Type: application/json`.
    pub fn json<T: serde::Serialize>(mut self, v: &T) -> Self {
        self.body = Bytes::from(serde_json::to_vec(v).expect("serializable"));
        self.headers.push(("content-type".into(), "application/json".into()));
        self
    }

    pub async fn send(self) -> TestResponse {
        let hs: Vec<(&str, &str)> = self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        self.client.send(&self.method, &self.url, &hs, self.body).await
    }
}

/// Replies with a JSON description of the request it received. Mount
/// `AccountHost("echo")`; `path_style_on_pinned` is true.
///
/// ```json
/// {"service":"echo","method":"GET","path":"/a/b","raw_query":"x=1","query":[["x","1"]],
///  "account":"acct","body_len":0,"body_sha256":"…","request_id":"…",
///  "endpoint":{"scheme":"http","authority":"…","path_style":false,"account":"acct","base_url":"…"}}
/// ```
#[derive(Default)]
pub struct EchoService {
    resets: AtomicUsize,
}

impl EchoService {
    pub fn new() -> EchoService {
        EchoService::default()
    }

    /// How many times `reset()` was called.
    pub fn reset_count(&self) -> usize {
        self.resets.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Service for EchoService {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("echo")]
    }

    fn path_style_on_pinned(&self) -> bool {
        true
    }

    fn default_account(&self) -> &'static str {
        "devaccount"
    }

    async fn handle(&self, req: Request) -> Response {
        let mut hash = String::with_capacity(64);
        for b in Sha256::digest(&req.body) {
            hash.push_str(&format!("{b:02x}"));
        }
        let e = &req.ctx.endpoint;
        let v = serde_json::json!({
            "service": "echo",
            "method": req.method.as_str(),
            "path": req.path,
            "raw_query": req.query.raw(),
            "query": req.query.pairs(),
            "account": req.account,
            "body_len": req.body.len(),
            "body_sha256": hash,
            "request_id": req.ctx.request_id,
            "endpoint": {
                "scheme": e.scheme,
                "authority": e.authority,
                "path_style": e.path_style,
                "account": e.account,
                "base_url": e.base_url(),
            },
        });
        Resp::new(200u16).json(&v).build()
    }

    fn reset(&self) {
        self.resets.fetch_add(1, Ordering::SeqCst);
    }
}
