//! The shared `main`.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::persistence::DEFAULT_SAVE_INTERVAL_SECS;
use crate::server::{Emulator, RunningEmulator};
use crate::service::{RawProtocol, Service};

/// Local Azure emulator.
#[derive(Parser, Debug)]
#[command(name = "azemu", version = crate::BUILD_VERSION_LINE, about = "Local Azure emulator")]
struct Cli {
    /// Gateway port. 0 picks any free port, and it is the same port on every address azemu listens on (127.0.0.1 and
    /// ::1 by default); the ready line and GET /_azemu/config name it.
    #[arg(long, default_value_t = 4577)]
    port: u16,

    /// Address to listen on (repeatable). Default: 127.0.0.1 and ::1
    #[arg(long = "host", value_name = "IP")]
    hosts: Vec<IpAddr>,

    /// Pinned listener for one service, for example blob=10000 (repeatable). Port 0 picks any free port, the same on
    /// every address; GET /_azemu/config names it.
    #[arg(long = "pin", value_name = "SERVICE=PORT")]
    pins: Vec<String>,

    /// One fixed port per service next to the gateway port, so that any host name works and no name under
    /// .localhost has to resolve (inside a container, for example). On such a port the port alone chooses the
    /// service: 10000 blob (http://azemu:10000/{account}/...), 10001 queue, 10002 table, 10003 data lake,
    /// 10010 key vault, 10011 app configuration, 10012 event grid, 10013 ARM, 10014 identity, 8081 cosmos,
    /// 5672 AMQP and 5671 AMQP over TLS (Service Bus; Event Hubs when the host name has the label eventhub), 9093
    /// Kafka, and 443 for ARM (where the Azure SDKs dial it with no port: the management API, and AMQP over
    /// WebSocket, which any port serves). Port 443 needs permission to listen below 1024: when the system
    /// refuses it, a warning says so and everything else starts. Key Vault,
    /// App Configuration and Event Grid serve one default vault / store / topic on their port. For https from
    /// another container also pass --tls-name <the name the clients use> (--tls-name azemu). A service you pin
    /// yourself with --pin keeps your port. Also AZEMU_STANDARD_PORTS=1.
    #[arg(long)]
    standard_ports: bool,

    /// Turn on the conveniences Azure does not have: queues, topics, event hubs, consumer groups and Event Grid
    /// topics that a client names but nobody created come into being on first use, and new storage accounts keep
    /// blob versions. Also AZEMU_LENIENT=1; a convenience's own variable still overrides it.
    #[arg(long)]
    lenient: bool,

    /// Event Grid webhooks may also go to this host name (repeatable, or a comma separated list in
    /// AZEMU_ALLOW_WEBHOOK_HOST; `*` allows any host).
    #[arg(long = "allow-webhook-host", value_name = "NAME", value_delimiter = ',')]
    allow_webhook_hosts: Vec<String>,

    /// Where ca.pem and ca.key live (default ~/.azemu)
    #[arg(long, value_name = "PATH")]
    cert_dir: Option<PathBuf>,

    /// Extra name the local CA may sign for, besides localhost, *.localhost and loopback addresses:
    /// a DNS suffix (azemu allows azemu and *.azemu), an IP address or a range (10.0.0.0/24).
    /// Repeatable, or a comma separated list in AZEMU_TLS_NAME. A different set makes a new CA.
    #[arg(long = "tls-name", value_name = "NAME", value_delimiter = ',')]
    tls_names: Vec<String>,

    /// Also serve requests whose Host is this name or a name under it (repeatable, or a comma separated list in
    /// AZEMU_ALLOW_HOST). Served without an option: IP addresses, localhost and *.localhost, names with no dot
    /// (Docker Compose service names), host.docker.internal, and the names given with --tls-name. Needed when a
    /// hosts file maps real Azure names to the emulator: --allow-host core.windows.net
    #[arg(long = "allow-host", value_name = "NAME", value_delimiter = ',')]
    allow_hosts: Vec<String>,

    /// Let web pages from this origin use the emulator although they are on another site: an origin
    /// (http://localhost:3000), a host (app.example.com, *.example.com), or * to turn the cross-site check off
    /// (repeatable, or a comma separated list in AZEMU_ALLOW_ORIGIN). Programs that are not browsers need nothing.
    #[arg(long = "allow-origin", value_name = "ORIGIN", value_delimiter = ',')]
    allow_origins: Vec<String>,

    /// Keep state between runs: read each service's state from this directory at start and write it back while
    /// running and at shutdown (also AZEMU_DATA_DIR). Created when missing; locked while azemu runs. The files
    /// can hold secrets (Key Vault private keys), so the directory is private to your user (mode 0700).
    #[arg(long, value_name = "PATH")]
    data_dir: Option<PathBuf>,

    /// Seconds between saves of changed state when --data-dir is set (also AZEMU_SAVE_INTERVAL). 0: save only on
    /// POST /_azemu/save and at shutdown.
    #[arg(long, value_name = "SECONDS")]
    save_interval: Option<u64>,

    /// When saved state cannot be read, move that service's directory aside (<service>.unreadable-<time>) and start
    /// the service empty, instead of refusing to start.
    #[arg(long)]
    data_discard_unreadable: bool,

    /// error | warn | info | debug | trace
    #[arg(long, default_value = "info", value_name = "LEVEL")]
    log_level: String,

    /// Stop (with the final save) when standard input reaches its end. A test program that starts azemu with a pipe on
    /// its standard input ends it by exiting, however it exits; without this option azemu keeps running until it gets
    /// a signal.
    #[arg(long)]
    exit_on_stdin_close: bool,
}

/// The fixed port of every service with `--standard-ports`. A raw protocol is served on any port; the service named here
/// only decides what an HTTP request on that port means (`servicebus` and `kafka` have no HTTP side worth using).
pub(crate) const STANDARD_PORTS: &[(&str, u16)] = &[
    ("blob", 10000),
    ("queue", 10001),
    ("table", 10002),
    ("dfs", 10003),
    ("keyvault", 10010),
    ("appconfig", 10011),
    ("eventgrid", 10012),
    ("arm", 10013),
    ("identity", 10014),
    ("cosmos", 8081),
    ("servicebus", 5672),
    // The Azure SDKs dial 5671 for AMQP over TLS when a client uses an identity (the host name has no port), so the same
    // listener is opened there too. Every listener sniffs TLS and plain AMQP, so 5671 and 5672 behave alike.
    ("servicebus", 5671),
    ("kafka", 9093),
];

/// Standard ports that are tried but may fail: below 1024, a port needs privilege on most systems (a developer laptop refuses
/// it; a container usually does not). A failure is a warning and the emulator starts without it. Port 443 is where the Azure SDKs
/// dial the management API (`management.azure.com`) and AMQP over WebSocket (Service Bus, Event Hubs) when they are given a
/// token credential and no way to name a port. WebSocket upgrades are accepted on every listener, so the pin to `arm` is for
/// the HTTP side.
pub(crate) const OPTIONAL_STANDARD_PORTS: &[(&str, u16)] = &[("arm", 443)];

/// The optional standard pins (443) for the services that are mounted. Left out when the user already uses the port (`--port 443`
/// or a pin on it) or pinned that service to a port of their own.
fn optional_standard_pins(
    mounted: &[&str],
    gateway_port: u16,
    user_pins: &[(String, u16)],
    required: &[(String, u16)],
) -> Vec<(String, u16)> {
    let mut out = Vec::new();
    for (name, port) in OPTIONAL_STANDARD_PORTS {
        let used = *port == gateway_port || user_pins.iter().chain(required.iter()).any(|(_, p)| p == port);
        let own_port = user_pins.iter().any(|(n, _)| n == name);
        if mounted.contains(name) && !used && !own_port {
            out.push((name.to_string(), *port));
        }
    }
    out
}

/// The standard pins for the services that are mounted: `eventhubs` takes the AMQP ports (5672 and 5671) when there is no
/// Service Bus.
fn standard_pins(mounted: &[&str], already_pinned: &[(String, u16)]) -> Vec<(String, u16)> {
    let mut out = Vec::new();
    for (name, port) in STANDARD_PORTS {
        let name =
            if *name == "servicebus" && !mounted.contains(&"servicebus") && mounted.contains(&"eventhubs") { "eventhubs" } else { name };
        if mounted.contains(&name) && !already_pinned.iter().any(|(n, _)| n == name) {
            out.push((name.to_string(), *port));
        }
    }
    out
}

/// What the user asked to listen on, so that a failure can say which option asked for the port that failed.
#[derive(Debug, Default)]
struct ListenerPlan {
    gateway_port: u16,
    /// `--pin <service>=<port>`
    user_pins: Vec<(String, u16)>,
    /// the pins that `--standard-ports` added
    standard_pins: Vec<(String, u16)>,
}

impl ListenerPlan {
    /// Which option opened `port` and what to change (the gateway first, then the user's pins, then the standard ports).
    fn asked_by(&self, port: u16) -> Option<(String, String)> {
        if port == self.gateway_port {
            return Some(("It is the gateway port (--port, default 4577).".into(), "choose another with --port".into()));
        }
        if let Some((svc, _)) = self.user_pins.iter().find(|(_, p)| *p == port) {
            return Some((format!("It is the port you gave with --pin {svc}={port}."), format!("choose another with --pin {svc}=<port>")));
        }
        if let Some((svc, _)) = self.standard_pins.iter().find(|(_, p)| *p == port) {
            return Some((
                format!("It is the standard port of {svc} that --standard-ports (or AZEMU_STANDARD_PORTS=1) opens."),
                format!("move that one service with --pin {svc}=<port>"),
            ));
        }
        None
    }
}

/// The address and the kind of error when `e` is a failure to listen (core's `start` says `binding <address>` and keeps the
/// operating system's error underneath).
fn bind_failure(e: &anyhow::Error) -> Option<(SocketAddr, std::io::ErrorKind)> {
    let addr = e.chain().find_map(|c| c.to_string().strip_prefix("binding ").and_then(|a| a.trim().parse::<SocketAddr>().ok()))?;
    let kind = e.chain().find_map(|c| c.downcast_ref::<std::io::Error>()).map(|io| io.kind())?;
    Some((addr, kind))
}

/// Say what a start failure is. Only a failure to listen is about ports; every other failure (a certificate directory or a
/// data directory that cannot be written, a data directory another azemu holds, saved state that cannot be read, a bad option)
/// already carries a message that names its path or process, and is passed on as it is.
fn explain_start_error(e: anyhow::Error, plan: &ListenerPlan) -> anyhow::Error {
    let Some((addr, kind)) = bind_failure(&e) else { return e };
    use std::io::ErrorKind::*;
    let port = addr.port();
    let (which, fix) = plan.asked_by(port).unwrap_or_default();
    let fix = if fix.is_empty() { String::new() } else { format!(", or {fix}") };
    let text = match kind {
        AddrInUse => format!("cannot listen on {addr}: the port is already in use by another program. {which} Stop that program{fix}.").replace("  ", " "),
        PermissionDenied if port < 1024 => {
            format!("cannot listen on {addr}: the system lets only a privileged user listen on a port below 1024. {which} Use a port of 1024 or more{fix}.").replace("  ", " ")
        }
        PermissionDenied => format!("cannot listen on {addr}: the system refused it (permission denied). {which}").trim_end().to_string(),
        AddrNotAvailable => format!("cannot listen on {addr}: the address does not belong to this machine. Check the --host options."),
        _ => format!("cannot listen on {addr}. {which}").trim_end().to_string(),
    };
    e.context(text)
}

/// Port 443 and the like (`OPTIONAL_STANDARD_PORTS`): try each host once. Returns the hosts where the port can be bound, and
/// a warning text when it can be bound on none of them. `bind` is the real bind in `run_cli`; a test passes its own.
fn open_optional_port(
    hosts: &[IpAddr],
    port: u16,
    service: &str,
    bind: impl Fn(SocketAddr) -> std::io::Result<()>,
) -> (Vec<IpAddr>, Option<String>) {
    let mut ok = Vec::new();
    let mut first_error: Option<(IpAddr, std::io::Error)> = None;
    for ip in hosts {
        match bind(SocketAddr::new(*ip, port)) {
            Ok(()) => ok.push(*ip),
            Err(e) if first_error.is_none() => first_error = Some((*ip, e)),
            Err(_) => {}
        }
    }
    if !ok.is_empty() {
        return (ok, None);
    }
    let Some((ip, e)) = first_error else { return (ok, None) };
    let why = match e.kind() {
        std::io::ErrorKind::PermissionDenied => format!(
            "the system lets only a privileged user listen on a port below 1024. The Azure SDKs use port {port} for the management API and for AMQP over WebSocket (Service Bus, Event Hubs) when a client gives no port, so those clients will not find azemu. \
             In a container this works as it is (Docker lets any user listen on {port}). On Linux outside one, either give azemu the right once (`sudo setcap cap_net_bind_service=+ep <path of azemu>`), \
             or lower the limit for every program (`sudo sysctl net.ipv4.ip_unprivileged_port_start={port}`), or run azemu as root; or start it on the ports above 1024 and give the client a port"
        ),
        std::io::ErrorKind::AddrInUse => format!("another program already listens on port {port}, so the {service} service has no port {port} here (its standard port above 1024 still works)"),
        _ => format!("{e}"),
    };
    (ok, Some(format!("--standard-ports could not open port {port} on {ip}: {why}. azemu starts without it.")))
}

/// The certificate directory when the user named none (`~/.azemu`; the same rule as core's builder).
fn default_cert_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h).join(".azemu"),
        _ => PathBuf::from(".azemu"),
    }
}

/// Can azemu write in `dir` (creating it when needed)?
fn dir_is_writable(dir: &std::path::Path) -> std::io::Result<()> {
    crate::private_dir::create_private_dir(dir)?;
    let probe = dir.join(format!(".write-test-{}", std::process::id()));
    std::fs::write(&probe, b"x")?;
    std::fs::remove_file(&probe)
}

fn ca_files_exist(dir: &std::path::Path) -> bool {
    ["ca.pem", "ca.issuing.pem", "ca.key", "ca.meta"].iter().all(|f| dir.join(f).is_file())
}

/// A new private directory (mode 0700, named for this process and the time, created and not reused) for a CA that is not kept.
fn make_private_temp_dir(base: &std::path::Path) -> std::io::Result<PathBuf> {
    let mut attempt = 0u32;
    loop {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let dir = base.join(format!("azemu-ca-{}-{nanos:x}{attempt}", std::process::id()));
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        match b.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 20 => attempt += 1,
            Err(e) => return Err(e),
        }
    }
}

/// A certificate directory that azemu made for this run and removes at the end.
struct TempCertDir(PathBuf);

impl Drop for TempCertDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What to do about the certificate directory.
#[derive(Debug, PartialEq)]
enum CertDirChoice {
    /// Use what the builder uses: the directory the user named, or `~/.azemu`.
    AsConfigured,
    /// The default directory cannot be written and holds no usable CA: make a private temporary one.
    Temporary { default_dir: PathBuf, error: String },
}

/// Only the default directory is replaced: a directory the user named with `--cert-dir` is their choice, and an error about it
/// names it. A default directory that cannot be written is still used when it already holds all four CA files (a read-only
/// volume with a CA in it works as before; if that CA needs renewing the start fails with the path of the file).
fn choose_cert_dir(explicit: Option<&std::path::Path>, default_dir: &std::path::Path) -> CertDirChoice {
    if explicit.is_some() {
        return CertDirChoice::AsConfigured;
    }
    match dir_is_writable(default_dir) {
        Ok(()) => CertDirChoice::AsConfigured,
        Err(_) if ca_files_exist(default_dir) => CertDirChoice::AsConfigured,
        Err(e) => CertDirChoice::Temporary { default_dir: default_dir.to_path_buf(), error: e.to_string() },
    }
}

/// The log goes to stderr. `level` applies to azemu's own crates (their target starts with `azemu`); the libraries
/// underneath (HTTP/2 in particular, which at trace level writes the path and query of every request, `sig` included)
/// never log more than `info`, so no level can bring a credential into the log through them.
fn init_logging(level: tracing::Level) {
    init_logging_with(level, std::io::stderr, std::io::IsTerminal::is_terminal(&std::io::stderr()));
}

/// [`init_logging`] for a writer of the caller's choice (the log test of core reads the output back). Returns false
/// when a logger was already installed in this process.
#[doc(hidden)]
pub fn init_logging_with<W>(level: tracing::Level, writer: W, ansi: bool) -> bool
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;
    let layer = tracing_subscriber::fmt::layer().with_writer(writer).with_target(false).with_ansi(ansi).with_filter(
        tracing_subscriber::filter::filter_fn(move |meta| {
            *meta.level() <= level && (meta.target().starts_with("azemu") || *meta.level() <= tracing::Level::INFO)
        }),
    );
    tracing_subscriber::registry().with(layer).try_init().is_ok()
}

fn parse_level(s: &str) -> Result<tracing::Level> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "error" => tracing::Level::ERROR,
        "warn" | "warning" => tracing::Level::WARN,
        "info" => tracing::Level::INFO,
        "debug" => tracing::Level::DEBUG,
        "trace" => tracing::Level::TRACE,
        other => bail!("invalid --log-level '{other}' (use error, warn, info, debug or trace)"),
    })
}

fn parse_pin(s: &str) -> Result<(String, u16)> {
    let (name, port) = s.split_once('=').with_context(|| format!("invalid --pin '{s}' (use SERVICE=PORT)"))?;
    let port: u16 = port.parse().with_context(|| format!("invalid port in --pin '{s}'"))?;
    if name.is_empty() {
        bail!("invalid --pin '{s}' (empty service name)");
    }
    Ok((name.to_string(), port))
}

/// `azemu __healthcheck [PORT]` (hidden): asks the emulator on this machine, on the gateway port (default 4577), for
/// `GET /_azemu/health`, prints one line and exits with 0 when the answer is 200, 1 otherwise. It is what the container's
/// `HEALTHCHECK` runs, so the image needs no curl. Does nothing when the first argument is anything else.
pub fn health_check_if_requested() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("__healthcheck") {
        return;
    }
    let port: u16 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(4577);
    match health_check(port) {
        Ok(()) => {
            println!("ok: azemu answers on port {port}");
            std::process::exit(0);
        }
        Err(e) => {
            println!("unhealthy: {e}");
            std::process::exit(1);
        }
    }
}

/// One plain HTTP request over a loopback address, with short time limits: no TLS, no client library.
fn health_check(port: u16) -> std::result::Result<(), String> {
    use std::io::Read;
    let timeout = Duration::from_secs(3);
    let mut last = String::from("no address tried");
    'addresses: for ip in [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)] {
        let addr = SocketAddr::new(ip, port);
        let mut s = match std::net::TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => s,
            Err(e) => {
                last = format!("cannot connect to {addr}: {e}");
                continue;
            }
        };
        let _ = s.set_read_timeout(Some(timeout));
        let _ = s.set_write_timeout(Some(timeout));
        let request = format!("GET /_azemu/health HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
        if let Err(e) = s.write_all(request.as_bytes()) {
            last = format!("cannot send to {addr}: {e}");
            continue;
        }
        let mut head = [0u8; 64];
        let mut got = 0;
        while got < 12 {
            match s.read(&mut head[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) => {
                    last = format!("no answer from {addr}: {e}");
                    continue 'addresses;
                }
            }
        }
        let status = String::from_utf8_lossy(&head[..got]);
        if status.starts_with("HTTP/1.1 200") {
            return Ok(());
        }
        last = format!("{addr} answered {:?}", status.lines().next().unwrap_or(""));
    }
    Err(last)
}

/// Shared `main` for `azemu` and every `azemu-<name>` binary.
///
/// Parses the command line, starts the listeners, prints the ready line on stdout and
/// runs until Ctrl-C or SIGTERM. Logs go to stderr.
pub async fn run_cli(services: Vec<Arc<dyn Service>>, raw: Vec<Arc<dyn RawProtocol>>) -> Result<()> {
    // `azemu __settings-table`: the table of settings that docs/SETTINGS.md holds (hidden; a test compares them)
    if std::env::args().nth(1).as_deref() == Some("__settings-table") {
        crate::settings::describe_core();
        print!("{}", crate::settings::table_markdown());
        return Ok(());
    }
    let cli = Cli::parse();
    crate::settings::describe_core();
    let level = parse_level(&cli.log_level)?;
    init_logging(level);

    let default_hosts = cli.hosts.is_empty();
    let hosts: Vec<IpAddr> =
        if default_hosts { vec![IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)] } else { cli.hosts.clone() };
    let mut pins = cli.pins.iter().map(|p| parse_pin(p)).collect::<Result<Vec<_>>>()?;
    let standard = cli.standard_ports || crate::settings::flag("AZEMU_STANDARD_PORTS", false);
    let mut plan = ListenerPlan { gateway_port: cli.port, user_pins: pins.clone(), standard_pins: Vec::new() };
    let mut optional_pins: Vec<(String, u16)> = Vec::new();
    if standard {
        let mounted: Vec<&str> = services.iter().map(|s| s.name()).collect();
        let extra = standard_pins(&mounted, &pins);
        plan.standard_pins = extra.clone();
        optional_pins = optional_standard_pins(&mounted, cli.port, &pins, &extra);
        pins.extend(extra);
    }

    let mut b = Emulator::builder();
    for s in services {
        b = b.service(s);
    }
    for r in raw {
        b = b.raw_protocol(r);
    }
    // The certificate directory: the one named with --cert-dir, else ~/.azemu. When the default cannot be written (an image
    // run with `--user <some id>`, a read-only home) the CA goes to a private temporary directory for this run.
    let mut temp_cert_dir: Option<TempCertDir> = None;
    if let Some(d) = cli.cert_dir.clone() {
        b = b.cert_dir(d);
    } else if let CertDirChoice::Temporary { default_dir, error } = choose_cert_dir(None, &default_cert_dir()) {
        let dir = make_private_temp_dir(&std::env::temp_dir()).with_context(|| {
            format!(
                "the certificate directory {} cannot be written ({error}) and no temporary directory could be made in {}; give --cert-dir a directory this user can write",
                default_dir.display(),
                std::env::temp_dir().display()
            )
        })?;
        tracing::warn!(
            "the certificate directory {} cannot be written ({error}); using the temporary directory {} for this run. The local CA is new at every start, so clients must fetch ca.pem again (GET /_azemu/ca.pem), and it is deleted when azemu stops. To keep one CA, give --cert-dir a directory this user can write",
            default_dir.display(),
            dir.display()
        );
        b = b.cert_dir(dir.clone());
        temp_cert_dir = Some(TempCertDir(dir));
    }
    // `--lenient` and `--allow-webhook-host` stand in for variables the services read when they are built, which
    // happened before this point. `main` applies them first (`settings::apply_early_options`); an embedder that did
    // not gets a warning instead of a silent difference.
    // read here so that `GET /_azemu/config` always shows it, whether or not a service reads it itself
    let lenient = crate::settings::lenient();
    if cli.lenient && !lenient {
        tracing::warn!("--lenient was given but this program built its services before reading the command line, so it has no effect; set AZEMU_LENIENT=1 instead");
    }
    if !cli.allow_webhook_hosts.is_empty() && crate::settings::list("AZEMU_ALLOW_WEBHOOK_HOST").is_empty() {
        tracing::warn!("--allow-webhook-host was given but this program built its services before reading the command line, so it has no effect; set AZEMU_ALLOW_WEBHOOK_HOST instead");
    }
    // `--tls-name` (repeatable) and AZEMU_TLS_NAME (comma separated; for docker-compose files)
    let env_names = crate::settings::list("AZEMU_TLS_NAME");
    for n in cli.tls_names.iter().chain(env_names.iter()) {
        if !n.trim().is_empty() {
            b = b.tls_name(n.trim());
        }
    }
    // `--allow-host` and `--allow-origin`, with their environment variables (comma separated; for docker-compose files)
    let env_hosts = crate::settings::list("AZEMU_ALLOW_HOST");
    for n in cli.allow_hosts.iter().chain(env_hosts.iter()) {
        if !n.trim().is_empty() {
            b = b.allow_host(n.trim());
        }
    }
    let env_origins = crate::settings::list("AZEMU_ALLOW_ORIGIN");
    for n in cli.allow_origins.iter().chain(env_origins.iter()) {
        if !n.trim().is_empty() {
            b = b.allow_origin(n.trim());
        }
    }
    // `--data-dir` and `--save-interval`, with their environment variables
    let data_dir = cli.data_dir.clone().or_else(|| crate::settings::text("AZEMU_DATA_DIR").map(PathBuf::from));
    let interval_set = crate::settings::text("AZEMU_SAVE_INTERVAL").is_some();
    let env_interval = crate::settings::number("AZEMU_SAVE_INTERVAL", DEFAULT_SAVE_INTERVAL_SECS);
    if let Some(dir) = data_dir {
        b = b.data_dir(dir).discard_unreadable_data(cli.data_discard_unreadable);
        let secs = match cli.save_interval {
            Some(n) => Some(n),
            None => interval_set.then_some(env_interval),
        };
        if let Some(n) = secs {
            b = b.save_interval(std::time::Duration::from_secs(n));
        }
    } else if cli.save_interval.is_some() || cli.data_discard_unreadable {
        bail!("--save-interval and --data-discard-unreadable need --data-dir (or AZEMU_DATA_DIR)");
    }
    for (i, ip) in hosts.iter().enumerate() {
        // With the default host list only the first (IPv4) address is mandatory:
        // machines without IPv6 cannot bind ::1.
        let optional = default_hosts && i > 0;
        let gw = SocketAddr::new(*ip, cli.port);
        b = if optional { b.gateway_optional(gw) } else { b.gateway(gw) };
        for (name, port) in &pins {
            let a = SocketAddr::new(*ip, *port);
            b = if optional { b.pinned_optional(a, name) } else { b.pinned(a, name) };
        }
    }
    for (name, port) in &optional_pins {
        let (ok_hosts, warning) = open_optional_port(&hosts, *port, name, |a| std::net::TcpListener::bind(a).map(drop));
        if let Some(w) = warning {
            tracing::warn!("{w}");
        }
        for ip in ok_hosts {
            b = b.pinned_optional(SocketAddr::new(ip, *port), name);
        }
    }

    let running = b.start().await.map_err(|e| explain_start_error(e, &plan))?;
    // Settings are read when the services are built; a name nobody read is most likely a typing mistake. A setting
    // that a crate describes but reads later (on first use) is not reported.
    for name in crate::settings::unknown_variables() {
        tracing::warn!("{name} is set, but no part of azemu reads a setting of that name (a typing mistake? see docs/SETTINGS.md)");
    }
    tokio::spawn(warn_if_localhost_names_do_not_resolve(standard));
    announce(&running)?;
    if cli.exit_on_stdin_close {
        tokio::select! {
            _ = wait_for_signal() => {}
            _ = wait_for_stdin_end() => tracing::info!("standard input was closed"),
        }
    } else {
        wait_for_signal().await;
    }
    tracing::info!("shutting down (a second Ctrl-C or SIGTERM quits at once, without saving)");
    let result = tokio::select! {
        r = running.shutdown_checked() => r.context("the final save failed; the state on disk is from the last successful save"),
        _ = wait_for_signal() => {
            tracing::warn!("second signal: quitting at once");
            drop(temp_cert_dir);
            std::process::exit(1);
        }
    };
    drop(temp_cert_dir);
    if cli.exit_on_stdin_close {
        // the thread that waits on standard input cannot be stopped, and would keep the process from ending
        if let Err(e) = &result {
            eprintln!("Error: {e:#}");
        }
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
    result
}

/// The name the start-up check looks up. It has `.core.` like a real storage name.
const SELF_TEST_NAME: &str = "azemu-self-test.blob.core.localhost";

/// Does this name resolve, and only to loopback addresses? (Blocking: a lookup through the system resolver.)
fn resolves_to_loopback_only_blocking(name: &str) -> bool {
    use std::net::ToSocketAddrs;
    match (name, 0u16).to_socket_addrs() {
        Ok(addrs) => {
            let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
            !ips.is_empty() && ips.iter().all(|ip| ip.is_loopback())
        }
        Err(_) => false,
    }
}

/// The one warning, with the three ways out.
fn localhost_warning(name: &str, standard_ports: bool) -> String {
    let first = if standard_ports {
        "you started with --standard-ports, so use it: every service has a port of its own and any host name works (for example http://azemu:10000/devstoreaccount1 for Blob)"
    } else {
        "(1) start azemu with --standard-ports: every service then has a port of its own and any host name works, for example http://azemu:10000/devstoreaccount1 for Blob (the ports are in azemu --help)"
    };
    format!(
        "this system does not resolve names under .localhost (tried {name}), so addresses like http://myaccount.blob.core.localhost:4577 will fail from here. Ways out: {first}; (2) add a hosts-file line such as '127.0.0.1 myaccount.blob.core.localhost' for each name; (3) in Docker Compose, give the azemu container a network alias for each name. Programs on this same machine that resolve .localhost themselves (curl, browsers) are not affected."
    )
}

/// Names such as `acct.blob.core.localhost` need the system to resolve anything under `.localhost`. Linux desktops
/// do; containers, Windows and macOS before 26 often do not. Say so once, with the ways out.
async fn warn_if_localhost_names_do_not_resolve(standard_ports: bool) {
    let lookup = tokio::task::spawn_blocking(|| resolves_to_loopback_only_blocking(SELF_TEST_NAME));
    let loopback = matches!(tokio::time::timeout(Duration::from_secs(3), lookup).await, Ok(Ok(true)));
    if !loopback {
        tracing::warn!("{}", localhost_warning(SELF_TEST_NAME, standard_ports));
    }
}

fn announce(running: &RunningEmulator) -> Result<()> {
    let gw = running.gateway_addrs().first().map(|a| format!("http://{a}")).unwrap_or_default();
    let ca = std::fs::canonicalize(running.ca_path()).unwrap_or_else(|_| running.ca_path().to_path_buf());
    for name in running.service_names() {
        if let Some(a) = running.pinned_addr(name) {
            tracing::info!(service = name, addr = %a, "pinned");
        }
    }
    let line = format!(
        "azemu ready gateway={gw} ca={} services={} version={} commit={}",
        ca.display(),
        running.service_names().join(","),
        crate::VERSION,
        crate::GIT_COMMIT
    );
    tracing::info!(version = crate::VERSION, commit = crate::GIT_COMMIT, "azemu starting");
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}

/// Returns when standard input has reached its end (or cannot be read any more). The read waits on a thread of the
/// blocking pool that nothing can stop, so a caller that has used this ends the process with `exit`.
async fn wait_for_stdin_end() {
    use std::io::Read;
    let _ = tokio::task::spawn_blocking(|| {
        let mut buf = [0u8; 256];
        let mut stdin = std::io::stdin().lock();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    })
    .await;
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_ports_cover_the_mounted_services_and_leave_pins_alone() {
        let all = ["blob", "queue", "keyvault", "servicebus", "eventhubs", "kafka", "arm", "nothing-else"];
        let pins = standard_pins(&all, &[("queue".to_string(), 12345)]);
        let get = |n: &str| pins.iter().find(|(name, _)| name == n).map(|(_, p)| *p);
        assert_eq!((get("blob"), get("keyvault"), get("servicebus"), get("kafka")), (Some(10000), Some(10010), Some(5672), Some(9093)));
        // AMQP over TLS: the Azure SDKs dial 5671 when a client uses an identity (no port in the host name)
        let amqp: Vec<u16> = pins.iter().filter(|(n, _)| n == "servicebus").map(|(_, p)| *p).collect();
        assert_eq!(amqp, vec![5672, 5671]);
        assert_eq!(get("queue"), None, "a service the user pinned keeps the user's port");
        assert_eq!(get("table"), None, "a service that is not mounted gets none");
        // only Event Hubs mounted (its own binary): it takes the AMQP port
        assert_eq!(standard_pins(&["eventhubs"], &[]), vec![("eventhubs".to_string(), 5672), ("eventhubs".to_string(), 5671)]);
        // a Service Bus the user pinned keeps the user's port and gets no standard one at all
        let own = standard_pins(&["servicebus"], &[("servicebus".to_string(), 7000)]);
        assert!(own.is_empty(), "{own:?}");
        // no two services share a port
        let mut ports: Vec<u16> = STANDARD_PORTS.iter().map(|(_, p)| *p).collect();
        ports.sort();
        ports.dedup();
        assert_eq!(ports.len(), STANDARD_PORTS.len());
    }

    use std::io::{Error as IoError, ErrorKind};

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("azemu-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn plan() -> ListenerPlan {
        ListenerPlan {
            gateway_port: 4577,
            user_pins: vec![("blob".into(), 12000)],
            standard_pins: vec![("queue".into(), 10001), ("keyvault".into(), 10010)],
        }
    }

    /// A failure to listen, shaped as core's `start` reports it (`binding <address>` over the operating system's error).
    fn bind_error(addr: &str, kind: ErrorKind) -> anyhow::Error {
        anyhow::Error::new(IoError::from(kind)).context(format!("binding {addr}"))
    }

    #[test]
    fn a_port_in_use_names_the_port_and_the_option_that_asked_for_it() {
        let m = format!("{:#}", explain_start_error(bind_error("127.0.0.1:10010", ErrorKind::AddrInUse), &plan()));
        assert!(
            m.contains("127.0.0.1:10010") && m.contains("already in use") && m.contains("--standard-ports") && m.contains("keyvault"),
            "{m}"
        );
        let m = format!("{:#}", explain_start_error(bind_error("127.0.0.1:12000", ErrorKind::AddrInUse), &plan()));
        assert!(m.contains("--pin blob=12000") && !m.contains("--standard-ports"), "{m}");
        let m = format!("{:#}", explain_start_error(bind_error("[::1]:4577", ErrorKind::AddrInUse), &plan()));
        assert!(m.contains("--port") && m.contains("4577") && !m.contains("--standard-ports"), "{m}");
        // a port no option of ours asked for is still named
        let m = format!("{:#}", explain_start_error(bind_error("127.0.0.1:9", ErrorKind::AddrInUse), &plan()));
        assert!(m.contains("127.0.0.1:9") && m.contains("already in use"), "{m}");
    }

    #[test]
    fn a_port_below_1024_that_is_refused_says_so() {
        let m = format!("{:#}", explain_start_error(bind_error("127.0.0.1:443", ErrorKind::PermissionDenied), &plan()));
        assert!(m.contains("443") && m.contains("below 1024") && !m.contains("already in use"), "{m}");
    }

    #[test]
    fn a_real_port_in_use_is_explained_through_core() {
        // the real error of core's `start`, so that a change of its wording breaks this test and not the user's message
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let dir = scratch("inuse");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let err = rt
            .block_on(Emulator::builder().cert_dir(dir.join("ca")).gateway(SocketAddr::from(([127, 0, 0, 1], port))).start())
            .err()
            .expect("the port is taken");
        let plan = ListenerPlan { gateway_port: port, ..ListenerPlan::default() };
        let m = format!("{:#}", explain_start_error(err, &plan));
        assert!(m.contains(&format!("127.0.0.1:{port}")) && m.contains("already in use") && m.contains("--port"), "{m}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn other_failures_are_not_called_port_problems() {
        let dir = scratch("others");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let standard = ListenerPlan { gateway_port: 4577, standard_pins: vec![("blob".into(), 10000)], ..ListenerPlan::default() };
        let free = SocketAddr::from(([127, 0, 0, 1], 0));
        // a certificate directory that cannot be made: its path is in the message
        let blocker = dir.join("not-a-directory");
        std::fs::write(&blocker, b"x").unwrap();
        let bad_ca = blocker.join("ca");
        let err = rt.block_on(Emulator::builder().cert_dir(bad_ca.clone()).gateway(free).start()).err().expect("cannot make the directory");
        let m = format!("{:#}", explain_start_error(err, &standard));
        assert!(m.contains(&bad_ca.display().to_string()), "{m}");
        for wrong in ["--standard-ports", "free that port", "already in use", "listen"] {
            assert!(!m.contains(wrong), "{wrong}: {m}");
        }
        // a data directory that another azemu holds: the other process is named
        let data = dir.join("data");
        let first = rt.block_on(Emulator::builder().cert_dir(dir.join("ca")).data_dir(&data).gateway(free).start()).expect("first start");
        let err = rt
            .block_on(Emulator::builder().cert_dir(dir.join("ca2")).data_dir(&data).gateway(free).start())
            .err()
            .expect("the data directory is locked");
        let m = format!("{:#}", explain_start_error(err, &standard));
        assert!(m.contains(&data.display().to_string()) && m.contains(&format!("process {}", std::process::id())), "{m}");
        for wrong in ["--standard-ports", "free that port", "already in use by another program"] {
            assert!(!m.contains(wrong), "{wrong}: {m}");
        }
        rt.block_on(first.shutdown());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn port_443_is_optional_and_its_refusal_is_a_warning_that_says_what_to_do() {
        let hosts = [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)];
        // refused everywhere: no host, one warning that names the port, the reason and a way out
        let (ok, warning) = open_optional_port(&hosts, 443, "arm", |_| Err(IoError::from(ErrorKind::PermissionDenied)));
        assert!(ok.is_empty());
        let w = warning.expect("a warning");
        for want in ["443", "below 1024", "container", "setcap", "ip_unprivileged_port_start", "starts without it"] {
            assert!(w.contains(want), "{want}: {w}");
        }
        // the IPv6 address is missing on this machine: the IPv4 one is enough, and there is nothing to warn about
        let (ok, warning) =
            open_optional_port(&hosts, 443, "arm", |a| if a.is_ipv4() { Ok(()) } else { Err(IoError::from(ErrorKind::AddrNotAvailable)) });
        assert_eq!(ok, vec![hosts[0]]);
        assert!(warning.is_none());
        // taken by another program
        let (_, warning) = open_optional_port(&hosts, 443, "arm", |_| Err(IoError::from(ErrorKind::AddrInUse)));
        assert!(warning.unwrap().contains("another program already listens on port 443"));
    }

    #[test]
    fn standard_ports_try_443_for_arm_and_leave_it_when_the_user_took_arm_or_the_port() {
        let all = ["blob", "arm", "servicebus"];
        assert_eq!(optional_standard_pins(&all, 4577, &[], &[]), vec![("arm".to_string(), 443)]);
        assert!(optional_standard_pins(&["blob"], 4577, &[], &[]).is_empty(), "no ARM, no pin");
        assert!(optional_standard_pins(&all, 443, &[], &[]).is_empty(), "--port 443 is the gateway's");
        assert!(optional_standard_pins(&all, 4577, &[("arm".into(), 7000)], &[]).is_empty(), "ARM has the user's port");
        assert!(optional_standard_pins(&all, 4577, &[("blob".into(), 443)], &[]).is_empty(), "the user pinned something else to 443");
        assert!(STANDARD_PORTS.iter().all(|(_, p)| *p != 443), "443 is optional; it must not also be a required port");
    }

    #[test]
    fn an_unwritable_default_certificate_directory_gets_a_private_temporary_one() {
        let dir = scratch("certdir");
        // the default cannot be made (a file is in the way): a temporary directory is chosen and the reason is kept
        let blocker = dir.join("home-is-a-file");
        std::fs::write(&blocker, b"x").unwrap();
        let default = blocker.join(".azemu");
        match choose_cert_dir(None, &default) {
            CertDirChoice::Temporary { default_dir, error } => {
                assert_eq!(default_dir, default);
                assert!(!error.is_empty());
            }
            other => panic!("{other:?}"),
        }
        // a directory the user named is theirs: no replacement, the later error names it
        assert_eq!(choose_cert_dir(Some(&default), &default), CertDirChoice::AsConfigured);
        // a writable default is used as it is
        assert_eq!(choose_cert_dir(None, &dir.join("fine")), CertDirChoice::AsConfigured);
        // the temporary directory is new, private to the user, and not shared between two calls
        let a = make_private_temp_dir(&dir).unwrap();
        let b = make_private_temp_dir(&dir).unwrap();
        assert_ne!(a, b);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_default_directory_that_holds_a_ca_is_still_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("readonly");
        let default = dir.join("ca");
        std::fs::create_dir_all(&default).unwrap();
        // not writable: without a CA in it the answer is a temporary directory (skipped as root, which can write anywhere)
        std::fs::set_permissions(&default, std::fs::Permissions::from_mode(0o500)).unwrap();
        if dir_is_writable(&default).is_err() {
            assert!(matches!(choose_cert_dir(None, &default), CertDirChoice::Temporary { .. }));
            // with the four files of a CA in it, it is left alone
            std::fs::set_permissions(&default, std::fs::Permissions::from_mode(0o700)).unwrap();
            for f in ["ca.pem", "ca.issuing.pem", "ca.key", "ca.meta"] {
                std::fs::write(default.join(f), b"x").unwrap();
            }
            std::fs::set_permissions(&default, std::fs::Permissions::from_mode(0o500)).unwrap();
            assert_eq!(choose_cert_dir(None, &default), CertDirChoice::AsConfigured);
        }
        std::fs::set_permissions(&default, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_name_check_tells_a_name_that_resolves_from_one_that_does_not() {
        assert!(resolves_to_loopback_only_blocking("localhost"));
        assert!(!resolves_to_loopback_only_blocking("azemu-self-test.invalid"));
    }

    #[test]
    fn the_warning_names_the_three_ways_out() {
        let t = localhost_warning("azemu-self-test.blob.core.localhost", false);
        for want in ["--standard-ports", "hosts-file", "network alias", "azemu-self-test.blob.core.localhost", ".core.localhost"] {
            assert!(t.contains(want), "{want}: {t}");
        }
        assert!(localhost_warning("x.localhost", true).contains("you started with --standard-ports"));
    }
}
