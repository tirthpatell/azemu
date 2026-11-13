//! Shared helpers for the azemu Rust SDK tests.
//!
//! Each test binary calls [`emu()`]. The first call starts one emulator on a free port with a
//! temporary certificate directory, waits for `/_azemu/health`, builds a CA bundle and points the
//! rustls platform verifier at it (`SSL_CERT_FILE`). The emulator is stopped when the test
//! process exits. Tests share the emulator and use unique resource names ([`uniq`]).
//!
//! Environment:
//! * `AZEMU_GATEWAY` (for example `http://127.0.0.1:4577`) use a running emulator instead.
//!   `AZEMU_CA_FILE` then names its `ca.pem`.
//! * `AZEMU_BIN` the emulator binary to start. Default: `target/azemu-under-test` in this
//!   directory, else `../../target/debug/azemu`.
//! * `AZEMU_TEST_KEEP_LOGS=1` keep the certificate directory and the emulator log.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azure_core::credentials::{AccessToken, Secret, TokenCredential, TokenRequestOptions};
use azure_core::time::OffsetDateTime;

pub struct Emu {
    pub port: u16,
    pub ca_pem: Option<PathBuf>,
    pub dir: Option<PathBuf>,
    pub external: bool,
}

static EMU: OnceLock<Emu> = OnceLock::new();
static CHILD: Mutex<Option<Child>> = Mutex::new(None);
static COUNTER: AtomicUsize = AtomicUsize::new(0);
static PID_SEED: AtomicU32 = AtomicU32::new(0);

extern "C" fn stop_emulator() {
    if let Ok(mut g) = CHILD.lock() {
        if let Some(mut c) = g.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    if std::env::var("AZEMU_TEST_KEEP_LOGS").is_err() {
        if let Some(e) = EMU.get() {
            if let Some(d) = &e.dir {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

fn emulator_binary() -> PathBuf {
    if let Ok(b) = std::env::var("AZEMU_BIN") {
        return PathBuf::from(b);
    }
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // `tests/sdk-rust/target/azemu-under-test` (also from the `beta` project one level down), else the repository build.
    for rel in ["target/azemu-under-test", "../target/azemu-under-test", "../../target/debug/azemu", "../../../target/debug/azemu"] {
        let c = here.join(rel);
        if c.exists() {
            return c;
        }
    }
    here.join("../../target/debug/azemu")
}

pub fn http_get(port: u16, path: &str) -> Option<String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").ok()?;
    let mut out = Vec::new();
    s.read_to_end(&mut out).ok()?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn start() -> Emu {
    if let Ok(g) = std::env::var("AZEMU_GATEWAY") {
        let port: u16 = g.trim_end_matches('/').rsplit(':').next().unwrap().parse().unwrap();
        let ca = std::env::var("AZEMU_CA_FILE").ok().map(PathBuf::from);
        let e = Emu { port, ca_pem: ca, dir: None, external: true };
        trust(&e);
        return e;
    }
    let bin = emulator_binary();
    assert!(bin.exists(), "emulator binary not found at {bin:?}; build it with `cargo build -p azemu` in the repository root");
    let port = free_port();
    PID_SEED.store(std::process::id(), Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("azemu-rust-sdk-{}-{}", std::process::id(), port));
    std::fs::create_dir_all(&dir).expect("cert dir");
    let log = std::fs::File::create(dir.join("azemu.log")).expect("log");
    let log2 = log.try_clone().expect("log clone");
    let child = Command::new(&bin)
        .args(["--port", &port.to_string(), "--cert-dir"])
        .arg(&dir)
        .args(["--log-level", &std::env::var("AZEMU_TEST_LOG_LEVEL").unwrap_or_else(|_| "debug".into())])
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .expect("start emulator");
    *CHILD.lock().unwrap() = Some(child);
    unsafe {
        libc::atexit(stop_emulator);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(r) = http_get(port, "/_azemu/health") {
            if r.starts_with("HTTP/1.1 200") {
                break;
            }
        }
        if let Some(c) = CHILD.lock().unwrap().as_mut() {
            if let Ok(Some(st)) = c.try_wait() {
                panic!("emulator exited early: {st}");
            }
        }
        assert!(Instant::now() < deadline, "emulator did not become healthy");
        std::thread::sleep(Duration::from_millis(100));
    }
    let e = Emu { port, ca_pem: Some(dir.join("ca.pem")), dir: Some(dir), external: false };
    trust(&e);
    e
}

/// Rustls (through rustls-platform-verifier) reads the system trust list; `SSL_CERT_FILE`
/// replaces it, so the file has to be the system list plus the emulator's CA.
fn trust(e: &Emu) {
    let Some(ca) = &e.ca_pem else { return };
    let mut bundle = String::new();
    for sys in ["/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt"] {
        if let Ok(s) = std::fs::read_to_string(sys) {
            bundle.push_str(&s);
            bundle.push('\n');
            break;
        }
    }
    bundle.push_str(&std::fs::read_to_string(ca).expect("ca.pem"));
    let path = ca.with_file_name("bundle.pem");
    std::fs::write(&path, bundle).expect("write bundle");
    std::env::set_var("SSL_CERT_FILE", &path);
}

pub fn emu() -> &'static Emu {
    EMU.get_or_init(|| {
        let e = start();
        // App Service style managed identity (README: "Use DefaultAzureCredential with no code change").
        std::env::set_var("IDENTITY_ENDPOINT", format!("http://localhost:{}/_azemu/identity/msi/token", e.port));
        std::env::set_var("IDENTITY_HEADER", "azemu");
        e
    })
}

impl Emu {
    pub fn host(&self, label: &str, name: &str) -> String {
        let suffix = if matches!(label, "blob" | "queue" | "table" | "dfs") { "core.localhost" } else { "localhost" };
        format!("{name}.{label}.{suffix}:{}", self.port)
    }
    pub fn https(&self, label: &str, name: &str) -> String {
        format!("https://{}/", self.host(label, name))
    }
    pub fn http(&self, label: &str, name: &str) -> String {
        format!("http://{}/", self.host(label, name))
    }
    pub fn gateway(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    pub fn log_path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join("azemu.log"))
    }
}

/// A unique lowercase name: `{prefix}{pid-ish}{counter}{random}`, at most 24 characters for a prefix up to 8.
pub fn uniq(prefix: &str) -> String {
    use rand::Rng;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let r: u32 = rand::rng().random_range(0..0xfffff);
    format!("{prefix}{n:x}{r:05x}")
}

/// A credential that returns one fixed token, as the emulator accepts any bearer token.
#[derive(Debug)]
pub struct StaticToken(pub String);

#[async_trait]
impl TokenCredential for StaticToken {
    async fn get_token(&self, _scopes: &[&str], _o: Option<TokenRequestOptions<'_>>) -> azure_core::Result<AccessToken> {
        Ok(AccessToken::new(Secret::new(self.0.clone()), OffsetDateTime::now_utc() + azure_core::time::Duration::hours(1)))
    }
}

pub fn cred() -> Arc<dyn TokenCredential> {
    Arc::new(StaticToken("azemu-test-token".into()))
}

/// A `ClientSecretCredential` whose token requests go to the emulator's login endpoint
/// (`https://login.localhost:{port}/{tenant}/oauth2/v2.0/token`). The Rust SDK has no
/// instance discovery, so the authority host alone is enough.
pub fn client_secret_cred(tenant: &str, client_id: &str, secret: &str) -> Arc<azure_identity::ClientSecretCredential> {
    use azure_core::cloud::{CloudConfiguration, CustomConfiguration};
    use azure_core::http::ClientOptions;
    let mut cloud = CustomConfiguration::default();
    cloud.authority_host = format!("https://login.localhost:{}", emu().port);
    let mut client_options = ClientOptions::default();
    client_options.cloud = Some(Arc::new(CloudConfiguration::Custom(cloud)));
    azure_identity::ClientSecretCredential::new(
        tenant,
        client_id.to_string(),
        Secret::new(secret.to_string()),
        Some(azure_identity::ClientSecretCredentialOptions { client_options }),
    )
    .expect("client secret credential")
}

/// The claims (second part) of a JWT, decoded.
pub fn jwt_claims(token: &str) -> serde_json::Value {
    use base64::Engine;
    let part = token.split('.').nth(1).expect("jwt has three parts");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(part).expect("jwt payload base64url");
    serde_json::from_slice(&bytes).expect("jwt payload json")
}

/// A tiny HTTP/1.1 request to the emulator's gateway (for the `/_azemu/...` admin APIs). Returns status and body.
pub fn http_call(method: &str, path: &str, body: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let port = emu().port;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("write");
    let mut out = Vec::new();
    s.read_to_end(&mut out).ok();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// An event hub, made through the emulator's admin API (`PUT /_azemu/eventhubs/{ns}/hubs/{hub}`). The emulator does not create hubs
/// by being named (`AZEMU_EVENTHUBS_AUTOCREATE` is off by default), as Azure does not, so a test that uses a hub makes it first.
/// `ns` is the namespace label (the first label of the host name). Event hubs get the consumer group `analytics` next to `$Default`.
pub fn create_eventhub(ns: &str, hub: &str) {
    let (st, b) = http_call("PUT", &format!("/_azemu/eventhubs/{ns}/hubs/{hub}"), r#"{"consumerGroups":["analytics"]}"#);
    assert!(st == 200 || st == 201, "creating the event hub {ns}/{hub}: {st} {b}");
}

/// A request to the management API (ARM) on the emulator's gateway: the host name `management.localhost` picks ARM, any bearer
/// token is accepted, and plain HTTP is enough here (the Rust SDK has no management crate for Storage). Returns status and body.
fn arm_call(method: &str, path: &str, body: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let port = emu().port;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: management.localhost:{port}\r\nAuthorization: Bearer azemu-test-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write");
    let mut out = Vec::new();
    s.read_to_end(&mut out).ok();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, text.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}

/// A storage account with blob versioning on, and its name. Versioning is off until someone turns it on, and a team
/// turns it on through the management API: a resource group, the storage account, and `isVersioningEnabled` on
/// `blobServices/default`. Use any bearer token with the account's Blob endpoint (`emu().https("blob", &name)`).
pub fn account_with_versioning(prefix: &str) -> String {
    const SUB: &str = "11111111-2222-3333-4444-555555555555";
    let group = uniq("rgver");
    let name = uniq(prefix);
    let (st, b) =
        arm_call("PUT", &format!("/subscriptions/{SUB}/resourcegroups/{group}?api-version=2021-04-01"), r#"{"location":"eastus"}"#);
    assert!(st == 200 || st == 201, "resource group: {st} {b}");
    let (st, b) = arm_call(
        "PUT",
        &format!("/subscriptions/{SUB}/resourceGroups/{group}/providers/Microsoft.Storage/storageAccounts/{name}?api-version=2023-01-01"),
        r#"{"location":"eastus","kind":"StorageV2","sku":{"name":"Standard_LRS"}}"#,
    );
    assert!(st == 200 || st == 201 || st == 202, "storage account: {st} {b}");
    let (st, b) = arm_call(
        "PUT",
        &format!("/subscriptions/{SUB}/resourceGroups/{group}/providers/Microsoft.Storage/storageAccounts/{name}/blobServices/default?api-version=2023-01-01"),
        r#"{"properties":{"isVersioningEnabled":true}}"#,
    );
    assert!(st == 200 && b.contains(r#""isVersioningEnabled":true"#), "versioning was not switched on: {st} {b}");
    name
}
