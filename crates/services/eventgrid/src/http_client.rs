//! The only outbound HTTP in the emulator: calls to the user's own webhooks.
//! HTTP/1.1, one connection per request. `https://` works through rustls (ring) and does
//! not verify the certificate: local webhooks usually have a self-signed one.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Uri;
use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::webhook_policy::WebhookPolicy;

/// What came back from the webhook.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    /// Header names lower-case.
    pub headers: Vec<(String, String)>,
    /// At most the first 64 KiB.
    pub body: Bytes,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

const MAX_REPLY_BODY: usize = 64 * 1024;

/// `Ok` for an absolute http or https URL with a host that `policy` allows (no lookup is made).
pub fn check_url(url: &str, policy: &WebhookPolicy) -> Result<(), String> {
    let t = parse(url)?;
    policy.check_host(&t.host)
}

struct Target {
    https: bool,
    host: String,
    port: u16,
    authority: String,
    path: String,
}

fn parse(url: &str) -> Result<Target, String> {
    let uri: Uri = url.parse().map_err(|_| format!("'{url}' is not a valid URL."))?;
    let https = match uri.scheme_str() {
        Some("http") => false,
        Some("https") => true,
        _ => return Err(format!("The webhook URL '{url}' must start with http:// or https://.")),
    };
    let host = uri
        .host()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("The webhook URL '{url}' has no host."))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    let authority = uri.authority().map(|a| a.to_string()).unwrap_or_default();
    let path = uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    Ok(Target { https, host, port, authority, path })
}

/// Send one request and read the whole answer, all within `timeout`.
pub async fn send(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Bytes,
    timeout: Duration,
    policy: &WebhookPolicy,
) -> Result<Reply, String> {
    match tokio::time::timeout(timeout, send_inner(method, url, headers, body, policy)).await {
        Ok(r) => r,
        Err(_) => Err(format!("timed out after {} ms", timeout.as_millis())),
    }
}

/// The addresses a connection to `host` may use: the host itself passes the webhook rule, then `lookup` (the
/// name lookup; a test gives its own) says where the name points now and public addresses are dropped (see
/// `WebhookPolicy::vet_addresses`). An IP address is not looked up.
async fn checked_addresses<F, Fut>(host: &str, port: u16, policy: &WebhookPolicy, lookup: F) -> Result<Vec<std::net::SocketAddr>, String>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
{
    policy.check_host(host)?;
    let addrs = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => vec![std::net::SocketAddr::new(ip, port)],
        Err(_) => lookup(host.to_string(), port).await.map_err(|e| format!("connect failed: {e}"))?,
    };
    policy.vet_addresses(host, addrs)
}

async fn send_inner(method: &str, url: &str, headers: &[(String, String)], body: Bytes, policy: &WebhookPolicy) -> Result<Reply, String> {
    let t = parse(url)?;
    // The rule is checked again here, on the addresses the name points to now (a name can change what it points to).
    // The one lookup is the one the connection needs anyway, and the connection goes to an address that was checked.
    let addrs = checked_addresses(&t.host, t.port, policy, |h, p| async move {
        tokio::net::lookup_host((h.as_str(), p)).await.map(|a| a.collect::<Vec<_>>())
    })
    .await?;
    let mut last = String::from("no address");
    let mut connected = None;
    for a in addrs {
        match TcpStream::connect(a).await {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(e) => last = e.to_string(),
        }
    }
    let tcp = connected.ok_or_else(|| format!("connect failed: {last}"))?;
    let _ = tcp.set_nodelay(true);
    let mut req = hyper::Request::builder().method(method).uri(&t.path).header("host", &t.authority);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let req = req
        .header("connection", "close")
        .header("content-length", body.len())
        .body(Full::new(body))
        .map_err(|e| format!("bad request: {e}"))?;
    if t.https {
        let name = ServerName::try_from(t.host.clone()).map_err(|e| format!("bad host name: {e}"))?;
        let connector = tokio_rustls::TlsConnector::from(tls_config());
        let tls = connector.connect(name, tcp).await.map_err(|e| format!("TLS handshake failed: {e}"))?;
        exchange(tls, req).await
    } else {
        exchange(tcp, req).await
    }
}

async fn exchange<S>(io: S, req: hyper::Request<Full<Bytes>>) -> Result<Reply, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(TokioIo::new(io)).await.map_err(|e| format!("HTTP handshake failed: {e}"))?;
    let driver = tokio::spawn(async move {
        let _ = conn.await;
    });
    let result = async {
        let resp = sender.send_request(req).await.map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status().as_u16();
        let headers =
            resp.headers().iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect();
        let body = read_limited(resp.into_body(), MAX_REPLY_BODY).await?;
        Ok(Reply { status, headers, body })
    }
    .await;
    driver.abort();
    result
}

/// Read at most `limit` bytes of a reply body and stop: a webhook that answers with a huge body costs at most the
/// limit (the connection is closed afterwards), not the whole body.
async fn read_limited<B>(mut body: B, limit: usize) -> Result<Bytes, String>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let mut out = bytes::BytesMut::new();
    while out.len() < limit {
        match body.frame().await {
            None => break,
            Some(Err(e)) => return Err(format!("reading the reply failed: {e}")),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    let room = limit - out.len();
                    out.extend_from_slice(&data[..data.len().min(room)]);
                }
            }
        }
    }
    Ok(out.freeze())
}

fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let cfg = ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .map(|b| b.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAny(provider))).with_no_client_auth());
            match cfg {
                Ok(c) => Arc::new(c),
                // Cannot happen with the ring provider and default versions.
                Err(e) => panic!("rustls client config: {e}"),
            }
        })
        .clone()
}

#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_name_that_now_points_to_a_public_address_is_not_called() {
        let strict = WebhookPolicy::default();
        let public: std::net::SocketAddr = "93.184.216.34:80".parse().unwrap();
        let local: std::net::SocketAddr = "127.0.0.1:80".parse().unwrap();
        // a single-label name passes the rule at subscription time; what it points to is checked when it is called
        let e = checked_addresses("hook", 80, &strict, |_, _| async move { Ok(vec![public]) }).await.unwrap_err();
        assert!(e.contains("93.184.216.34") && e.contains("--allow-webhook-host"), "{e}");
        let ok = checked_addresses("hook", 80, &strict, |_, _| async move { Ok(vec![public, local]) }).await.unwrap();
        assert_eq!(ok, vec![local]);
        // a host the user named is called whatever it points to; so is anything with `*`
        let named = WebhookPolicy::new(&["hook".into()], &[]);
        assert_eq!(checked_addresses("hook", 80, &named, |_, _| async move { Ok(vec![public]) }).await.unwrap(), vec![public]);
        assert_eq!(
            checked_addresses("hook", 80, &WebhookPolicy::allow_all(), |_, _| async move { Ok(vec![public]) }).await.unwrap(),
            vec![public]
        );
        // an address is not looked up, and a public one is refused before anything is
        assert!(checked_addresses("8.8.8.8", 80, &strict, |_, _| async move { Err(std::io::Error::other("looked up")) }).await.is_err());
        assert_eq!(
            checked_addresses("127.0.0.1", 81, &strict, |_, _| async move { Err(std::io::Error::other("looked up")) }).await.unwrap(),
            vec!["127.0.0.1:81".parse().unwrap()]
        );
    }

    #[test]
    fn url_checks() {
        let p = WebhookPolicy::default();
        assert!(check_url("http://127.0.0.1:9000/hook", &p).is_ok());
        assert!(check_url("https://example.test/a?b=c", &WebhookPolicy::allow_all()).is_ok());
        assert!(check_url("https://example.test/a?b=c", &p).is_err());
        assert!(check_url("ftp://x/", &p).is_err());
        assert!(check_url("/relative", &p).is_err());
        assert!(check_url("not a url", &p).is_err());
    }

    /// A body of `left` frames of 1 KiB that counts how many frames were asked for.
    struct Counting {
        left: usize,
        asked: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl hyper::body::Body for Counting {
        type Data = Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.left == 0 {
                return std::task::Poll::Ready(None);
            }
            self.left -= 1;
            std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(Bytes::from(vec![b'x'; 1024])))))
        }
    }

    #[tokio::test]
    async fn a_reply_is_cut_at_the_limit_without_reading_the_rest() {
        // 1000 frames of 1 KiB; the limit is 10 KiB: only about ten frames are asked for
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let got = read_limited(Counting { left: 1000, asked: asked.clone() }, 10 * 1024).await.unwrap();
        assert_eq!(got.len(), 10 * 1024);
        assert!(asked.load(std::sync::atomic::Ordering::SeqCst) <= 12, "{} frames read", asked.load(std::sync::atomic::Ordering::SeqCst));
        // a short body is returned whole
        let all = read_limited(Counting { left: 3, asked: Arc::new(Default::default()) }, 10 * 1024).await.unwrap();
        assert_eq!(all.len(), 3 * 1024);
    }

    #[test]
    fn parts() {
        let t = parse("http://[::1]:8080/x?y=1").unwrap();
        assert_eq!(t.host, "::1");
        assert_eq!(t.port, 8080);
        assert_eq!(t.path, "/x?y=1");
        assert!(!t.https);
    }
}
