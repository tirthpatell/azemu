//! Helpers for the tests of the older crate versions: one emulator per test binary on a free port, with a certificate directory of
//! its own; the system's certificate list plus the emulator's authority in `SSL_CERT_FILE`; unique names; plain HTTP calls to the
//! emulator for what these versions cannot do (making a Cosmos database and container); an account SAS signed with the development key.
//! (The main project's `src/lib.rs` is not shared: it uses parts of `azure_core` that are newer than 1.0.0.)
//!
//! Environment: `AZEMU_GATEWAY` (a running emulator, with `AZEMU_CA_FILE` for its `ca.pem`), `AZEMU_BIN` (the binary to start),
//! `AZEMU_TEST_KEEP_LOGS=1` (keep the certificate directory and `azemu.log`).

#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azure_core::credentials::{AccessToken, Secret, TokenCredential, TokenRequestOptions};
use azure_core::time::OffsetDateTime;

/// The well-known development account and key (the same as Azurite's).
pub const DEV_ACCOUNT: &str = "devstoreaccount1";
pub const DEV_KEY: &str = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
/// The well-known Cosmos DB emulator key. The Rust crate signs requests itself and needs valid base64.
pub const COSMOS_KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

pub struct Emu {
    pub port: u16,
    pub ca_pem: Option<PathBuf>,
    pub dir: Option<PathBuf>,
}

static EMU: OnceLock<Emu> = OnceLock::new();
static CHILD: Mutex<Option<Child>> = Mutex::new(None);
static COUNTER: AtomicUsize = AtomicUsize::new(0);

extern "C" fn stop_emulator() {
    if let Ok(mut g) = CHILD.lock() {
        if let Some(mut c) = g.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    if std::env::var("AZEMU_TEST_KEEP_LOGS").is_err() {
        if let Some(d) = EMU.get().and_then(|e| e.dir.as_ref()) {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

fn emulator_binary() -> PathBuf {
    if let Ok(b) = std::env::var("AZEMU_BIN") {
        return PathBuf::from(b);
    }
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // `tests/sdk-rust/target/azemu-under-test` (the copy the other Rust projects use too), else the repository build.
    for rel in ["../target/azemu-under-test", "../../../target/debug/azemu"] {
        let c = here.join(rel);
        if c.exists() {
            return c;
        }
    }
    here.join("../../../target/debug/azemu")
}

fn http_get(port: u16, path: &str) -> Option<String> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").ok()?;
    let mut out = Vec::new();
    s.read_to_end(&mut out).ok()?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn start() -> Emu {
    if let Ok(g) = std::env::var("AZEMU_GATEWAY") {
        let port: u16 = g.trim_end_matches('/').rsplit(':').next().and_then(|p| p.parse().ok()).expect("AZEMU_GATEWAY needs a port");
        let e = Emu { port, ca_pem: std::env::var("AZEMU_CA_FILE").ok().map(PathBuf::from), dir: None };
        trust(&e);
        return e;
    }
    let bin = emulator_binary();
    assert!(bin.exists(), "emulator binary not found at {bin:?}; build it with `cargo build -p azemu` in the repository root");
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let dir = std::env::temp_dir().join(format!("azemu-rust-older-{}-{}", std::process::id(), port));
    std::fs::create_dir_all(&dir).expect("cert dir");
    let log = std::fs::File::create(dir.join("azemu.log")).expect("log");
    let log2 = log.try_clone().expect("log clone");
    let child = Command::new(&bin)
        .args(["--port", &port.to_string(), "--cert-dir"])
        .arg(&dir)
        .args(["--log-level", "debug"])
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
        if http_get(port, "/_azemu/health").is_some_and(|r| r.starts_with("HTTP/1.1 200")) {
            break;
        }
        if let Some(c) = CHILD.lock().unwrap().as_mut() {
            if let Ok(Some(st)) = c.try_wait() {
                panic!("emulator exited early: {st}");
            }
        }
        assert!(Instant::now() < deadline, "emulator did not become healthy");
        std::thread::sleep(Duration::from_millis(100));
    }
    let e = Emu { port, ca_pem: Some(dir.join("ca.pem")), dir: Some(dir) };
    trust(&e);
    e
}

/// Rustls (through rustls-platform-verifier) reads the system list on Linux, and `SSL_CERT_FILE` replaces it, so the file is the
/// system list plus the emulator's authority. This works on Linux only: on macOS the authority has to be in the Keychain (docs/rust.md).
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
        std::env::set_var("IDENTITY_ENDPOINT", format!("http://localhost:{}/_azemu/identity/msi/token", e.port));
        std::env::set_var("IDENTITY_HEADER", "azemu");
        // 0.36.0 does not read AZURE_COSMOS_BINARY_ENCODING_ENABLED (the 1.x crates send binary-encoded bodies unless it is "false").
        std::env::set_var("COSMOS_DISABLE_IMDS", "1");
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

    /// A plain HTTP/1.1 request to the gateway with the given `Host`, for the calls these crate versions cannot make.
    /// Returns the status and the body.
    pub fn call(&self, method: &str, host: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        write!(s, "{head}\r\n{body}").expect("write");
        let mut out = Vec::new();
        s.read_to_end(&mut out).ok();
        let text = String::from_utf8_lossy(&out).into_owned();
        let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        (status, text.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
    }

    /// A Cosmos database and a container (partition key `/pk`) in `account`, made with the two `POST`s a client would send.
    /// The `Authorization` value only has to be there.
    pub fn create_cosmos(&self, account: &str, database: &str, container: &str) {
        let host = self.host("documents", account);
        let headers = [("Authorization", "x"), ("x-ms-version", "2018-12-31"), ("Content-Type", "application/json")];
        let (st, b) = self.call("POST", &host, "/dbs", &headers, &format!(r#"{{"id":"{database}"}}"#));
        assert_eq!(st, 201, "creating the database: {b}");
        let body = format!(r#"{{"id":"{container}","partitionKey":{{"paths":["/pk"],"kind":"Hash","version":2}}}}"#);
        let (st, b) = self.call("POST", &host, &format!("/dbs/{database}/colls"), &headers, &body);
        assert_eq!(st, 201, "creating the container: {b}");
    }
}

/// A unique lowercase name: `{prefix}{counter}{random}`.
pub fn uniq(prefix: &str) -> String {
    use rand::Rng;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let r: u32 = rand::rng().random_range(0..0xfffff);
    format!("{prefix}{n:x}{r:05x}")
}

/// A credential that returns one fixed token (the emulator accepts any bearer token).
#[derive(Debug)]
pub struct StaticToken(pub String);

#[async_trait]
impl TokenCredential for StaticToken {
    async fn get_token(&self, _scopes: &[&str], _o: Option<TokenRequestOptions<'_>>) -> azure_core::Result<AccessToken> {
        Ok(AccessToken::new(Secret::new(self.0.clone()), OffsetDateTime::now_utc() + azure_core::time::Duration::hours(1)))
    }
}

pub fn static_cred() -> Arc<dyn TokenCredential> {
    Arc::new(StaticToken("azemu-test-token".into()))
}

/// An account SAS for the development account, made by hand (this crate version has no SAS generation): read, write, list, create and
/// delete on blobs and containers, valid for an hour, for use over plain http. The string to sign is the one of service version
/// 2020-12-06; the parts are separated by a newline, and an empty part (start, IP, encryption scope) is an empty line.
pub fn dev_account_sas() -> String {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let key = base64::engine::general_purpose::STANDARD.decode(DEV_KEY).expect("key");
    let t = OffsetDateTime::now_utc() + azure_core::time::Duration::hours(1);
    let expiry = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second());
    let (sp, ss, srt, spr, sv) = ("rwdlac", "b", "sco", "https,http", "2020-12-06");
    let to_sign = format!("{DEV_ACCOUNT}\n{sp}\n{ss}\n{srt}\n\n{expiry}\n\n{spr}\n{sv}\n\n");
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).expect("hmac key");
    mac.update(to_sign.as_bytes());
    let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    let enc = |s: &str| s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D").replace(':', "%3A").replace(',', "%2C");
    format!("sv={sv}&ss={ss}&srt={srt}&sp={sp}&se={}&spr={}&sig={}", enc(&expiry), enc(spr), enc(&sig))
}
