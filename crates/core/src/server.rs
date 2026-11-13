//! Listeners, protocol detection, HTTP handling and the admin API.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use http::{header, HeaderValue, Method, StatusCode};
use http_body_util::{BodyExt, Limited};
use hyper::body::{Body as _, Incoming};
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_rustls::LazyConfigAcceptor;

use crate::access::{self, AccessPolicy, Browser};
use crate::answer::{AbortOnDrop, AnswerBody};
use crate::cancel::{self, CancelGuard, CancelToken};
use crate::persistence::{PersistConfig, Persistence, SaveOutcome, ServiceSave, DEFAULT_SAVE_INTERVAL_SECS};
use crate::request::{Endpoint, Query, Request, RequestCtx};
use crate::resp::Resp;
use crate::routing::{route_gateway, route_pinned, Routed};
use crate::service::{RawConn, RawProtocol, Service};
use crate::tls::{server_config, server_config_no_alpn, Ca, LeafResolver};
use crate::websocket::{self, RawOverWebSocket, WebSocketProtocol, WsRequest, WsStream};
use crate::{json_error, Response};

/// Default request body limit: 1 GiB.
pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024 * 1024;

/// How long to wait for the first bytes of a new connection before treating it as HTTP.
const PEEK_TIMEOUT: Duration = Duration::from_secs(5);
/// With raw protocols registered, how long to wait for the 4 bytes a sniffer needs.
const SNIFF_TIMEOUT: Duration = Duration::from_millis(250);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A client that has not finished sending the head of a request (or, on a kept-alive connection, has not
/// started the next one) for this long is disconnected. Real Azure also closes idle connections after minutes.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(300);
/// A request body that stalls for this long (no data at all) is answered with 408.
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

// HTTP/2 limits (per connection).
/// Requests in progress at the same time on one HTTP/2 connection. More are refused until one ends.
const H2_MAX_CONCURRENT_STREAMS: u32 = 128;
/// Largest header list (names, values and 32 bytes per header, as RFC 9113 counts it) we accept: 128 KiB.
const H2_MAX_HEADER_LIST_SIZE: u32 = 128 * 1024;
/// Receive window per request and per connection. The default 64 KiB makes a large upload wait for a
/// window update after every 64 KiB.
const H2_STREAM_WINDOW: u32 = 2 * 1024 * 1024;
const H2_CONNECTION_WINDOW: u32 = 8 * 1024 * 1024;
const H2_MAX_FRAME_SIZE: u32 = 64 * 1024;
/// Streams the client reset before we accepted them, kept at once; above this the connection is closed
/// with `ENHANCE_YOUR_CALM` (the "rapid reset" attack, CVE-2023-44487).
const H2_MAX_PENDING_ACCEPT_RESET_STREAMS: usize = 20;
/// Streams reset by us because of a protocol error; above this the connection is closed.
const H2_MAX_LOCAL_ERROR_RESET_STREAMS: usize = 256;
/// A PING every 30 s finds a dead peer (no answer in 20 s closes the connection).
const H2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const H2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Entry point: `Emulator::builder()`.
pub struct Emulator;

impl Emulator {
    pub fn builder() -> EmulatorBuilder {
        EmulatorBuilder::default()
    }
}

struct Listen {
    addr: SocketAddr,
    /// `Some(service name)` for a pinned listener.
    pinned: Option<String>,
    /// A failed bind is a warning, not an error.
    optional: bool,
}

#[derive(Default)]
pub struct EmulatorBuilder {
    services: Vec<Arc<dyn Service>>,
    raw: Vec<Arc<dyn RawProtocol>>,
    listeners: Vec<Listen>,
    cert_dir: Option<PathBuf>,
    max_body: Option<usize>,
    ws: Vec<Arc<dyn WebSocketProtocol>>,
    ws_max_frame: Option<u64>,
    tls_names: Vec<String>,
    allow_hosts: Vec<String>,
    allow_origins: Vec<String>,
    header_timeout: Option<Duration>,
    body_timeout: Option<Duration>,
    data_dir: Option<PathBuf>,
    save_interval: Option<Duration>,
    discard_unreadable: bool,
}

impl EmulatorBuilder {
    pub fn service(mut self, s: Arc<dyn Service>) -> Self {
        self.services.push(s);
        self
    }

    pub fn raw_protocol(mut self, p: Arc<dyn RawProtocol>) -> Self {
        self.raw.push(p);
        self
    }

    /// Register a protocol that runs inside a WebSocket (see [`WebSocketProtocol`]). Raw protocols
    /// need no such registration: when no registered protocol wants an upgrade request, core
    /// serves it with the raw protocols.
    pub fn websocket_protocol(mut self, p: Arc<dyn WebSocketProtocol>) -> Self {
        self.ws.push(p);
        self
    }

    /// Limit for the payload of one WebSocket frame from a client (default 16 MiB). A bigger
    /// frame closes the connection with status 1009.
    pub fn websocket_max_frame_bytes(mut self, n: u64) -> Self {
        self.ws_max_frame = Some(n);
        self
    }

    /// Add a gateway listener. Repeatable. Port 0 picks a free port; the gateway listeners that all ask for port 0
    /// share one port number (the first is bound to port 0, the others to the port it got).
    pub fn gateway(mut self, addr: SocketAddr) -> Self {
        self.listeners.push(Listen { addr, pinned: None, optional: false });
        self
    }

    /// Like `gateway`, but a bind failure only logs a warning (used for `::1` on
    /// machines without IPv6). `start` still fails when no gateway at all bound.
    pub fn gateway_optional(mut self, addr: SocketAddr) -> Self {
        self.listeners.push(Listen { addr, pinned: None, optional: true });
        self
    }

    /// Add a listener that sends everything to one service. Repeatable.
    pub fn pinned(mut self, addr: SocketAddr, service: &str) -> Self {
        self.listeners.push(Listen { addr, pinned: Some(service.to_string()), optional: false });
        self
    }

    /// Like `pinned`, but a bind failure only logs a warning.
    pub fn pinned_optional(mut self, addr: SocketAddr, service: &str) -> Self {
        self.listeners.push(Listen { addr, pinned: Some(service.to_string()), optional: true });
        self
    }

    /// Where `ca.pem` / `ca.key` are loaded from or written to.
    /// Default: `$HOME/.azemu`.
    pub fn cert_dir(mut self, dir: PathBuf) -> Self {
        self.cert_dir = Some(dir);
        self
    }

    /// Extra names the local CA may sign for, on top of `localhost`, `*.localhost` and the loopback
    /// addresses: DNS suffixes (`azemu` allows `azemu` and `*.azemu`), IP addresses or ranges
    /// (`10.0.0.0/24`). Repeatable. The CA in the cert dir is replaced when this set differs from
    /// the one it was made for. A bad value makes `start` fail.
    pub fn tls_name(mut self, name: impl Into<String>) -> Self {
        self.tls_names.push(name.into());
        self
    }

    /// Serve requests for this host name (and every name under it), besides the default ones: IP addresses,
    /// `localhost` and `*.localhost`, names with no dot, `host.docker.internal` and `gateway.docker.internal`, and the
    /// suffixes of `tls_name`. Needed when a hosts file maps real Azure names such as `core.windows.net` to the emulator.
    /// Repeatable; a comma separated list is accepted. A public suffix (`com`, `co.uk`) makes `start` fail.
    pub fn allow_host(mut self, name: impl Into<String>) -> Self {
        self.allow_hosts.push(name.into());
        self
    }

    /// Let web pages from this origin (`http://localhost:3000`), host (`app.example.com`, `*.example.com`) use the
    /// services although they are on another site. `*` turns the cross-site check off, admin paths included.
    /// Repeatable. Pages from a host the host check serves (localhost, an IP address, ...) need no option unless
    /// the browser calls the request cross-site (`localhost` calling `127.0.0.1` is).
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        self.allow_origins.push(origin.into());
        self
    }

    /// How long a client may take to send the head of a request, and how long an idle kept-alive
    /// connection stays open (default 5 minutes).
    pub fn header_read_timeout(mut self, d: Duration) -> Self {
        self.header_timeout = Some(d);
        self
    }

    /// How long a request body may stall, with no data arriving, before the request gets 408
    /// (default 60 seconds).
    pub fn body_idle_timeout(mut self, d: Duration) -> Self {
        self.body_timeout = Some(d);
        self
    }

    /// Request body limit in bytes (default 1 GiB). Larger bodies get 413.
    pub fn max_body_bytes(mut self, n: usize) -> Self {
        self.max_body = Some(n);
        self
    }

    /// Keep state between runs: every service that supports it reads its state from `<dir>/<service>/` at start
    /// and writes it back while the emulator runs and when it stops. The directory is created when missing and
    /// locked while the emulator runs. `start` fails when it cannot be written, is in use by another emulator, or
    /// holds state that cannot be read (see [`discard_unreadable_data`](Self::discard_unreadable_data)).
    /// Without this call nothing is written and nothing changes.
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// How often changed state is written to the data directory (default 15 seconds). `Duration::ZERO`: only on
    /// request ([`RunningEmulator::save_now`], `POST /_azemu/save`) and at shutdown.
    pub fn save_interval(mut self, d: Duration) -> Self {
        self.save_interval = Some(d);
        self
    }

    /// When a service's saved state cannot be read, move its directory aside (`<service>.unreadable-<time>`) and
    /// start that service empty, instead of failing to start.
    pub fn discard_unreadable_data(mut self, yes: bool) -> Self {
        self.discard_unreadable = yes;
        self
    }

    pub async fn start(self) -> Result<RunningEmulator> {
        if self.listeners.is_empty() {
            bail!("no listeners configured (call gateway() or pinned())");
        }
        let mut names = std::collections::HashSet::new();
        for s in &self.services {
            if !names.insert(s.name()) {
                bail!("two services are named '{}'", s.name());
            }
        }
        for l in &self.listeners {
            if let Some(p) = &l.pinned {
                if !self.services.iter().any(|s| s.name() == p) {
                    bail!("cannot pin '{p}': no such service (mounted: {})", service_names(&self.services));
                }
            }
        }

        let cert_dir = self.cert_dir.clone().unwrap_or_else(default_cert_dir);
        let names = crate::tls::TlsNames::parse(&self.tls_names)?;
        // Saved state is read before anything listens. The lock on the directory is released when `start`
        // fails below, because `persist` is dropped with it.
        let persist = match self.data_dir.clone() {
            Some(dir) => {
                let cfg = PersistConfig {
                    dir,
                    interval: self.save_interval.unwrap_or(Duration::from_secs(DEFAULT_SAVE_INTERVAL_SECS)),
                    discard_unreadable: self.discard_unreadable,
                };
                Some(Persistence::open(cfg, &self.services).await?)
            }
            None => None,
        };
        let access = AccessPolicy::new(&self.allow_hosts, names.dns_suffixes(), &self.allow_origins)?;
        // read once here, so the first request does not decide when `AZEMU_LOOSE_AUTH` is looked at
        let _ = crate::storage::sig::loose_auth();
        let config = ConfigSummary {
            tls_names: self.tls_names.clone(),
            allow_hosts: access.suffixes().to_vec(),
            allow_origins: self
                .allow_origins
                .iter()
                .flat_map(|s| s.split(','))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            save_interval: self.save_interval,
        };
        let ca = Ca::load_or_create_with(&cert_dir, &names)?;
        let resolver = Arc::new(LeafResolver::new(ca)?);
        let tls_http = Arc::new(server_config(resolver.clone())?);
        let tls_other = Arc::new(server_config_no_alpn(resolver.clone())?);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (drain_tx, drain_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            persist: persist.clone(),
            draining: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
            services: self.services,
            registry: Arc::new(crate::unsupported::Registry::default()),
            refusals: crate::refusals::Refusals::default(),
            listeners: std::sync::OnceLock::new(),
            config,
            raw_ws: Arc::new(RawOverWebSocket { raw: self.raw.clone() }),
            raw: self.raw,
            ws: self.ws,
            ws_max_frame: self.ws_max_frame.unwrap_or(websocket::DEFAULT_MAX_FRAME_BYTES),
            tls_http,
            tls_other,
            resolver,
            access,
            max_body: self.max_body.unwrap_or(DEFAULT_MAX_BODY_BYTES),
            header_timeout: self.header_timeout.unwrap_or(HEADER_READ_TIMEOUT),
            body_timeout: self.body_timeout.unwrap_or(BODY_IDLE_TIMEOUT),
            shutdown: shutdown_rx.clone(),
        });

        let mut gateway_addrs = Vec::new();
        let mut pinned_addrs: Vec<(String, SocketAddr)> = Vec::new();
        let mut tasks = Vec::new();
        for (l, listener) in bind_listeners(self.listeners, TcpListener::bind).await? {
            let bound = listener.local_addr()?;
            let kind = match &l.pinned {
                None => {
                    gateway_addrs.push(bound);
                    Kind::Gateway
                }
                Some(name) => {
                    pinned_addrs.push((name.clone(), bound));
                    let svc = shared.services.iter().find(|s| s.name() == name).cloned().context("pinned service vanished")?;
                    Kind::Pinned(svc)
                }
            };
            tracing::info!(addr = %bound, kind = ?kind.label(), "listening");
            tasks.push(tokio::spawn(accept_loop(listener, kind, shared.clone(), drain_rx.clone())));
        }
        if gateway_addrs.is_empty() && pinned_addrs.is_empty() {
            bail!("no listener could be bound");
        }
        let _ = shared
            .listeners
            .set(gateway_addrs.iter().map(|a| (None, *a)).chain(pinned_addrs.iter().map(|(n, a)| (Some(n.clone()), *a))).collect());

        let saver = persist.as_ref().and_then(|p| p.spawn_background(shutdown_rx.clone()));
        Ok(RunningEmulator { gateway_addrs, pinned_addrs, shared, shutdown_tx, drain_tx, tasks, saver, finished: false })
    }
}

/// How many times a group of listeners that asked for port 0 looks for a port that every address accepts.
const PORT_ZERO_ATTEMPTS: usize = 8;

/// Open every listener, in the order given.
///
/// Port 0 means "any free port", and the system picks one per socket. A client that resolves `localhost` to `::1`
/// while the emulator announced the port of `127.0.0.1` would be refused. So the listeners of one kind (the
/// gateway, or the pins of one service) that ask for port 0 get one number between them: the first address is bound
/// to port 0, and the others to the port it received. When another address refuses that port because something else
/// uses it, the whole group starts again with a new port.
async fn bind_listeners<F, Fut>(listeners: Vec<Listen>, bind: F) -> Result<Vec<(Listen, TcpListener)>>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<TcpListener>>,
{
    let mut slots: Vec<Option<TcpListener>> = Vec::new();
    slots.resize_with(listeners.len(), || None);
    let mut groups: Vec<(Option<&str>, Vec<usize>)> = Vec::new();
    for (i, l) in listeners.iter().enumerate() {
        if l.addr.port() != 0 {
            slots[i] = bind_one(l, &bind).await?;
            continue;
        }
        let key = l.pinned.as_deref();
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(i),
            None => groups.push((key, vec![i])),
        }
    }
    for (_, members) in groups {
        'attempt: for attempt in 0..PORT_ZERO_ATTEMPTS {
            let last = attempt + 1 == PORT_ZERO_ATTEMPTS;
            let mut port = 0u16;
            let mut got: Vec<(usize, TcpListener)> = Vec::new();
            for &i in &members {
                let l = &listeners[i];
                let addr = SocketAddr::new(l.addr.ip(), port);
                match bind(addr).await {
                    Ok(x) => {
                        if port == 0 {
                            port = x.local_addr()?.port();
                        }
                        got.push((i, x));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && port != 0 && !last => continue 'attempt,
                    Err(e) if l.optional => tracing::warn!(addr = %addr, error = %e, "cannot bind, skipping this address"),
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!("binding {addr} (port {port} was chosen for the first address, and this address could not use it)")
                        })
                    }
                }
            }
            for (i, x) in got {
                slots[i] = Some(x);
            }
            break;
        }
    }
    Ok(listeners.into_iter().zip(slots).filter_map(|(l, x)| x.map(|x| (l, x))).collect())
}

/// Bind one listener whose port is fixed. `None` when it is optional and could not be bound.
async fn bind_one<F, Fut>(l: &Listen, bind: &F) -> Result<Option<TcpListener>>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<TcpListener>>,
{
    match bind(l.addr).await {
        Ok(x) => Ok(Some(x)),
        Err(e) if l.optional => {
            tracing::warn!(addr = %l.addr, error = %e, "cannot bind, skipping this address");
            Ok(None)
        }
        Err(e) => Err(e).with_context(|| format!("binding {}", l.addr)),
    }
}

fn service_names(services: &[Arc<dyn Service>]) -> String {
    services.iter().map(|s| s.name()).collect::<Vec<_>>().join(",")
}

fn default_cert_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h).join(".azemu"),
        _ => PathBuf::from(".azemu"),
    }
}

/// A started emulator. Dropping it also stops the listeners.
pub struct RunningEmulator {
    gateway_addrs: Vec<SocketAddr>,
    pinned_addrs: Vec<(String, SocketAddr)>,
    shared: Arc<Shared>,
    shutdown_tx: watch::Sender<bool>,
    /// Turns true first: listeners stop accepting and new requests are refused while the old ones finish.
    drain_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    saver: Option<JoinHandle<()>>,
    /// `shutdown` ran to the end (the lock on the data directory is released).
    finished: bool,
}

/// How long `shutdown` waits for requests in progress before it cuts connections.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long it then waits for the connection tasks to end before the final save.
const CONNECTIONS_END_TIMEOUT: Duration = Duration::from_secs(2);

impl RunningEmulator {
    /// Bound gateway addresses (the real port when 0 was requested).
    pub fn gateway_addrs(&self) -> &[SocketAddr] {
        &self.gateway_addrs
    }

    /// First bound address of the listener pinned to `service`.
    pub fn pinned_addr(&self, service: &str) -> Option<SocketAddr> {
        self.pinned_addrs.iter().find(|(n, _)| n == service).map(|(_, a)| *a)
    }

    /// Every bound address pinned to `service` (one per host).
    pub fn pinned_addrs(&self, service: &str) -> Vec<SocketAddr> {
        self.pinned_addrs.iter().filter(|(n, _)| n == service).map(|(_, a)| *a).collect()
    }

    /// The CA certificate in PEM form.
    pub fn ca_pem(&self) -> &str {
        self.shared.resolver.ca_pem()
    }

    /// Where `ca.pem` was written.
    pub fn ca_path(&self) -> &std::path::Path {
        self.shared.resolver.ca_path()
    }

    /// Names of the mounted services, in the order they were added.
    pub fn service_names(&self) -> Vec<&'static str> {
        self.shared.services.iter().map(|s| s.name()).collect()
    }

    /// Call `reset()` on every service (same as `POST /_azemu/reset`).
    pub fn reset(&self) {
        self.shared.reset_all();
    }

    /// The data directory, when the emulator was started with one.
    pub fn data_dir(&self) -> Option<&std::path::Path> {
        self.shared.persist.as_ref().map(|p| p.root())
    }

    /// Save, now, every service whose state changed since its last save, and return when it is on disk
    /// (same as `POST /_azemu/save`). Empty when there is no data directory.
    pub async fn save_now(&self) -> Vec<ServiceSave> {
        match &self.shared.persist {
            Some(p) => p.save_all(false).await,
            None => Vec::new(),
        }
    }

    /// Stop accepting, let the requests in progress finish (up to a few seconds), close the connections,
    /// then write the state that changed to the data directory, if there is one. Errors of that last save
    /// are logged; use [`shutdown_checked`](Self::shutdown_checked) to get them.
    pub async fn shutdown(self) {
        if let Err(e) = self.shutdown_checked().await {
            tracing::error!(error = %e, "final save failed");
        }
    }

    /// Like [`shutdown`](Self::shutdown), but returns an error when the final save did not succeed.
    pub async fn shutdown_checked(mut self) -> Result<()> {
        let _ = self.drain_tx.send(true);
        self.shared.draining.store(true, Ordering::SeqCst);
        for t in std::mem::take(&mut self.tasks) {
            let _ = t.await;
        }
        wait_for_zero(&self.shared.in_flight, DRAIN_TIMEOUT).await;
        let _ = self.shutdown_tx.send(true);
        wait_for_zero(&self.shared.connections, CONNECTIONS_END_TIMEOUT).await;
        if let Some(t) = self.saver.take() {
            let _ = t.await;
        }
        let fallback = crate::unsupported::fallback();
        for line in self.shared.registry.summary_lines(Some(fallback)) {
            tracing::warn!("not emulated during this run: {line}");
        }
        let mut result = Ok(());
        if let Some(p) = &self.shared.persist {
            let failed: Vec<String> = p
                .save_all(false)
                .await
                .into_iter()
                .filter_map(|r| match r.outcome {
                    SaveOutcome::Failed(e) => Some(format!("{}: {e}", r.service)),
                    _ => None,
                })
                .collect();
            if !failed.is_empty() {
                result = Err(anyhow!("state could not be saved: {}", failed.join("; ")));
            }
            p.release_lock();
        }
        self.finished = true;
        result
    }
}

async fn wait_for_zero(counter: &AtomicUsize, limit: Duration) {
    let deadline = Instant::now() + limit;
    while counter.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

impl Drop for RunningEmulator {
    /// Dropping without `shutdown` stops the listeners and releases the data directory but does not save: it
    /// behaves like a crash, and what changed since the last save is lost.
    fn drop(&mut self) {
        let _ = self.drain_tx.send(true);
        let _ = self.shutdown_tx.send(true);
        if !self.finished {
            if let Some(p) = &self.shared.persist {
                p.release_lock();
            }
        }
    }
}

/// What `GET /_azemu/config` shows besides the settings.
struct ConfigSummary {
    tls_names: Vec<String>,
    allow_hosts: Vec<String>,
    allow_origins: Vec<String>,
    save_interval: Option<Duration>,
}

struct Shared {
    /// 501 answers and stored-only resource types (`GET /_azemu/unsupported`).
    registry: Arc<crate::unsupported::Registry>,
    /// Requests refused for lack of a valid credential, logged once per service and reason in a few seconds.
    refusals: crate::refusals::Refusals,
    /// Bound listeners: `None` is a gateway, `Some(service)` a pinned port. Set once the sockets are open.
    listeners: std::sync::OnceLock<Vec<(Option<String>, SocketAddr)>>,
    config: ConfigSummary,
    /// The data directory driver, when `--data-dir` is set.
    persist: Option<Arc<Persistence>>,
    /// Set when shutdown begins: new requests are refused with 503.
    draining: AtomicBool,
    /// HTTP requests being handled, and connection tasks alive: what `shutdown` waits for.
    in_flight: AtomicUsize,
    connections: AtomicUsize,
    services: Vec<Arc<dyn Service>>,
    raw: Vec<Arc<dyn RawProtocol>>,
    ws: Vec<Arc<dyn WebSocketProtocol>>,
    /// Raw protocols served inside a WebSocket: the fallback for an upgrade nobody registered for.
    raw_ws: Arc<RawOverWebSocket>,
    ws_max_frame: u64,
    /// TLS config for a hello that offers `h2` or `http/1.1` (ALPN answered), and for any other hello (no ALPN).
    tls_http: Arc<rustls::ServerConfig>,
    tls_other: Arc<rustls::ServerConfig>,
    resolver: Arc<LeafResolver>,
    /// Host check and cross-site check (`access.rs`).
    access: AccessPolicy,
    max_body: usize,
    header_timeout: Duration,
    body_timeout: Duration,
    shutdown: watch::Receiver<bool>,
}

/// What an HTTP request handler needs to know about the connection.
struct ConnMeta {
    peer: SocketAddr,
    local: SocketAddr,
    scheme: &'static str,
    /// TLS server name, if any.
    sni: Option<String>,
}

impl Shared {
    fn reset_all(&self) {
        for s in &self.services {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.reset()));
            if r.is_err() {
                tracing::error!(service = s.name(), "reset() panicked");
            }
        }
    }
}

enum Kind {
    Gateway,
    Pinned(Arc<dyn Service>),
}

impl Kind {
    fn label(&self) -> String {
        match self {
            Kind::Gateway => "gateway".into(),
            Kind::Pinned(s) => format!("pinned:{}", s.name()),
        }
    }
}

/// Resolves when shutdown was requested (or the emulator handle is gone).
async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}

/// Counts a live connection task or request while it exists.
struct CountGuard<'a>(&'a AtomicUsize);

impl<'a> CountGuard<'a> {
    fn new(c: &'a AtomicUsize) -> Self {
        c.fetch_add(1, Ordering::SeqCst);
        CountGuard(c)
    }
}

impl Drop for CountGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn accept_loop(listener: TcpListener, kind: Kind, shared: Arc<Shared>, mut drain: watch::Receiver<bool>) {
    let kind = Arc::new(kind);
    loop {
        tokio::select! {
            _ = wait_shutdown(&mut drain) => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let shared = shared.clone();
                    let kind = kind.clone();
                    let mut sd = shared.shutdown.clone();
                    shared.connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        let _alive = ConnectionAlive(shared.clone());
                        tokio::select! {
                            _ = wait_shutdown(&mut sd) => {}
                            _ = serve_connection(stream, peer, shared, kind) => {}
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
}

/// Counts a connection task; the count is `Shared::connections`.
struct ConnectionAlive(Arc<Shared>);

impl Drop for ConnectionAlive {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

enum Proto {
    Tls,
    Raw(Arc<dyn RawProtocol>),
    Http,
    /// HTTP/2 with prior knowledge: the connection starts with the HTTP/2 preface (`PRI * HTTP/2.0`).
    Http2,
}

/// The first four bytes of the HTTP/2 connection preface. No HTTP/1 method is `PRI`.
const H2_PREFACE_START: &[u8] = b"PRI ";

/// Look at the first bytes without consuming them.
async fn detect(stream: &TcpStream, raw: &[Arc<dyn RawProtocol>]) -> Proto {
    let mut buf = [0u8; 16];
    let first = match tokio::time::timeout(PEEK_TIMEOUT, stream.peek(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => n,
        // EOF, error or idle: let HTTP deal with it (it ends or waits).
        _ => return Proto::Http,
    };
    let mut n = first;
    let deadline = Instant::now() + SNIFF_TIMEOUT;
    loop {
        if buf[0] == 0x16 {
            return Proto::Tls;
        }
        if let Some(p) = raw.iter().find(|p| p.sniff(&buf[..n])) {
            return Proto::Raw(p.clone());
        }
        if n >= 4 && &buf[..4] == H2_PREFACE_START {
            return Proto::Http2;
        }
        // fewer than 4 bytes that could still become "PRI " (or a raw protocol's start): wait a moment
        let maybe_h2 = n < 4 && H2_PREFACE_START.starts_with(&buf[..n]);
        if (raw.is_empty() && !maybe_h2) || n >= 4 || Instant::now() >= deadline {
            return Proto::Http;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        match stream.peek(&mut buf).await {
            Ok(m) if m > 0 => n = m,
            _ => return Proto::Http,
        }
    }
}

async fn serve_connection(stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>, kind: Arc<Kind>) {
    let _ = stream.set_nodelay(true);
    let local = match stream.local_addr() {
        Ok(a) => a,
        Err(_) => return,
    };
    match detect(&stream, &shared.raw).await {
        Proto::Raw(p) => p.serve(RawConn { io: Box::new(stream), peer, local, tls: false, server_name: None }).await,
        Proto::Http => {
            let meta = ConnMeta { peer, local, scheme: "http", sni: None };
            serve_http(TokioIo::new(stream), meta, shared, kind, HttpMode::Http1).await
        }
        Proto::Http2 => {
            let meta = ConnMeta { peer, local, scheme: "http", sni: None };
            serve_http(TokioIo::new(stream), meta, shared, kind, HttpMode::Http2).await
        }
        Proto::Tls => {
            let accept = async {
                let start = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream).await?;
                let offers_http = start.client_hello().alpn().is_some_and(|mut protos| protos.any(|p| p == b"h2" || p == b"http/1.1"));
                let cfg = if offers_http { shared.tls_http.clone() } else { shared.tls_other.clone() };
                start.into_stream(cfg).await
            };
            let tls = match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, accept).await {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => {
                    tracing::debug!(%peer, error = %e, "TLS handshake failed");
                    return;
                }
                Err(_) => {
                    tracing::debug!(%peer, "TLS handshake timed out");
                    return;
                }
            };
            let (server_name, alpn) = {
                let conn = tls.get_ref().1;
                (conn.server_name().map(str::to_string), conn.alpn_protocol().map(<[u8]>::to_vec))
            };
            let meta = ConnMeta { peer, local, scheme: "https", sni: server_name.clone() };
            // ALPN decided: `h2` is HTTP/2. `http/1.1` is HTTP/1.1, unless a raw protocol could still start.
            if alpn.as_deref() == Some(b"h2") {
                return serve_http(TokioIo::new(tls), meta, shared, kind, HttpMode::Http2).await;
            }
            if alpn.is_some() && shared.raw.is_empty() {
                return serve_http(TokioIo::new(tls), meta, shared, kind, HttpMode::Http1).await;
            }
            // No ALPN (AMQP in TLS sends none; so do plain HTTP/1.1 clients): the decrypted stream
            // cannot be peeked, so read the first bytes and hand them back in front. They show a raw
            // protocol, or HTTP/2 with prior knowledge, or HTTP/1.1.
            let (prefix, tls) = read_prefix(tls).await;
            let raw = shared.raw.iter().find(|p| !prefix.is_empty() && p.sniff(&prefix)).cloned();
            let mode = if prefix.starts_with(H2_PREFACE_START) { HttpMode::Http2 } else { HttpMode::Http1 };
            let io = Prefixed::new(Bytes::from(prefix), tls);
            match raw {
                Some(p) => p.serve(RawConn { io: Box::new(io), peer, local, tls: true, server_name }).await,
                None => serve_http(TokioIo::new(io), meta, shared, kind, mode).await,
            }
        }
    }
}

/// Read the first bytes (at least 4 if the peer sends them soon enough, at most 16).
pub(crate) async fn read_prefix<S: tokio::io::AsyncRead + Unpin>(mut s: S) -> (Vec<u8>, S) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 16];
    let mut n = match tokio::time::timeout(PEEK_TIMEOUT, s.read(&mut buf)).await {
        Ok(Ok(n)) => n,
        _ => return (Vec::new(), s),
    };
    let deadline = tokio::time::Instant::now() + SNIFF_TIMEOUT;
    while n > 0 && n < 4 {
        match tokio::time::timeout_at(deadline, s.read(&mut buf[n..])).await {
            Ok(Ok(m)) if m > 0 => n += m,
            _ => break,
        }
    }
    (buf[..n].to_vec(), s)
}

/// A stream that yields `prefix` first and then reads from `inner`.
pub(crate) struct Prefixed<S> {
    prefix: Bytes,
    inner: S,
}

impl<S> Prefixed<S> {
    pub(crate) fn new(prefix: Bytes, inner: S) -> Prefixed<S> {
        Prefixed { prefix, inner }
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            let chunk = self.prefix.split_to(n);
            buf.put_slice(&chunk);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HttpMode {
    Http1,
    Http2,
}

/// Requests in progress on one connection and when the last one ended: an HTTP/2 connection that stays
/// idle as long as `header_read_timeout` is closed, like an idle HTTP/1.1 keep-alive connection.
struct Activity {
    in_flight: std::sync::atomic::AtomicUsize,
    last: parking_lot::Mutex<Instant>,
}

struct ActivityGuard(Arc<Activity>);

impl Activity {
    fn enter(self: &Arc<Self>) -> ActivityGuard {
        self.in_flight.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        ActivityGuard(self.clone())
    }

    fn idle_for(&self) -> Option<Duration> {
        if self.in_flight.load(std::sync::atomic::Ordering::Acquire) > 0 {
            return None;
        }
        Some(self.last.lock().elapsed())
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        *self.0.last.lock() = Instant::now();
        self.0.in_flight.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

async fn serve_http<I>(io: TokioIo<I>, meta: ConnMeta, shared: Arc<Shared>, kind: Arc<Kind>, mode: HttpMode)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Whoever ends the connection while the peer may still be sending (an early answer such as 413 or
    // 431, an idle timeout) goes through `Linger`, so the answer is not lost to a reset.
    let io = TokioIo::new(Linger::new(io.into_inner()));
    let meta = Arc::new(meta);
    let header_timeout = shared.header_timeout;
    let activity = Arc::new(Activity { in_flight: Default::default(), last: parking_lot::Mutex::new(Instant::now()) });
    let act = activity.clone();
    // Fired by a streamed answer whose client stopped reading (see `AnswerBody`): the connection is closed.
    let kill = Arc::new(tokio::sync::Notify::new());
    let kill_conn = kill.clone();
    let svc = service_fn(move |req: hyper::Request<Incoming>| {
        let shared = shared.clone();
        let kind = kind.clone();
        let meta = meta.clone();
        let guard = act.enter();
        let kill = kill.clone();
        async move { Ok::<_, Infallible>(handle_request(req, &meta, &shared, &kind, guard, kill).await) }
    });
    match mode {
        HttpMode::Http1 => {
            // with_upgrades: a WebSocket upgrade takes the connection over after the 101
            let conn = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(header_timeout)
                .keep_alive(true)
                .serve_connection(io, svc)
                .with_upgrades();
            let conn = std::pin::pin!(conn);
            tokio::select! {
                r = conn => {
                    if let Err(e) = r {
                        tracing::debug!(error = %e, "connection ended with error");
                    }
                }
                _ = kill_conn.notified() => tracing::debug!("connection closed: the client stopped reading an answer"),
            }
        }
        HttpMode::Http2 => {
            let mut builder = http2::Builder::new(TokioExecutor::new());
            builder
                .timer(TokioTimer::new())
                .max_concurrent_streams(H2_MAX_CONCURRENT_STREAMS)
                .max_header_list_size(H2_MAX_HEADER_LIST_SIZE)
                .initial_stream_window_size(H2_STREAM_WINDOW)
                .initial_connection_window_size(H2_CONNECTION_WINDOW)
                .max_frame_size(H2_MAX_FRAME_SIZE)
                .max_pending_accept_reset_streams(H2_MAX_PENDING_ACCEPT_RESET_STREAMS)
                .max_local_error_reset_streams(H2_MAX_LOCAL_ERROR_RESET_STREAMS)
                .keep_alive_interval(H2_KEEP_ALIVE_INTERVAL)
                .keep_alive_timeout(H2_KEEP_ALIVE_TIMEOUT);
            let mut conn = std::pin::pin!(builder.serve_connection(io, svc));
            let mut tick = tokio::time::interval((header_timeout / 4).clamp(Duration::from_millis(10), Duration::from_secs(5)));
            let mut closing = false;
            loop {
                tokio::select! {
                    r = conn.as_mut() => {
                        if let Err(e) = r {
                            tracing::debug!(error = %e, "HTTP/2 connection ended with error");
                        }
                        break;
                    }
                    _ = kill_conn.notified() => {
                        tracing::debug!("connection closed: the client stopped reading an answer");
                        break;
                    }
                    _ = tick.tick(), if !closing => {
                        if activity.idle_for().is_some_and(|d| d >= header_timeout) {
                            closing = true;
                            conn.as_mut().graceful_shutdown();
                        }
                    }
                }
            }
        }
    }
}

/// The host the client asked for. HTTP/1.1: the `Host` header (else the authority of an absolute-form
/// target). HTTP/2 has no `Host`: the `:authority` pseudo-header, which hyper puts in the URI, comes first
/// (RFC 9113 8.3.1: it takes precedence over a `Host` header when a client sends both).
fn authority_of(version: http::Version, headers: &http::HeaderMap, uri: &http::Uri, local: SocketAddr) -> String {
    let host = || headers.get(header::HOST).and_then(|h| h.to_str().ok()).filter(|h| !h.trim().is_empty()).map(|h| h.trim().to_string());
    let uri_authority = || uri.authority().map(|a| a.to_string());
    let found = if version == http::Version::HTTP_2 { uri_authority().or_else(host) } else { host().or_else(uri_authority) };
    found.unwrap_or_else(|| local.to_string())
}

/// The other places a host can be named in the request, when they differ from `authority`: the `Host` header and the
/// authority of the URI. The host check looks at all of them, so a request cannot name one host for routing and another
/// for the check.
fn other_authorities(headers: &http::HeaderMap, uri: &http::Uri, authority: &str) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(h) = headers.get(header::HOST).and_then(|h| h.to_str().ok()).map(str::trim).filter(|h| !h.is_empty()) {
        if h != authority {
            v.push(h.to_string());
        }
    }
    if let Some(a) = uri.authority().map(|a| a.to_string()) {
        if a != authority {
            v.push(a);
        }
    }
    v
}

async fn handle_request(
    mut req: hyper::Request<Incoming>,
    meta: &ConnMeta,
    shared: &Shared,
    kind: &Kind,
    activity: ActivityGuard,
    kill: Arc<tokio::sync::Notify>,
) -> http::Response<AnswerBody> {
    let (scheme, local) = (meta.scheme, meta.local);
    let started = Instant::now();
    let _counted = CountGuard::new(&shared.in_flight);
    if shared.draining.load(Ordering::SeqCst) {
        let mut r = json_error::response(StatusCode::SERVICE_UNAVAILABLE, "ServerBusy", "azemu is shutting down.");
        r.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
        return finalize(r, false, time::OffsetDateTime::now_utc(), false);
    }
    // The host check comes first, for every request (WebSocket upgrades included); see `access.rs`.
    let authority = authority_of(req.version(), req.headers(), req.uri(), local);
    let others = other_authorities(req.headers(), req.uri(), &authority);
    let other_refs: Vec<&str> = others.iter().map(String::as_str).collect();
    let host_refusal = shared.access.check_host(&authority, &other_refs);
    let browser = shared.access.classify(req.headers());
    if host_refusal.is_none() {
        if let Some(resp) = try_websocket(&mut req, meta, shared, &browser) {
            return finalize(resp, false, time::OffsetDateTime::now_utc(), false);
        }
    }
    let (mut parts, mut body) = req.into_parts();
    let is_head = parts.method == Method::HEAD;
    let raw_path = parts.uri.path().to_string();
    let raw_query = parts.uri.query().unwrap_or("").to_string();
    if parts.version == http::Version::HTTP_2 {
        // Services see the same request whatever the protocol: `Host` is the authority.
        if let Ok(v) = HeaderValue::from_str(&authority) {
            parts.headers.insert(header::HOST, v);
        }
    }
    let now = time::OffsetDateTime::now_utc();

    let mut cancel_guard: Option<CancelGuard> = None;
    let (service_name, account, mut response) = dispatch(
        &parts,
        &mut body,
        &raw_path,
        &raw_query,
        &authority,
        scheme,
        local,
        shared,
        kind,
        now,
        host_refusal,
        &browser,
        &mut cancel_guard,
    )
    .await;

    let status = response.status();
    let ms = started.elapsed().as_millis() as u64;
    tracing::info!(
        service = %service_name,
        account = %account,
        method = %parts.method,
        path = %raw_path,
        status = status.as_u16(),
        ms,
        "request"
    );
    tracing::debug!(query = %crate::redact::query(&raw_query), headers = %crate::redact::headers(&parts.headers), "request detail");
    if status == StatusCode::NOT_IMPLEMENTED && service_name != "core" {
        let operation = crate::unsupported::marked(&response).unwrap_or("(unnamed)").to_string();
        shared.registry.count_unsupported(&service_name, &operation);
        tracing::debug!(service = %service_name, operation = %operation, "answered 501: not built");
    }
    // Answered before the request body was read to the end (refused by size, unknown route, ...): over
    // HTTP/1.1 the connection ends after this answer, so say so (Linger then reads what the client
    // still sends, for a short time, so the answer is not lost to a reset). HTTP/2 forbids the header; the
    // stream is reset after the answer (RST_STREAM NO_ERROR, done by h2) and the connection goes on.
    if parts.version == http::Version::HTTP_11 && !body.is_end_stream() {
        response.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    // A streamed answer is read by hyper from here on; its cancellation guard, the connection's activity count and
    // the watch on a client that stops reading go with it. Without a body to send the stream is dropped unread.
    let stream = crate::stream::take(&mut response).filter(|_| !is_head && can_have_body(response.status()));
    let streamed = stream.is_some();
    let mut out = finalize(response, is_head, now, streamed);
    match (stream, cancel_guard) {
        (Some(src), guard) => {
            *out.body_mut() = AnswerBody::stream(src, guard, activity, shared.body_timeout, kill);
        }
        (None, Some(mut guard)) => guard.disarm(),
        (None, None) => {}
    }
    out
}

/// Answers a WebSocket upgrade request when some handler wants it. `None`: not an upgrade
/// request, or nobody wants it (the request goes on as ordinary HTTP).
fn try_websocket(req: &mut hyper::Request<Incoming>, meta: &ConnMeta, shared: &Shared, browser: &Browser) -> Option<Response> {
    let outcome = websocket::inspect(req.method(), req.version(), req.headers())?;
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(|h| h.trim().to_string()).unwrap_or_default();
    let server_name = meta.sni.clone().or_else(|| {
        let h = host.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map_or(host.as_str(), |(h, _)| h);
        (!h.is_empty()).then(|| h.to_string())
    });
    let wsreq = WsRequest {
        path: req.uri().path().to_string(),
        query: req.uri().query().unwrap_or("").to_string(),
        host,
        headers: req.headers().clone(),
        subprotocols: websocket::offered_subprotocols(req.headers()),
        tls: meta.scheme == "https",
        peer: meta.peer,
        local: meta.local,
        server_name,
    };
    // a registered protocol first (`Some(p)`), then the raw protocols (`None`)
    let (registered, accept) = shared
        .ws
        .iter()
        .find_map(|p| p.accept(&wsreq).map(|a| (Some(p.clone()), a)))
        .or_else(|| shared.raw_ws.accept(&wsreq).map(|a| (None, a)))?;
    // Browsers apply no CORS rules to a WebSocket and send `Origin`: this is the only defence against a page on
    // another site talking AMQP to the emulator.
    if browser.cross_site && !browser.permitted {
        tracing::info!(service = "core", path = %wsreq.path, origin = browser.origin.as_deref().unwrap_or(""), status = 403, "websocket upgrade refused: cross-site request");
        return Some(access::cross_site_refusal(browser, access::Target::WebSocket));
    }
    let offer = match outcome {
        Ok(o) => o,
        Err(r) => {
            let mut b = Resp::new(r.status).json(&serde_json::json!({
                "error": {"code": "BadRequest", "message": r.message}
            }));
            if r.status == StatusCode::UPGRADE_REQUIRED {
                b = b.header("Sec-WebSocket-Version", "13");
            }
            return Some(b.build());
        }
    };
    if let Some(sp) = &accept.subprotocol {
        if !wsreq.subprotocols.iter().any(|o| o == sp) {
            tracing::error!(subprotocol = %sp, "a WebSocket handler chose a subprotocol the client did not offer");
            return Some(json_error::response(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", "The WebSocket handler failed."));
        }
    }
    let on_upgrade = hyper::upgrade::on(&mut *req);
    let chosen = accept.subprotocol.clone();
    let mut resp = Resp::new(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header("Sec-WebSocket-Accept", websocket::accept_key(&offer.key));
    if let Some(sp) = &chosen {
        resp = resp.header("Sec-WebSocket-Protocol", sp);
    }
    tracing::info!(
        service = "core",
        method = "GET",
        path = %wsreq.path,
        status = 101,
        subprotocol = chosen.as_deref().unwrap_or(""),
        tls = wsreq.tls,
        "websocket upgrade"
    );

    // Everything below needs owned values: the connection task outlives this request.
    let max_frame = shared.ws_max_frame;
    let mut sd = shared.shutdown.clone();
    let raw_ws = shared.raw_ws.clone();
    tokio::spawn(async move {
        let upgraded = match on_upgrade.await {
            Ok(u) => u,
            Err(e) => {
                tracing::debug!(error = %e, "WebSocket upgrade did not complete");
                return;
            }
        };
        let ws = WsStream::new(TokioIo::new(upgraded), max_frame);
        let serve = async move {
            match registered {
                Some(p) => p.serve(websocket::WsConn { io: Box::new(ws), request: wsreq, subprotocol: chosen }).await,
                None => raw_ws.serve(ws, wsreq).await,
            }
        };
        tokio::select! {
            _ = wait_shutdown(&mut sd) => {}
            _ = serve => {}
        }
    });
    Some(resp.build())
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    parts: &http::request::Parts,
    body: &mut Incoming,
    raw_path: &str,
    raw_query: &str,
    authority: &str,
    scheme: &'static str,
    local: SocketAddr,
    shared: &Shared,
    kind: &Kind,
    now: time::OffsetDateTime,
    host_refusal: Option<Response>,
    browser: &Browser,
    cancel_out: &mut Option<CancelGuard>,
) -> (String, String, Response) {
    let core = |resp: Response| ("core".to_string(), String::new(), resp);

    // 0. who is asking: a host the emulator does not serve (DNS rebinding) is refused before anything else
    if let Some(resp) = host_refusal {
        return core(resp);
    }
    // a service request is blocked when the browser says it comes from another site (see `access.rs` for why only then)
    let cross_site_blocked = browser.cross_site_fetch && !browser.permitted;

    // 1. admin API, then PathPrefix mounts under /_azemu/ (the latter via routing below)
    let is_admin_path = crate::routing::collapse_slashes(raw_path).starts_with(crate::routing::ADMIN_PREFIX);
    // Every admin path (core's and the services') refuses a request another web site sent, simple GET and form POST included.
    if is_admin_path && browser.cross_site && !shared.access.any_origin() {
        return core(access::cross_site_refusal(browser, access::Target::Admin));
    }
    if is_admin_path {
        if let Some(resp) = admin(&crate::routing::collapse_slashes(raw_path), raw_query, &parts.method, shared).await {
            return core(resp);
        }
    }

    // 2. route
    let routed: Routed = match kind {
        Kind::Pinned(svc) => route_pinned(svc, raw_path, authority),
        Kind::Gateway => match route_gateway(&shared.services, raw_path, authority, local.port()) {
            Ok(r) => r,
            Err(msg) => {
                if is_admin_path {
                    return core(json_error::response(
                        StatusCode::NOT_FOUND,
                        "NotFound",
                        &format!("Unknown azemu admin path '{raw_path}'."),
                    ));
                }
                return core(json_error::response(StatusCode::NOT_FOUND, "NoServiceMatched", &msg));
            }
        },
    };

    // 2b. A request from another web site goes on only when the service's own CORS rules accept the origin: core asks
    // the service with the preflight a browser would send (the services that have CORS rules answer it from them;
    // the others answer something that is not a CORS approval).
    if cross_site_blocked {
        let allowed = match browser.origin.as_deref() {
            Some(origin) => service_allows_origin(&routed, parts, origin, authority, scheme, now).await,
            None => false,
        };
        if !allowed {
            return (routed.service.name().to_string(), routed.account, access::cross_site_refusal(browser, access::Target::Service));
        }
    }

    // 3. body. A service may lower the limit for a request line (`Service::max_body_bytes`); the refusal is
    // made before any of the body is read.
    let limit = match routed.service.max_body_bytes(&parts.method, &routed.path) {
        Some(n) => n.min(shared.max_body),
        None => shared.max_body,
    };
    let declared = parts.headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let svc_for_limit = routed.service.clone();
    let too_large = move || {
        svc_for_limit.body_too_large(limit).unwrap_or_else(|| {
            json_error::response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "RequestBodyTooLarge",
                &format!("The request body is larger than the limit of {limit} bytes."),
            )
        })
    };
    let name = routed.service.name().to_string();
    if declared.is_some_and(|n| n > limit as u64) {
        return (name, routed.account, too_large());
    }
    let body_bytes = match read_body(body, limit, shared.body_timeout).await {
        Ok(b) => b,
        Err(BodyError::TooLarge) => return (name, routed.account, too_large()),
        Err(BodyError::Stalled) => {
            let resp = json_error::response(StatusCode::REQUEST_TIMEOUT, "RequestTimeout", "The request body stopped arriving.");
            return (name, routed.account, resp);
        }
        Err(BodyError::Broken) => {
            let resp = json_error::response(StatusCode::BAD_REQUEST, "InvalidRequestBody", "The request body could not be read.");
            return (name, routed.account, resp);
        }
    };

    // 4. build the request and call the service in its own task, so a panic
    // becomes a 500 instead of killing the connection.
    let request = Request {
        method: parts.method.clone(),
        path: routed.path,
        query: Query::parse(raw_query),
        headers: parts.headers.clone(),
        body: body_bytes,
        account: routed.account.clone(),
        ctx: RequestCtx {
            request_id: uuid::Uuid::new_v4().to_string(),
            time: now,
            endpoint: Endpoint { scheme, authority: authority.to_string(), path_style: routed.path_style, account: routed.account.clone() },
        },
    };
    let svc = routed.service;
    let shape_path = request.path.clone();
    // The handler runs in its own task. When the service asks for it (`Service::abort_on_disconnect`), the task is
    // aborted when this future is dropped, and that is what hyper does when the client closes the connection or
    // resets the HTTP/2 stream; the token then reaches work on blocking threads, and stays armed in `cancel_out`
    // until the answer is complete. Otherwise the task runs to its end whatever the client does (the token is armed
    // only for the answer's body, so a streamed answer that nobody reads is still stopped).
    let abort = svc.abort_on_disconnect(&parts.method, &shape_path);
    let token = CancelToken::new();
    let registry = shared.registry.clone();
    let work = crate::unsupported::scope(registry, cancel::scope(token.clone(), async move { svc.handle(request).await }));
    let resp = if abort {
        *cancel_out = Some(token.drop_guard());
        let mut task = AbortOnDrop(tokio::spawn(work));
        (&mut task.0).await
    } else {
        let r = tokio::spawn(work).await;
        *cancel_out = Some(token.drop_guard());
        r
    };
    let mut resp = match resp {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(service = %name, error = %e, "service handler panicked");
            json_error::response(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", "The service handler failed.")
        }
    };
    if resp.status() == StatusCode::NOT_IMPLEMENTED && crate::unsupported::marked(&resp).is_none() {
        let shape = crate::unsupported::request_shape(&parts.method, &shape_path, raw_query);
        resp.extensions_mut().insert(crate::unsupported::UnsupportedOperation(shape));
    }
    shared.refusals.note(&crate::refusals::Refused {
        service: &name,
        account: &routed.account,
        method: parts.method.as_str(),
        path: &shape_path,
        query: raw_query,
        headers: &parts.headers,
        status: resp.status(),
        error_code: resp.headers().get("x-ms-error-code").and_then(|v| v.to_str().ok()),
    });
    (name, routed.account, resp)
}

/// Would the service answer a CORS preflight for this request with an approval for `origin`? Approval: a 2xx answer with
/// `Access-Control-Allow-Origin` equal to the origin or `*`.
async fn service_allows_origin(
    routed: &Routed,
    parts: &http::request::Parts,
    origin: &str,
    authority: &str,
    scheme: &'static str,
    now: time::OffsetDateTime,
) -> bool {
    let mut headers = http::HeaderMap::new();
    let mut set = |name: &'static str, value: &str| {
        if let Ok(v) = HeaderValue::from_str(value) {
            headers.insert(header::HeaderName::from_static(name), v);
        }
    };
    set("host", authority);
    set("origin", origin);
    let own_preflight = parts.method == Method::OPTIONS && parts.headers.contains_key("access-control-request-method");
    if own_preflight {
        for n in ["access-control-request-method", "access-control-request-headers"] {
            if let Some(v) = parts.headers.get(n).and_then(|v| v.to_str().ok()) {
                set(if n.ends_with("method") { "access-control-request-method" } else { "access-control-request-headers" }, v);
            }
        }
    } else {
        set("access-control-request-method", parts.method.as_str());
        let listed = preflight_headers(&parts.headers);
        if !listed.is_empty() {
            set("access-control-request-headers", &listed);
        }
    }
    let request = Request {
        method: Method::OPTIONS,
        path: routed.path.clone(),
        query: Query::parse(""),
        headers,
        body: Bytes::new(),
        account: routed.account.clone(),
        ctx: RequestCtx {
            request_id: uuid::Uuid::new_v4().to_string(),
            time: now,
            endpoint: Endpoint { scheme, authority: authority.to_string(), path_style: routed.path_style, account: routed.account.clone() },
        },
    };
    let svc = routed.service.clone();
    let mut task = AbortOnDrop(tokio::spawn(async move { svc.handle(request).await }));
    let Ok(resp) = (&mut task.0).await else { return false };
    resp.status().is_success()
        && resp.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).and_then(|v| v.to_str().ok()).is_some_and(|v| v == "*" || v == origin)
}

/// The header names a browser would list in `Access-Control-Request-Headers` for this request: the ones a page may
/// set that are not CORS-safelisted (browsers add the others themselves and do not list them).
fn preflight_headers(h: &http::HeaderMap) -> String {
    const SKIP: &[&str] = &[
        "host",
        "connection",
        "content-length",
        "user-agent",
        "accept",
        "accept-language",
        "accept-encoding",
        "content-language",
        "origin",
        "referer",
        "cookie",
        "date",
        "dnt",
        "expect",
        "keep-alive",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "via",
        "priority",
        "upgrade-insecure-requests",
        "range",
        "pragma",
    ];
    let mut names: Vec<String> = h
        .iter()
        .filter_map(|(k, v)| {
            let n = k.as_str();
            if SKIP.contains(&n) || n.starts_with("sec-") || n.starts_with("proxy-") || n.starts_with("access-control-") {
                return None;
            }
            if n == "content-type" {
                let ct = v.to_str().unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
                if matches!(ct.as_str(), "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain" | "") {
                    return None;
                }
            }
            Some(n.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names.join(",")
}

enum BodyError {
    TooLarge,
    Stalled,
    Broken,
}

/// Read the whole body, at most `limit` bytes, giving up when no data arrives for `BODY_IDLE_TIMEOUT`.
async fn read_body(body: &mut Incoming, limit: usize, idle: Duration) -> Result<Bytes, BodyError> {
    use bytes::BufMut;
    let mut body = Limited::new(body, limit);
    let mut out = bytes::BytesMut::new();
    loop {
        match tokio::time::timeout(idle, body.frame()).await {
            Err(_) => return Err(BodyError::Stalled),
            Ok(None) => return Ok(out.freeze()),
            Ok(Some(Err(e))) => {
                return Err(if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                    BodyError::TooLarge
                } else {
                    BodyError::Broken
                })
            }
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    out.put(data);
                }
            }
        }
    }
}

/// The most a closing connection keeps reading and discarding what the peer still sends, and for how long. The bytes
/// must cover a body a client sends before it looks at the answer: the .NET HTTP client writes a whole request body of
/// 2 to 4 MB (a Cosmos item over its limit) and only then reads, so a limit of 1 MiB made it fail with "broken pipe"
/// when the server closed with the rest still on the way (finding CO1). A client that never stops is cut at the limit.
const LINGER_MAX_BYTES: usize = 32 * 1024 * 1024;
const LINGER_MAX_TIME: Duration = Duration::from_secs(5);

/// The stream of one HTTP connection. When the connection is dropped while the peer has not closed its
/// side, the stream is not closed at once: closing a socket that still has unread data in its receive
/// buffer makes the kernel send a reset, and a client that is still sending (a big upload we refused, a
/// request head we gave up on) then often sees "connection reset" instead of our answer. So the answer is
/// flushed, our side is shut down, and a task reads and discards what arrives for up to
/// `LINGER_MAX_BYTES` or `LINGER_MAX_TIME`, then closes ("lingering close", RFC 9112 section 9.6).
struct Linger<I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> {
    inner: Option<I>,
    /// The peer closed its side (or the read failed): nothing is left to read, no linger needed.
    peer_done: bool,
}

impl<I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> Linger<I> {
    fn new(inner: I) -> Self {
        Linger { inner: Some(inner), peer_done: false }
    }
    fn io(&mut self) -> std::io::Result<&mut I> {
        self.inner.as_mut().ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotConnected))
    }
}

impl<I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> tokio::io::AsyncRead for Linger<I> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let io = match self.io() {
            Ok(io) => io,
            Err(e) => return std::task::Poll::Ready(Err(e)),
        };
        let r = std::pin::Pin::new(io).poll_read(cx, buf);
        match &r {
            std::task::Poll::Ready(Ok(())) if buf.filled().len() == before => self.peer_done = true,
            std::task::Poll::Ready(Err(_)) => self.peer_done = true,
            _ => {}
        }
        r
    }
}

impl<I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> tokio::io::AsyncWrite for Linger<I> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.io() {
            Ok(io) => std::pin::Pin::new(io).poll_write(cx, buf),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.io() {
            Ok(io) => std::pin::Pin::new(io).poll_flush(cx),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.io() {
            Ok(io) => std::pin::Pin::new(io).poll_shutdown(cx),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }
    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.io() {
            Ok(io) => std::pin::Pin::new(io).poll_write_vectored(cx, bufs),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.as_ref().is_some_and(|io| io.is_write_vectored())
    }
}

impl<I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> Drop for Linger<I> {
    fn drop(&mut self) {
        let Some(io) = self.inner.take() else { return };
        if self.peer_done {
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(linger_close(io));
        }
    }
}

async fn linger_close<I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut io: I) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _ = tokio::time::timeout(LINGER_MAX_TIME, async {
        let _ = io.shutdown().await;
        let mut buf = vec![0u8; 16 * 1024];
        let mut total = 0usize;
        while total < LINGER_MAX_BYTES {
            match io.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n,
            }
        }
    })
    .await;
}

/// Core admin API. `None` when the path is not an admin path.
async fn admin(path: &str, query: &str, method: &Method, shared: &Shared) -> Option<Response> {
    let allowed = |m: &[Method]| m.contains(method);
    let method_not_allowed = |allow: &str| {
        Resp::new(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, allow)
            .json(&serde_json::json!({"error": {"code": "MethodNotAllowed", "message": "Method not allowed."}}))
            .build()
    };
    match path {
        "/_azemu/health" => Some(if allowed(&[Method::GET, Method::HEAD]) {
            let mut body = serde_json::json!({
                "status": "ok",
                "version": crate::VERSION,
                "commit": crate::GIT_COMMIT,
                "services": shared.services.iter().map(|s| s.name()).collect::<Vec<_>>(),
            });
            if let Some(p) = &shared.persist {
                body["persistence"] = p.health();
            }
            Resp::new(StatusCode::OK).json(&body).build()
        } else {
            method_not_allowed("GET, HEAD")
        }),
        "/_azemu/config" => Some(if allowed(&[Method::GET, Method::HEAD]) {
            Resp::new(StatusCode::OK).json(&config_json(shared)).build()
        } else {
            method_not_allowed("GET, HEAD")
        }),
        "/_azemu/unsupported" => Some(if allowed(&[Method::GET, Method::HEAD]) {
            Resp::new(StatusCode::OK).json(&shared.registry.to_json(Some(crate::unsupported::fallback()))).build()
        } else if allowed(&[Method::DELETE]) {
            shared.registry.clear();
            crate::unsupported::fallback().clear();
            Resp::new(StatusCode::OK).json(&serde_json::json!({})).build()
        } else {
            method_not_allowed("GET, HEAD, DELETE")
        }),
        "/_azemu/reset" => Some(if allowed(&[Method::POST]) { reset_response(shared, query).await } else { method_not_allowed("POST") }),
        "/_azemu/save" => Some(if allowed(&[Method::POST]) { save_response(shared).await } else { method_not_allowed("POST") }),
        "/_azemu/ca.pem" => Some(if allowed(&[Method::GET, Method::HEAD]) {
            Resp::new(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/x-pem-file")
                .body(Bytes::from(shared.resolver.ca_pem().to_string()))
                .build()
        } else {
            method_not_allowed("GET, HEAD")
        }),
        _ => None,
    }
}

/// `POST /_azemu/reset`, with `service=<name>` and `account=<name>` (both optional) and `save=true`. Nothing given:
/// every service is emptied and the answer is `{}`. An unknown parameter or service is a 400. A service that cannot
/// reset one account (`Service::reset_account` answered false) is named in `notSupported`.
async fn reset_response(shared: &Shared, query: &str) -> Response {
    let bad = |code: &str, message: String| json_error::response(StatusCode::BAD_REQUEST, code, &message);
    let mut service: Option<String> = None;
    let mut account: Option<String> = None;
    let mut save = false;
    let mut restricted = false;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let (k, v) = (decode_component(k), decode_component(v));
        match k.as_str() {
            "service" | "account" if v.is_empty() => {
                return bad("InvalidParameter", format!("The parameter '{k}' of /_azemu/reset needs a value."))
            }
            "service" => {
                service = Some(v);
                restricted = true;
            }
            "account" => {
                account = Some(v);
                restricted = true;
            }
            "save" => save = v.eq_ignore_ascii_case("true"),
            other => {
                return bad(
                    "InvalidParameter",
                    format!("Unknown parameter '{other}' for /_azemu/reset. Known: service=<name>, account=<name>, save=true."),
                )
            }
        }
    }
    let known: Vec<&'static str> = shared.services.iter().map(|s| s.name()).collect();
    if let Some(name) = &service {
        if !known.iter().any(|k| k == name) {
            return bad("UnknownService", format!("No service named '{name}' is mounted. Mounted: {}.", known.join(", ")));
        }
    }
    let mut body = serde_json::json!({});
    if !restricted {
        shared.reset_all();
    } else {
        let mut done: Vec<&str> = Vec::new();
        let mut not_supported: Vec<&str> = Vec::new();
        for s in shared.services.iter().filter(|s| service.as_deref().is_none_or(|n| n == s.name())) {
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &account {
                Some(a) => s.reset_account(a),
                None => {
                    s.reset();
                    true
                }
            }));
            match ok {
                Ok(true) => done.push(s.name()),
                Ok(false) => not_supported.push(s.name()),
                Err(_) => tracing::error!(service = s.name(), "reset panicked"),
            }
        }
        // one service asked for, and it cannot do it: nothing happened, say so plainly
        if service.is_some() && done.is_empty() && !not_supported.is_empty() {
            return json_error::response(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                &format!("The service '{}' cannot reset one account yet; nothing was reset. Use POST /_azemu/reset?service={} to reset all of it.", not_supported[0], not_supported[0]),
            );
        }
        if !not_supported.is_empty() {
            // A service with no state of its own per account (Identity) answers false: that is the normal case in
            // every per-test reset, and the answer lists it in `notSupported` for anyone who looks.
            tracing::debug!(services = %not_supported.join(","), "reset by account: these services keep no state per account and were left as they are");
        }
        body = serde_json::json!({"reset": done, "notSupported": not_supported});
    }
    if save {
        let saved = save_response(shared).await;
        if !restricted {
            return saved;
        }
        // keep what the reset did in the answer
        let (parts, bytes) = saved.into_parts();
        let mut merged: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| serde_json::json!({}));
        if let (Some(m), Some(b)) = (merged.as_object_mut(), body.as_object()) {
            m.extend(b.clone());
        }
        return Resp::new(parts.status).json(&merged).build();
    }
    Resp::new(StatusCode::OK).json(&body).build()
}

fn decode_component(s: &str) -> String {
    percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().into_owned()
}

/// The body of `GET /_azemu/config`: what is running and with which settings.
fn config_json(shared: &Shared) -> serde_json::Value {
    let listeners: Vec<serde_json::Value> = shared
        .listeners
        .get()
        .map(|l| {
            l.iter()
                .map(|(service, addr)| match service {
                    None => serde_json::json!({"kind": "gateway", "address": addr.to_string()}),
                    Some(s) => serde_json::json!({"kind": "pinned", "service": s, "address": addr.to_string()}),
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "version": crate::VERSION,
        "commit": crate::GIT_COMMIT,
        "services": shared.services.iter().map(|s| s.name()).collect::<Vec<_>>(),
        "listeners": listeners,
        "dataDirectory": shared.persist.as_ref().map(|p| p.root().display().to_string()),
        "saveIntervalSeconds": shared.persist.as_ref().map(|_| shared.config.save_interval.unwrap_or(Duration::from_secs(DEFAULT_SAVE_INTERVAL_SECS)).as_secs()),
        "allowedHosts": {
            "builtIn": ["IP addresses", "localhost", "*.localhost", "names with no dot", "host.docker.internal", "gateway.docker.internal"],
            "suffixes": shared.config.allow_hosts,
        },
        "tlsNames": shared.config.tls_names,
        "allowedOrigins": shared.config.allow_origins,
        "crossSiteCheck": if shared.access.any_origin() { "off" } else { "on" },
        "settings": crate::settings::in_force(),
        // `AZEMU_*` variables in the environment that no code has read (yet): a typing mistake, or a setting that a
        // service reads on first use
        "unreadVariables": crate::settings::unread_variables(),
    })
}

/// Save what changed and answer with the result per service: 200 when every save worked, 500 when one failed,
/// 409 when the emulator has no data directory.
async fn save_response(shared: &Shared) -> Response {
    let Some(p) = &shared.persist else {
        return json_error::response(
            StatusCode::CONFLICT,
            "NoDataDirectory",
            "This emulator was started without --data-dir, so there is nowhere to save to.",
        );
    };
    let results = p.save_all(false).await;
    let mut services = serde_json::Map::new();
    let mut all_ok = true;
    for r in &results {
        let v = match &r.outcome {
            SaveOutcome::Saved => serde_json::json!({"result": "saved"}),
            SaveOutcome::Unchanged => serde_json::json!({"result": "unchanged"}),
            SaveOutcome::Failed(e) => {
                all_ok = false;
                serde_json::json!({"result": "error", "error": e})
            }
        };
        services.insert(r.service.to_string(), v);
    }
    let status = if all_ok { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR };
    Resp::new(status).json(&serde_json::json!({"directory": p.root().display().to_string(), "services": services})).build()
}

fn can_have_body(status: StatusCode) -> bool {
    !(status.is_informational() || status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED)
}

/// Add `Date` and `Content-Length`; leave anything the service set alone.
fn finalize(resp: Response, is_head: bool, now: time::OffsetDateTime, streamed: bool) -> http::Response<AnswerBody> {
    let (mut parts, body) = resp.into_parts();
    if !parts.headers.contains_key(header::DATE) {
        if let Ok(v) = HeaderValue::from_str(&crate::time::rfc1123(now)) {
            parts.headers.insert(header::DATE, v);
        }
    }
    // An error answer to HEAD has no body and says nothing about one: no `Content-Type` and no `Content-Length` computed from
    // the error body that is not sent (Azure sends neither; the error code is in `x-ms-error-code`). A client that sees
    // `Content-Type: application/xml` parses the empty body as the error and fails with a parser exception (the .NET SDK did).
    // A `Content-Length` the service set itself stays.
    let head_error = is_head && (parts.status.is_client_error() || parts.status.is_server_error());
    if head_error {
        parts.headers.remove(header::CONTENT_TYPE);
    }
    if !parts.headers.contains_key(header::CONTENT_LENGTH)
        && !parts.headers.contains_key(header::TRANSFER_ENCODING)
        && can_have_body(parts.status)
        && !head_error
        && !streamed
    {
        parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    }
    let body = if is_head || !can_have_body(parts.status) { Bytes::new() } else { body };
    http::Response::from_parts(parts, AnswerBody::Full(Some(body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn listen(ip: [u8; 4], port: u16, pinned: Option<&str>, optional: bool) -> Listen {
        Listen { addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port), pinned: pinned.map(str::to_string), optional }
    }

    #[tokio::test]
    async fn addresses_that_ask_for_port_zero_share_one_port() {
        // 127.0.0.2 stands in for ::1: another address of this machine that only loopback traffic reaches
        let ls = vec![listen([127, 0, 0, 1], 0, None, false), listen([127, 0, 0, 2], 0, None, true)];
        let bound = bind_listeners(ls, TcpListener::bind).await.unwrap();
        let ports: Vec<u16> = bound.iter().map(|(_, x)| x.local_addr().unwrap().port()).collect();
        assert_eq!(ports.len(), 2);
        assert_ne!(ports[0], 0);
        assert_eq!(ports[0], ports[1]);
    }

    #[tokio::test]
    async fn a_port_another_address_refuses_is_replaced_by_a_fresh_one() {
        // the second address refuses the first two ports it is offered
        let refused = AtomicUsize::new(0);
        let seen = std::sync::Mutex::new(Vec::new());
        let ls = vec![listen([127, 0, 0, 1], 0, None, false), listen([127, 0, 0, 2], 0, None, false)];
        let bound = bind_listeners(ls, |a| {
            let refuse = a.ip() == IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)) && refused.fetch_add(1, Ordering::SeqCst) < 2;
            seen.lock().unwrap().push(a);
            async move {
                if refuse {
                    Err(std::io::Error::from(std::io::ErrorKind::AddrInUse))
                } else {
                    TcpListener::bind(a).await
                }
            }
        })
        .await
        .unwrap();
        let ports: Vec<u16> = bound.iter().map(|(_, x)| x.local_addr().unwrap().port()).collect();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0], ports[1]);
        assert_eq!(refused.load(Ordering::SeqCst), 3, "two refusals, then success");
        let offered: std::collections::HashSet<u16> =
            seen.lock().unwrap().iter().filter(|a| a.ip() == IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))).map(|a| a.port()).collect();
        assert_eq!(offered.len(), 3, "each attempt offered the second address a different port: {offered:?}");
    }

    #[tokio::test]
    async fn gateway_and_pins_each_get_their_own_port_and_fixed_ports_stay_fixed() {
        let ls = vec![
            listen([127, 0, 0, 1], 0, None, false),
            listen([127, 0, 0, 1], 0, Some("blob"), false),
            listen([127, 0, 0, 2], 0, Some("blob"), true),
            listen([127, 0, 0, 2], 0, None, true),
        ];
        let bound = bind_listeners(ls, TcpListener::bind).await.unwrap();
        let port = |i: usize| bound[i].1.local_addr().unwrap().port();
        assert_eq!(bound.len(), 4);
        assert_eq!(port(0), port(3), "gateway");
        assert_eq!(port(1), port(2), "pin");
        assert_ne!(port(0), port(1));
    }

    #[tokio::test]
    async fn an_optional_address_that_cannot_be_bound_is_left_out_and_the_others_keep_their_port() {
        let ls = vec![listen([127, 0, 0, 1], 0, None, false), listen([127, 0, 0, 2], 0, None, true)];
        let bound = bind_listeners(ls, |a| async move {
            if a.ip() == IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)) {
                Err(std::io::Error::from(std::io::ErrorKind::AddrNotAvailable))
            } else {
                TcpListener::bind(a).await
            }
        })
        .await
        .unwrap();
        assert_eq!(bound.len(), 1);
    }
}
