#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The key of Microsoft's Cosmos DB emulator. azemu accepts any key, but the Azure SDK for Rust refuses one that is not
/// base64, so tests that need a key use this one.
pub const COSMOS_EMULATOR_KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

/// How long `start` waits for the ready line.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// A service and the address forms a client uses for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Service {
    Blob,
    Queue,
    Table,
    DataLake,
    KeyVault,
    AppConfig,
    Cosmos,
    EventGrid,
    ServiceBus,
    EventHubs,
}

impl Service {
    /// What follows the account in the host-name form: `blob.core.localhost` in `myacct.blob.core.localhost`.
    pub fn host_suffix(self) -> &'static str {
        match self {
            Service::Blob => "blob.core.localhost",
            Service::Queue => "queue.core.localhost",
            Service::Table => "table.core.localhost",
            Service::DataLake => "dfs.core.localhost",
            Service::KeyVault => "vault.localhost",
            Service::AppConfig => "azconfig.localhost",
            Service::Cosmos => "documents.localhost",
            Service::EventGrid => "eventgrid.localhost",
            Service::ServiceBus => "servicebus.localhost",
            Service::EventHubs => "eventhub.localhost",
        }
    }

    /// The service's port when the emulator was started with `--standard-ports` (the container image does that).
    pub fn standard_port(self) -> u16 {
        match self {
            Service::Blob => 10000,
            Service::Queue => 10001,
            Service::Table => 10002,
            Service::DataLake => 10003,
            Service::KeyVault => 10010,
            Service::AppConfig => 10011,
            Service::EventGrid => 10012,
            Service::Cosmos => 8081,
            Service::ServiceBus | Service::EventHubs => 5672,
        }
    }

    /// The services that take the account from the first path segment on their standard port (the others take it from
    /// the host name).
    fn account_in_path(self) -> bool {
        matches!(self, Service::Blob | Service::Queue | Service::Table | Service::DataLake)
    }
}

/// An answer to [`Emulator::request`].
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub body: String,
}

/// A running emulator: one this crate started, or one that was already running.
#[derive(Debug)]
pub struct Emulator {
    /// The host part of the gateway address (`127.0.0.1`, or the name the gateway was given by).
    host: String,
    port: u16,
    /// Where the emulator's `ca.pem` is, once known.
    ca_file: OnceLock<PathBuf>,
    /// Where files this crate writes go (the fetched `ca.pem`, `bundle.pem`).
    work_dir: PathBuf,
    /// The emulator's log, for one that this crate started.
    log: Option<PathBuf>,
    owned: Option<Owned>,
}

/// The process of an emulator this crate started. The pipe on its standard input is what ends it: the emulator is
/// started with `--exit-on-stdin-close`, and the system closes the pipe when this process ends, however it ends.
#[derive(Debug)]
struct Owned {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    _stdout: Mutex<BufReader<ChildStdout>>,
}

static SHARED: OnceLock<Emulator> = OnceLock::new();
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The emulator of this test program. The first call finds or starts it, and every call after that returns the same one.
///
/// * `AZEMU_GATEWAY` set (`http://127.0.0.1:4577`, or the address of a container): that emulator is used and none is
///   started. Its certificate authority is read from the file `AZEMU_CA_FILE` names, or fetched from the emulator.
/// * Otherwise `azemu` is started on a free port (see [`Emulator::start`]) and stops when the test program ends.
///
/// The first call also does [`Emulator::apply_environment`]. It panics, with what to do about it, when no emulator can be
/// had.
pub fn shared() -> &'static Emulator {
    SHARED.get_or_init(|| {
        let emu = match std::env::var("AZEMU_GATEWAY") {
            Ok(gateway) if !gateway.trim().is_empty() => Emulator::connect(gateway.trim()).unwrap_or_else(|e| panic!("azemu-testkit: {e}")),
            _ => Emulator::start(),
        };
        emu.apply_environment();
        emu
    })
}

/// A name that no other test in any test program uses, made of lowercase letters and digits and at most 24 characters long,
/// so that a storage account, a vault, a Cosmos account and a namespace take it. `prefix` (letters and digits are kept, up
/// to 8 of them) says what it is for; start it with a letter.
pub fn unique_name(prefix: &str) -> String {
    let mut name: String = prefix.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).take(8).collect();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    name.push_str(&format!("{:04x}{:06x}{:x}", std::process::id() & 0xffff, nanos & 0xff_ffff, n & 0xfff));
    name.truncate(24);
    name
}

impl Emulator {
    /// Start `azemu` on a free port, with a certificate directory of its own, and wait until it is ready. The emulator
    /// is stopped when the returned value is dropped, and when the test program ends in any other way.
    ///
    /// The program is the file `AZEMU_BIN` names, else `azemu` from the `PATH`. `AZEMU_TESTKIT_ARGS` adds arguments (split
    /// at spaces), for example `--lenient`. The files go in `target/tmp` of the project when the test program runs from
    /// a `target` directory, else in the system's temporary directory; `AZEMU_TESTKIT_DIR` names another place. The
    /// emulator's log is `azemu.log` there. The same test program uses the same directory on every run, so the certificate
    /// authority is made once.
    ///
    /// Panics when the emulator cannot be started; [`Emulator::try_start`] returns the error instead.
    pub fn start() -> Emulator {
        Emulator::try_start(&[]).unwrap_or_else(|e| panic!("azemu-testkit: {e}"))
    }

    /// [`Emulator::start`] with more arguments for `azemu`, and an error instead of a panic.
    pub fn try_start(extra_args: &[&str]) -> Result<Emulator, String> {
        let program = std::env::var("AZEMU_BIN").ok().filter(|b| !b.trim().is_empty());
        let (bin, from_env) = match &program {
            Some(b) => (b.as_str(), true),
            None => ("azemu", false),
        };
        let work_dir = work_dir_for_this_program();
        create_private_dir(&work_dir).map_err(|e| format!("cannot create {}: {e}", work_dir.display()))?;
        let log = work_dir.join("azemu.log");
        let log_file = std::fs::File::create(&log).map_err(|e| format!("cannot write {}: {e}", log.display()))?;
        let mut cmd = Command::new(bin);
        cmd.args(["--port", "0", "--exit-on-stdin-close", "--cert-dir"]).arg(&work_dir);
        if let Ok(extra) = std::env::var("AZEMU_TESTKIT_ARGS") {
            cmd.args(extra.split_whitespace());
        }
        cmd.args(extra_args);
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::from(log_file)).spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                if from_env {
                    format!("AZEMU_BIN names `{bin}`, which does not exist. Build azemu (`cargo build -p azemu`) and point AZEMU_BIN at target/debug/azemu, or unset it to use `azemu` from the PATH")
                } else {
                    "`azemu` was not found on the PATH. Install it (`cargo install --locked --git https://github.com/tirthpatell/azemu azemu`), or set AZEMU_BIN to the program, or set AZEMU_GATEWAY to an emulator that is already running".to_string()
                }
            } else {
                format!("cannot start `{bin}`: {e}")
            }
        })?;
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().ok_or("no standard output on the emulator")?);
        // The ready line is read on a thread of its own so that an emulator that says nothing is given up on.
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let _ = stdout.read_line(&mut line);
            let _ = tx.send(line);
            stdout
        });
        let line = match rx.recv_timeout(START_TIMEOUT) {
            Ok(line) if line.starts_with("azemu ready ") => line,
            other => {
                let _ = child.kill();
                let _ = child.wait();
                let said = other.unwrap_or_default();
                return Err(format!(
                    "azemu did not become ready (`{bin}`{}).{} Its log, {}:\n{}",
                    if said.is_empty() { String::new() } else { format!(", first line: {}", said.trim()) },
                    if extra_args.is_empty() { "" } else { " (with the extra arguments given)" },
                    log.display(),
                    std::fs::read_to_string(&log).unwrap_or_default()
                ));
            }
        };
        let stdout = reader.join().map_err(|_| "the reader of the ready line failed".to_string())?;
        let field = |key: &str| line.split_whitespace().find_map(|w| w.strip_prefix(key)).map(str::to_string);
        let gateway = field("gateway=").ok_or_else(|| format!("no gateway= in the ready line: {line}"))?;
        let (host, port) = split_gateway(&gateway)?;
        let ca = field("ca=").ok_or_else(|| format!("no ca= in the ready line: {line}"))?;
        let emu = Emulator {
            host,
            port,
            ca_file: OnceLock::new(),
            work_dir,
            log: Some(log),
            owned: Some(Owned { child: Mutex::new(child), stdin: Mutex::new(stdin), _stdout: Mutex::new(stdout) }),
        };
        let _ = emu.ca_file.set(PathBuf::from(ca));
        Ok(emu)
    }

    /// Use an emulator that is already running: `gateway` is `http://host:port` (the port azemu prints in its ready line,
    /// 4577 by default; `host:port` also works). Nothing is started and nothing is stopped. The certificate authority is
    /// the file `AZEMU_CA_FILE` names, else it is fetched from the emulator (`GET /_azemu/ca.pem`) when first needed.
    pub fn connect(gateway: &str) -> Result<Emulator, String> {
        let (host, port) = split_gateway(gateway)?;
        let emu = Emulator { host, port, ca_file: OnceLock::new(), work_dir: work_dir_for_this_program(), log: None, owned: None };
        if let Ok(ca) = std::env::var("AZEMU_CA_FILE") {
            if !ca.trim().is_empty() {
                let _ = emu.ca_file.set(PathBuf::from(ca.trim()));
            }
        }
        let health = emu
            .try_request("GET", "/_azemu/health", "")
            .map_err(|e| format!("no emulator answers at {gateway} ({e}); start one, or unset AZEMU_GATEWAY"))?;
        if health.status != 200 {
            return Err(format!("{gateway} answered /_azemu/health with {}: is that azemu?", health.status));
        }
        Ok(emu)
    }

    /// The port of the gateway (and of every service, in the host-name form).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>` (or the host the emulator was reached by): the address of the admin API (`/_azemu/...`),
    /// which every port serves.
    pub fn gateway(&self) -> String {
        format!("http://{}:{}", url_host(&self.host), self.port)
    }

    /// The host name of a service in the host-name form, with the port: `myacct.blob.core.localhost:4577`. This is what
    /// Event Hubs wants as its "fully qualified namespace" (`namespace_host(Service::EventHubs, "myns")`).
    pub fn host_name(&self, service: Service, name: &str) -> String {
        format!("{name}.{}:{}", service.host_suffix(), self.port)
    }

    /// The address of a service in the host-name form: `https://myacct.blob.core.localhost:4577/`. The name is the storage
    /// account, vault, store, Cosmos account, topic or namespace. Every port answers https and http. The names under
    /// `.localhost` resolve to the local machine on most systems; where they do not (inside many containers), use
    /// [`Emulator::standard_endpoint`].
    pub fn endpoint(&self, service: Service, name: &str) -> String {
        format!("https://{}/", self.host_name(service, name))
    }

    /// [`Emulator::endpoint`] with `http`, for the clients that may use it (account keys, SAS, Table).
    pub fn http_endpoint(&self, service: Service, name: &str) -> String {
        format!("http://{}/", self.host_name(service, name))
    }

    /// The address of a service in the standard-port form, for an emulator started with `--standard-ports` (the container
    /// image is): `http://localhost:10000/myacct` for Blob, Queue, Table and Data Lake, which take the account from the
    /// path. Key Vault, App Configuration, Event Grid and Cosmos DB (and the namespace of Service Bus and Event Hubs) take
    /// it from the host name, so the address is `https://<host>:<port>/` for the account the host stands for, and `name`
    /// is not used. The host is the one the gateway was reached by. The ports are the ones `--standard-ports` opens, which
    /// only an emulator started that way (the container image) has.
    pub fn standard_endpoint(&self, service: Service, name: &str) -> String {
        let host = url_host(&self.host);
        let port = service.standard_port();
        match service {
            s if s.account_in_path() => format!("http://{host}:{port}/{name}"),
            Service::ServiceBus | Service::EventHubs => format!("sb://{host}:{port}/"),
            _ => format!("https://{host}:{port}/"),
        }
    }

    /// The address `ManagedIdentityCredential` takes as `IDENTITY_ENDPOINT` (with `IDENTITY_HEADER=azemu`): the
    /// emulator's token endpoint, which gives a token to every caller.
    pub fn identity_endpoint(&self) -> String {
        format!("http://localhost:{}/_azemu/identity/msi/token", self.port)
    }

    /// The file with the emulator's certificate authority (`ca.pem`), which a client has to trust for https. For an emulator
    /// that was already running and was given no `AZEMU_CA_FILE`, the first call fetches it from the emulator.
    ///
    /// Panics when it cannot be had.
    pub fn ca_file(&self) -> &Path {
        self.ca_file.get_or_init(|| {
            let answer = self
                .try_request("GET", "/_azemu/ca.pem", "")
                .unwrap_or_else(|e| panic!("azemu-testkit: cannot fetch the CA from {}: {e}", self.gateway()));
            assert!(
                answer.status == 200 && answer.body.contains("BEGIN CERTIFICATE"),
                "azemu-testkit: /_azemu/ca.pem answered {}: {}",
                answer.status,
                answer.body
            );
            let path = self.work_dir.join(format!("ca-{}.pem", self.port));
            write_atomically(&path, answer.body.as_bytes())
                .unwrap_or_else(|e| panic!("azemu-testkit: cannot write {}: {e}", path.display()));
            path
        })
    }

    /// The certificate authority in PEM form, for a client that takes the certificate as bytes (for example
    /// `reqwest::Certificate::from_pem`).
    pub fn ca_pem(&self) -> String {
        std::fs::read_to_string(self.ca_file()).unwrap_or_else(|e| panic!("azemu-testkit: cannot read {}: {e}", self.ca_file().display()))
    }

    /// A PEM file with the system's certificate list and the emulator's authority, for the programs that read one list
    /// and no more (`SSL_CERT_FILE`): the system list alone would not trust the emulator, and the emulator's authority
    /// alone would not trust anything else. Written next to the other files on the first call.
    pub fn ca_bundle(&self) -> PathBuf {
        let path = self.work_dir.join(format!("bundle-{}.pem", self.port));
        let mut bundle = String::new();
        for system in ["/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt", "/etc/ssl/cert.pem"] {
            if let Ok(list) = std::fs::read_to_string(system) {
                bundle.push_str(&list);
                bundle.push('\n');
                break;
            }
        }
        bundle.push_str(&self.ca_pem());
        write_atomically(&path, bundle.as_bytes()).unwrap_or_else(|e| panic!("azemu-testkit: cannot write {}: {e}", path.display()));
        path
    }

    /// Set the environment variables that make the Azure SDKs for Rust work against the emulator, in this process. Call it
    /// before any client is built ([`shared`] does). A variable that is already set is not changed, except `SSL_CERT_FILE`.
    ///
    /// * `SSL_CERT_FILE`: [`Emulator::ca_bundle`]. The Azure SDK's https clients (and Cosmos DB and Event Hubs) read the
    ///   trust list from it; without it a client does not trust the emulator, and the SDK retries the failure for over a
    ///   minute before it reports it.
    /// * `IDENTITY_ENDPOINT` and `IDENTITY_HEADER`: `ManagedIdentityCredential` then gets its tokens from the emulator.
    /// * `COSMOS_DISABLE_IMDS=1`: the Cosmos client does not wait two seconds for Azure's instance metadata service.
    /// * `AZURE_COSMOS_BINARY_ENCODING_ENABLED=false`: the Cosmos client sends and reads JSON, which is all azemu speaks.
    ///
    /// This changes the environment of the whole process, which is not safe to do while another thread reads it; call it
    /// at the start of the test program (the first call of [`shared`]).
    pub fn apply_environment(&self) {
        std::env::set_var("SSL_CERT_FILE", self.ca_bundle());
        for (name, value) in [
            ("IDENTITY_ENDPOINT", self.identity_endpoint()),
            ("IDENTITY_HEADER", "azemu".to_string()),
            ("COSMOS_DISABLE_IMDS", "1".to_string()),
            ("AZURE_COSMOS_BINARY_ENCODING_ENABLED", "false".to_string()),
        ] {
            if std::env::var_os(name).is_none() {
                std::env::set_var(name, value);
            }
        }
    }

    /// Where the emulator wrote its log, for an emulator this crate started.
    pub fn log_file(&self) -> Option<&Path> {
        self.log.as_deref()
    }

    /// Empty everything one account holds (a storage account, a vault, a Cosmos account, a namespace, ...), in every
    /// service that keeps state per account. Other accounts are not touched, so tests that each use an account of their own
    /// ([`unique_name`]) run side by side. Panics when the emulator does not answer 200.
    pub fn reset_account(&self, account: &str) {
        let answer = self.request("POST", &format!("/_azemu/reset?account={account}"), "");
        assert_eq!(answer.status, 200, "azemu-testkit: resetting {account}: {}", answer.body);
    }

    /// Empty everything, every account of every service. Panics when the emulator does not answer 200.
    pub fn reset_all(&self) {
        let answer = self.request("POST", "/_azemu/reset", "");
        assert_eq!(answer.status, 200, "azemu-testkit: reset: {}", answer.body);
    }

    /// Make an event hub (Azure makes them through Resource Manager, and the Rust SDK has no client for that). `namespace`
    /// is the first label of the host name the client uses.
    pub fn create_event_hub(&self, namespace: &str, hub: &str, partitions: u32) {
        let body = format!("{{\"partitionCount\":{partitions}}}");
        let answer = self.request("PUT", &format!("/_azemu/eventhubs/{namespace}/hubs/{hub}"), &body);
        assert!(
            answer.status == 200 || answer.status == 201,
            "azemu-testkit: creating event hub {namespace}/{hub}: {} {}",
            answer.status,
            answer.body
        );
    }

    /// Make a Service Bus queue, with the ATOM request an administration client sends (there is none for Rust).
    pub fn create_service_bus_queue(&self, namespace: &str, queue: &str) {
        let body = "<entry xmlns=\"http://www.w3.org/2005/Atom\"><content type=\"application/xml\"><QueueDescription xmlns=\"http://schemas.microsoft.com/netservices/2010/10/servicebus/connect\"/></content></entry>";
        let host = self.host_name(Service::ServiceBus, namespace);
        let answer = self
            .send(
                "PUT",
                &host,
                &format!("/{queue}?api-version=2021-05"),
                &[("Content-Type", "application/atom+xml;type=entry;charset=utf-8"), ("Authorization", "Bearer azemu-testkit")],
                body,
            )
            .unwrap_or_else(|e| panic!("azemu-testkit: creating Service Bus queue {namespace}/{queue}: {e}"));
        assert!(
            answer.status == 200 || answer.status == 201,
            "azemu-testkit: creating Service Bus queue {namespace}/{queue}: {} {}",
            answer.status,
            answer.body
        );
    }

    /// A request to a service, with the Host header of the host-name form (no name has to resolve: the connection goes to
    /// the gateway). For a quick check or a call the SDK cannot make; no credential is added. Panics when the emulator
    /// cannot be reached.
    pub fn call(&self, service: Service, name: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Answer {
        self.send(method, &self.host_name(service, name), path, headers, body)
            .unwrap_or_else(|e| panic!("azemu-testkit: {method} {path}: {e}"))
    }

    /// A request to the emulator's own API (`/_azemu/...`). Panics when the emulator cannot be reached.
    pub fn request(&self, method: &str, path: &str, body: &str) -> Answer {
        self.try_request(method, path, body).unwrap_or_else(|e| panic!("azemu-testkit: {method} {path}: {e}"))
    }

    fn try_request(&self, method: &str, path: &str, body: &str) -> Result<Answer, String> {
        let host = format!("{}:{}", url_host(&self.host), self.port);
        self.send(method, &host, path, &[("Content-Type", "application/json")], body)
    }

    /// One HTTP/1.1 request on a plain socket to the emulator, with a Host header of the caller's choice.
    fn send(&self, method: &str, host_header: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Result<Answer, String> {
        let mut socket = std::net::TcpStream::connect((self.host.as_str(), self.port))
            .map_err(|e| format!("cannot connect to {}:{}: {e}", self.host, self.port))?;
        socket.set_read_timeout(Some(Duration::from_secs(30))).map_err(|e| e.to_string())?;
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host_header}\r\n");
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
        socket.write_all(head.as_bytes()).and_then(|_| socket.write_all(body.as_bytes())).map_err(|e| format!("cannot send: {e}"))?;
        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).map_err(|e| format!("cannot read the answer: {e}"))?;
        parse_answer(&raw)
    }
}

impl Drop for Emulator {
    fn drop(&mut self) {
        if let Some(owned) = &self.owned {
            // closing the pipe asks it to stop and save; the kill is for one that does not
            drop(owned.stdin.lock().map(|mut s| s.take()));
            if let Ok(mut child) = owned.child.lock() {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                if matches!(child.try_wait(), Ok(None)) {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
    }
}

/// `gateway=http://127.0.0.1:4577`, `localhost:4577` or `http://[::1]:4577/` to host and port.
fn split_gateway(gateway: &str) -> Result<(String, u16), String> {
    let rest = gateway.trim().trim_start_matches("http://").trim_start_matches("https://").trim_end_matches('/');
    let (host, port) = rest.rsplit_once(':').ok_or_else(|| format!("`{gateway}` has no port; give it as http://host:port"))?;
    let port: u16 = port.parse().map_err(|_| format!("`{gateway}`: `{port}` is not a port number"))?;
    Ok((host.trim_start_matches('[').trim_end_matches(']').to_string(), port))
}

/// A host as it is written in a URL (an IPv6 address in brackets).
fn url_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Where this test program keeps its files: `target/tmp/azemu-testkit-<program>` when it runs from a cargo `target`
/// directory (`cargo clean` then removes them), else a directory of the same name in the temporary directory.
fn work_dir_for_this_program() -> PathBuf {
    let base = match std::env::var_os("AZEMU_TESTKIT_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => {
            let from_target = std::env::current_exe().ok().and_then(|exe| {
                // target/<profile>/deps/<program>, or target/<triple>/<profile>/deps/<program>
                let deps = exe.parent()?;
                (deps.file_name()? == "deps").then_some(())?;
                let target = deps.ancestors().find(|p| p.file_name().is_some_and(|n| n == "target"))?;
                Some(target.join("tmp"))
            });
            from_target.unwrap_or_else(std::env::temp_dir)
        }
    };
    let program = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "tests".to_string());
    base.join(format!("azemu-testkit-{program}"))
}

/// Create the kit's own directory so that only this user can enter it: it holds the key of the emulator's certificate
/// authority, and `azemu` warns about a certificate directory that others can read. A directory left by an earlier
/// run is narrowed too, since nothing but the kit puts files there.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Write a file so that a program that reads it at the same moment sees all of it or the old one.
fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// Status and body of an HTTP/1.1 answer, `Content-Length` and chunked bodies both.
fn parse_answer(raw: &[u8]) -> Result<Answer, String> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("the answer has no end of headers")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = &raw[split + 4..];
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("no status in `{}`", head.lines().next().unwrap_or("")))?;
    let chunked = head.lines().any(|l| l.to_ascii_lowercase().replace(' ', "").starts_with("transfer-encoding:chunked"));
    let body = if chunked { dechunk(body)? } else { body.to_vec() };
    Ok(Answer { status, body: String::from_utf8_lossy(&body).into_owned() })
}

fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = rest.windows(2).position(|w| w == b"\r\n").ok_or("a chunked body ends early")?;
        let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..line_end]).split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "a chunk size is not a number")?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size + 2 {
            return Err("a chunked body ends early".to_string());
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateways_are_split_into_host_and_port() {
        assert_eq!(split_gateway("http://127.0.0.1:4577").unwrap(), ("127.0.0.1".to_string(), 4577));
        assert_eq!(split_gateway("azemu:4577/").unwrap(), ("azemu".to_string(), 4577));
        assert_eq!(split_gateway("http://[::1]:99").unwrap(), ("::1".to_string(), 99));
        assert!(split_gateway("localhost").is_err());
        assert!(split_gateway("localhost:http").is_err());
    }

    #[test]
    fn answers_with_a_length_or_in_chunks_are_read() {
        let a = parse_answer(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok").unwrap();
        assert_eq!((a.status, a.body.as_str()), (201, "ok"));
        let a = parse_answer(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=1\r\nde\r\n0\r\n\r\n").unwrap();
        assert_eq!((a.status, a.body.as_str()), (200, "abcde"));
        assert!(parse_answer(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nabc").is_err());
    }

    #[test]
    fn unique_names_are_valid_for_every_service_and_do_not_repeat() {
        let names: Vec<String> = (0..2000).map(|_| unique_name("Acct-1")).collect();
        let distinct: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(distinct.len(), names.len());
        for n in &names {
            assert!((3..=24).contains(&n.len()), "{n}");
            assert!(n.starts_with("acct1") && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()), "{n}");
        }
        assert!((3..=24).contains(&unique_name("").len()));
        assert!(unique_name("averyverylongprefix").len() <= 24);
    }
}
