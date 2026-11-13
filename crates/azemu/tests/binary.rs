//! The `azemu` program itself: what `--version`, `--help` and the ready line say, how options that stand in for
//! variables reach the services, what the start-up warns about, and `GET /_azemu/config`.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use azemu_core::testing::Client;

const BIN: &str = env!("CARGO_BIN_EXE_azemu");

/// A new directory name under the system's temporary directory (not created).
fn scratch_dir(what: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "azemu-bin-test-{what}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

struct Running {
    child: Child,
    ready: String,
    stderr: Arc<Mutex<String>>,
    cert_dir: std::path::PathBuf,
}

impl Running {
    fn start(args: &[&str], env: &[(&str, &str)]) -> Running {
        let cert_dir = scratch_dir("cert");
        let dir = cert_dir.to_str().unwrap().to_string();
        let mut all = vec!["--port", "0", "--cert-dir", dir.as_str()];
        all.extend_from_slice(args);
        Running::launch(&all, env, None, cert_dir.clone())
    }

    /// Start with exactly these arguments. `umask`, when given, is set by a shell that then runs the program. `cleanup` is
    /// a directory to remove when the test ends.
    fn launch(args: &[&str], env: &[(&str, &str)], umask: Option<&str>, cleanup: std::path::PathBuf) -> Running {
        let mut cmd = match umask {
            Some(mask) => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(format!("umask {mask}; exec \"$0\" \"$@\"")).arg(BIN);
                c
            }
            None => Command::new(BIN),
        };
        cmd.args(args);
        // nothing from the caller's environment may change what these tests see
        for (k, _) in std::env::vars().filter(|(k, _)| k.starts_with("AZEMU_") && k != "AZEMU_GIT_COMMIT") {
            cmd.env_remove(k);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("start azemu");
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut err = child.stderr.take().unwrap();
        let sink = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(out).read_line(&mut line);
            let _ = tx.send(line);
        });
        let ready = rx.recv_timeout(Duration::from_secs(30)).unwrap_or_default();
        assert!(ready.starts_with("azemu ready"), "no ready line; stderr:\n{}", stderr.lock().unwrap());
        Running { child, ready, stderr, cert_dir: cleanup }
    }

    fn gateway(&self) -> String {
        self.ready.split_whitespace().find_map(|w| w.strip_prefix("gateway=")).unwrap().to_string()
    }

    fn stderr(&self) -> String {
        std::thread::sleep(Duration::from_millis(100));
        self.stderr.lock().unwrap().clone()
    }

    async fn get_json(&self, path: &str) -> azemu_core::serde_json::Value {
        let r = Client::new().request("GET", &format!("{}{path}", self.gateway())).send().await;
        assert_eq!(r.status, 200, "{path}: {}", r.text());
        r.json()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.cert_dir);
    }
}

fn run(args: &[&str]) -> String {
    let out = Command::new(BIN).args(args).output().expect("run azemu");
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

#[test]
fn version_and_the_ready_line_say_which_build_it_is() {
    let v = run(&["--version"]);
    assert!(v.contains(azemu_core::VERSION) && v.contains(&format!("commit {}", azemu_core::GIT_COMMIT)), "{v}");
    // nothing is committed in some trees: the build must still name a commit field
    assert!(azemu_core::GIT_COMMIT == "unknown" || azemu_core::GIT_COMMIT.chars().all(|c| c.is_ascii_hexdigit()));
    let r = Running::start(&[], &[]);
    assert!(r.ready.contains(&format!("version={}", azemu_core::VERSION)), "{}", r.ready);
    assert!(r.ready.contains(&format!("commit={}", azemu_core::GIT_COMMIT)), "{}", r.ready);
}

#[tokio::test]
async fn health_and_config_say_which_build_it_is() {
    let r = Running::start(&[], &[]);
    for path in ["/_azemu/health", "/_azemu/config"] {
        let v = r.get_json(path).await;
        assert_eq!(v["version"], azemu_core::VERSION, "{path}");
        assert_eq!(v["commit"], azemu_core::GIT_COMMIT, "{path}");
    }
}

#[test]
fn help_shows_the_options_that_stand_in_for_variables_and_the_standard_ports() {
    let h = run(&["--help"]);
    for want in ["--lenient", "--allow-webhook-host", "--standard-ports", "10000", "10010", "8081", "5672", "5671", "9093", "--tls-name"] {
        assert!(h.contains(want), "--help does not mention {want}:\n{h}");
    }
}

#[tokio::test]
async fn options_reach_the_settings_before_the_services_read_them() {
    let r = Running::start(
        &["--lenient", "--allow-webhook-host", "hooks.example", "--allow-webhook-host=other.example"],
        &[("AZEMU_ALLOW_WEBHOOK_HOST", "from-env.example")],
    );
    let config = r.get_json("/_azemu/config").await;
    let settings = config["settings"].as_array().unwrap();
    let get =
        |name: &str| settings.iter().find(|s| s["name"] == name).cloned().unwrap_or_else(|| panic!("{name} is not in force: {config}"));
    let lenient = get("AZEMU_LENIENT");
    assert_eq!((lenient["value"].as_str(), lenient["source"].as_str()), (Some("true"), Some("option")));
    // the option adds to the variable; the services see both
    let hosts = get("AZEMU_ALLOW_WEBHOOK_HOST");
    assert_eq!(hosts["value"], "from-env.example,hooks.example,other.example", "{hosts}");
    // the program did not complain that the options came too late
    assert!(!r.stderr().contains("has no effect"), "{}", r.stderr());
}

#[tokio::test]
async fn the_variable_works_alone_and_the_option_is_not_needed() {
    let r = Running::start(&[], &[("AZEMU_LENIENT", "yes")]);
    let config = r.get_json("/_azemu/config").await;
    let lenient = config["settings"].as_array().unwrap().iter().find(|s| s["name"] == "AZEMU_LENIENT").cloned().unwrap();
    assert_eq!((lenient["value"].as_str(), lenient["source"].as_str()), (Some("true"), Some("environment")));
}

#[tokio::test]
async fn a_mistyped_variable_is_warned_about_and_a_real_one_is_not() {
    let r = Running::start(
        &[],
        &[
            ("AZEMU_DATAA_DIR", "/nowhere"),   // typing mistake
            ("AZEMU_BLOB_QUERY_SECONDS", "5"), // a real setting that is read on first use
            ("AZEMU_SAVE_INTERVAL", "30"),     // a real setting core reads
        ],
    );
    let err = r.stderr();
    assert!(err.contains("AZEMU_DATAA_DIR is set, but no part of azemu reads"), "{err}");
    assert!(!err.contains("AZEMU_BLOB_QUERY_SECONDS is set"), "{err}");
    assert!(!err.contains("AZEMU_SAVE_INTERVAL is set"), "{err}");
    // the config shows both lists
    let config = r.get_json("/_azemu/config").await;
    let unread: Vec<&str> = config["unreadVariables"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    assert!(unread.contains(&"AZEMU_DATAA_DIR"), "{config}");
}

#[tokio::test]
async fn nothing_is_warned_about_when_nothing_is_wrong() {
    let r = Running::start(&[], &[]);
    let err = r.stderr();
    assert!(!err.contains("is set, but no part of azemu reads"), "{err}");
    assert!(!err.contains("ERROR"), "{err}");
}

/// One request with a Host header of our choosing, over a plain socket: the status line's code.
fn status_of(gateway: &str, host_label: &str, path: &str) -> u16 {
    use std::io::Write;
    let addr = gateway.trim_start_matches("http://");
    let port = addr.rsplit(':').next().unwrap();
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {host_label}:{port}\r\nAuthorization: Bearer x\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

#[test]
fn the_shutdown_log_says_what_was_not_emulated() {
    let mut r = Running::start(&[], &[]);
    let gw = r.gateway();
    // Key Vault does not build managed storage accounts: 501
    for _ in 0..2 {
        assert_eq!(status_of(&gw, "v.vault.localhost", "/storage?api-version=7.5"), 501);
    }
    let pid = r.child.id().to_string();
    assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    let code = r.child.wait().unwrap();
    assert!(code.success(), "{code:?}");
    let err = r.stderr();
    assert!(err.contains("not emulated during this run: 2 x keyvault"), "{err}");
    assert!(err.contains("answered 501"), "{err}");
}

#[test]
fn nothing_is_summarised_when_everything_asked_for_exists() {
    let mut r = Running::start(&[], &[]);
    let pid = r.child.id().to_string();
    assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    r.child.wait().unwrap();
    assert!(!r.stderr().contains("not emulated during this run"), "{}", r.stderr());
}

// ---- --port 0 ---------------------------------------------------------------------------------------------------------

/// One HTTP request over a plain socket to `addr`: the status code of the answer.
fn status_from(addr: &str, host: &str, method: &str, path: &str, extra_headers: &str) -> u16 {
    use std::io::Write;
    let mut s = std::net::TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect to {addr}: {e}"));
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n{extra_headers}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

/// The listeners `GET /_azemu/config` names: (kind or pinned service, address).
async fn listeners(r: &Running) -> Vec<(String, std::net::SocketAddr)> {
    let config = r.get_json("/_azemu/config").await;
    config["listeners"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let who = l["service"].as_str().unwrap_or("gateway").to_string();
            (who, l["address"].as_str().unwrap().parse().unwrap())
        })
        .collect()
}

#[tokio::test]
async fn port_zero_is_one_port_on_every_address_and_the_ready_line_names_it() {
    // the default addresses are 127.0.0.1 and ::1 (the second one only when the machine has IPv6)
    let r = Running::start(&["--pin", "blob=0"], &[]);
    let named: std::net::SocketAddr = r.gateway().trim_start_matches("http://").parse().unwrap();
    assert_ne!(named.port(), 0);
    let all = listeners(&r).await;
    let gateway: Vec<_> = all.iter().filter(|(who, _)| who == "gateway").map(|(_, a)| *a).collect();
    let pinned: Vec<_> = all.iter().filter(|(who, _)| who == "blob").map(|(_, a)| *a).collect();
    assert!(!gateway.is_empty() && !pinned.is_empty(), "{all:?}");
    assert!(
        gateway.iter().all(|a| a.port() == named.port()),
        "the gateway has more than one port: {gateway:?}, the ready line says {named}"
    );
    assert!(pinned.iter().all(|a| a.port() == pinned[0].port()), "the pinned port differs between addresses: {pinned:?}");
    assert_eq!(gateway.len(), pinned.len(), "{all:?}");
    // real sockets: every address answers on the port that was named
    for a in gateway.iter().chain(pinned.iter()) {
        assert_eq!(status_from(&a.to_string(), &format!("localhost:{}", a.port()), "GET", "/_azemu/health", ""), 200, "{a}");
    }
    // the ready line's port serves on both loopback addresses when both are listening
    if gateway.len() == 2 {
        let both = [format!("127.0.0.1:{}", named.port()), format!("[::1]:{}", named.port())];
        for addr in &both {
            assert_eq!(status_from(addr, &format!("localhost:{}", named.port()), "GET", "/_azemu/health", ""), 200, "{addr}");
        }
    }
}

// ---- the directories that hold secrets ------------------------------------------------------------------------------

#[cfg(unix)]
mod directories {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn stop(mut r: Running) -> String {
        let _ = Command::new("kill").args(["-TERM", &r.child.id().to_string()]).status();
        let _ = r.child.wait();
        r.stderr()
    }

    #[test]
    fn the_default_certificate_directory_is_private_whatever_the_umask() {
        let home = scratch_dir("home");
        std::fs::create_dir_all(&home).unwrap();
        let r = Running::launch(&["--port", "0"], &[("HOME", home.to_str().unwrap())], Some("002"), home.clone());
        let dir = home.join(".azemu");
        assert_eq!(mode(&dir), 0o700, "SECURITY.md says the directory azemu makes is 0700");
        assert_eq!(mode(&dir.join("ca.key")), 0o600);
        let err = stop(r);
        assert!(!err.contains("certificate directory"), "azemu made it and should not complain about it: {err}");
    }

    #[test]
    fn a_certificate_directory_named_with_cert_dir_is_private_when_azemu_makes_it() {
        let base = scratch_dir("named");
        let dir = base.join("ca");
        let r = Running::launch(&["--port", "0", "--cert-dir", dir.to_str().unwrap()], &[], Some("002"), base.clone());
        assert_eq!(mode(&dir), 0o700);
        stop(r);
    }

    #[test]
    fn a_certificate_directory_that_others_can_use_is_reported_and_left_as_it_is() {
        let base = scratch_dir("open");
        let dir = base.join("ca");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        let r = Running::launch(&["--port", "0", "--cert-dir", dir.to_str().unwrap()], &[], None, base.clone());
        assert_eq!(mode(&dir), 0o775, "a directory the user chose is not changed");
        let err = stop(r);
        let lines: Vec<&str> = err.lines().filter(|l| l.contains("the certificate directory")).collect();
        assert_eq!(lines.len(), 1, "{err}");
        assert!(lines[0].contains("WARN") && lines[0].contains("775") && lines[0].contains("chmod 700"), "{}", lines[0]);
    }

    #[test]
    fn the_data_directory_is_private_when_azemu_makes_it_and_an_open_one_is_reported() {
        let base = scratch_dir("data");
        let new = base.join("state");
        let r = Running::start_with_data(&new, Some("002"), &base);
        assert_eq!(mode(&new), 0o700, "the data directory holds Key Vault keys");
        let err = stop(r);
        assert!(!err.contains("the data directory"), "{err}");

        let open = base.join("open-state");
        std::fs::create_dir_all(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let r = Running::start_with_data(&open, None, &base);
        assert_eq!(mode(&open), 0o755);
        let err = stop(r);
        let lines: Vec<&str> = err.lines().filter(|l| l.contains("the data directory")).collect();
        assert_eq!(lines.len(), 1, "{err}");
        assert!(lines[0].contains("WARN") && lines[0].contains("755") && lines[0].contains("Key Vault"), "{}", lines[0]);
    }

    impl Running {
        fn start_with_data(data: &std::path::Path, umask: Option<&str>, cleanup: &std::path::Path) -> Running {
            let certs = cleanup.join("certs");
            Running::launch(
                &["--port", "0", "--cert-dir", certs.to_str().unwrap(), "--data-dir", data.to_str().unwrap()],
                &[],
                umask,
                cleanup.to_path_buf(),
            )
        }
    }
}

// ---- names that are not typing mistakes ---------------------------------------------------------------------------------

#[tokio::test]
async fn the_names_of_the_test_tools_are_not_warned_about_and_a_real_mistake_still_is() {
    let r = Running::start(
        &[],
        &[
            ("AZEMU_BIN", "/x/azemu"),
            ("AZEMU_GATEWAY", "http://127.0.0.1:4577"),
            ("AZEMU_CA_FILE", "/x/ca.pem"),
            ("AZEMU_IMAGE", "azemu:latest"),
            ("AZEMU_KEEP_LOG", "1"),
            ("AZEMU_TEST_KEEP_LOGS", "1"),
            ("AZEMU_TESTKIT_DIR", "/x"),
            ("AZEMU_BLOB_QUERY_SEOCNDS", "5"), // a typing mistake
        ],
    );
    let err = r.stderr();
    assert!(err.contains("AZEMU_BLOB_QUERY_SEOCNDS is set, but no part of azemu reads"), "{err}");
    for quiet in
        ["AZEMU_BIN", "AZEMU_GATEWAY", "AZEMU_CA_FILE", "AZEMU_IMAGE", "AZEMU_KEEP_LOG", "AZEMU_TEST_KEEP_LOGS", "AZEMU_TESTKIT_DIR"]
    {
        assert!(!err.contains(&format!("{quiet} is set")), "{err}");
    }
    let config = r.get_json("/_azemu/config").await;
    let unread: Vec<&str> = config["unreadVariables"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(unread, vec!["AZEMU_BLOB_QUERY_SEOCNDS"], "{config}");
}

#[tokio::test]
async fn a_reset_of_one_account_is_a_normal_operation_and_logs_no_warning() {
    let r = Running::start(&[], &[]);
    let gw = r.gateway();
    let client = Client::new();
    let reset = client.request("POST", &format!("{gw}/_azemu/reset?account=someacct")).send().await;
    assert_eq!(reset.status, 200, "{}", reset.text());
    // Identity keeps nothing per account: the answer says so, the log does not complain
    assert!(reset.text().contains("notSupported"), "{}", reset.text());
    let err = r.stderr();
    assert!(!err.contains("WARN"), "{err}");
    assert!(!err.contains("reset by account"), "{err}");
}

// ---- a refused request says why ----------------------------------------------------------------------------------------

#[test]
fn a_request_refused_for_lack_of_a_credential_is_explained_once_in_the_log() {
    let r = Running::start(&[], &[]);
    let gw = r.gateway();
    let addr = gw.trim_start_matches("http://").to_string();
    let port = addr.rsplit(':').next().unwrap().to_string();
    let host = format!("acct.blob.core.localhost:{port}");
    // a retry loop: the same refusal six times
    for _ in 0..6 {
        assert_eq!(status_from(&addr, &host, "PUT", "/box?restype=container", ""), 401);
    }
    // another reason: a credential that was sent and refused (a SAS whose time has passed)
    let sas = "/box?restype=container&sv=2021-08-06&sr=c&sp=rl&se=2001-01-01T00:00:00Z&sig=SECRETSIG";
    assert_eq!(status_from(&addr, &host, "GET", sas, ""), 403);
    let err = r.stderr();
    let refused: Vec<&str> = err.lines().filter(|l| l.contains("refused ")).collect();
    let none: Vec<&&str> = refused.iter().filter(|l| l.contains("no Authorization header")).collect();
    assert_eq!(none.len(), 1, "{err}");
    assert!(none[0].contains("INFO") && none[0].contains("service=\"blob\"") || none[0].contains("service=blob"), "{}", none[0]);
    assert!(
        none[0].contains("PUT /box (restype=container)")
            && none[0].contains("no SAS in the query")
            && none[0].contains("Blob SDK was probably built without a credential"),
        "{}",
        none[0]
    );
    let sent: Vec<&&str> = refused.iter().filter(|l| l.contains("a SAS signature in the query")).collect();
    assert_eq!(sent.len(), 1, "{err}");
    assert!(
        sent[0].contains("was not accepted")
            && sent[0].contains("AuthenticationFailed")
            && sent[0].contains("GET /box (restype=container)"),
        "{}",
        sent[0]
    );
    assert!(!err.contains("SECRETSIG"), "{err}");
    assert_eq!(refused.len(), 2, "{err}");
}

// ---- --exit-on-stdin-close --------------------------------------------------------------------------------------------

/// Start azemu with a pipe on its standard input and the option; the ready line is read.
fn start_with_stdin_pipe() -> (Child, std::path::PathBuf) {
    let dir = scratch_dir("stdin");
    let mut child = Command::new(BIN)
        .args(["--port", "0", "--host", "127.0.0.1", "--exit-on-stdin-close", "--cert-dir"])
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start azemu");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert!(line.starts_with("azemu ready"), "{line}");
    (child, dir)
}

fn wait_for_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if let Some(st) = child.try_wait().unwrap() {
            return Some(st);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn azemu_stops_when_the_program_that_started_it_goes_away() {
    // the test program stands for the one that exits (or panics) and takes its end of the pipe with it
    let (mut child, dir) = start_with_stdin_pipe();
    assert!(child.try_wait().unwrap().is_none(), "it keeps running while the pipe is open");
    drop(child.stdin.take());
    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    if status.is_none() {
        let _ = child.kill();
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.is_some_and(|s| s.success()), "{status:?}: azemu did not stop when its standard input was closed");
}

#[test]
fn with_the_option_a_signal_still_stops_azemu_while_the_pipe_is_open() {
    let (mut child, dir) = start_with_stdin_pipe();
    assert!(Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap().success());
    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    if status.is_none() {
        let _ = child.kill();
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.is_some_and(|s| s.success()), "{status:?}");
}
