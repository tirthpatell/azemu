//! What a release needs from the `azemu` program: the hidden `__healthcheck` command (what a container's HEALTHCHECK runs),
//! stopping on SIGTERM at once with exit code 0 (what `docker stop` sends), and, in a build without the `kafka` feature, an
//! answer on the Kafka paths that says so.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_azemu");

struct Running {
    child: Child,
    port: u16,
    cert_dir: std::path::PathBuf,
}

impl Running {
    fn start() -> Running {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let cert_dir = std::env::temp_dir().join(format!(
            "azemu-release-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut cmd = Command::new(BIN);
        cmd.args(["--port", "0", "--cert-dir"]).arg(&cert_dir);
        for (k, _) in std::env::vars().filter(|(k, _)| k.starts_with("AZEMU_") && k != "AZEMU_GIT_COMMIT") {
            cmd.env_remove(k);
        }
        let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("start azemu");
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(out).read_line(&mut line);
            let _ = tx.send(line);
        });
        let ready = rx.recv_timeout(Duration::from_secs(30)).unwrap_or_default();
        assert!(ready.starts_with("azemu ready"), "no ready line: {ready:?}");
        let gateway = ready.split_whitespace().find_map(|w| w.strip_prefix("gateway=")).unwrap();
        let port: u16 = gateway.rsplit(':').next().unwrap().parse().unwrap();
        Running { child, port, cert_dir }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.cert_dir);
    }
}

fn healthcheck(port: u16) -> (Option<i32>, String) {
    let out = Command::new(BIN).arg("__healthcheck").arg(port.to_string()).output().expect("run azemu __healthcheck");
    (out.status.code(), String::from_utf8_lossy(&out.stdout).to_string())
}

#[test]
fn the_health_check_says_ok_while_the_emulator_runs() {
    let r = Running::start();
    let (code, text) = healthcheck(r.port);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("ok"), "{text}");
}

#[test]
fn the_health_check_fails_when_nothing_listens() {
    // a free port: bind, note the number, close
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let started = Instant::now();
    let (code, text) = healthcheck(port);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.starts_with("unhealthy"), "{text}");
    assert!(started.elapsed() < Duration::from_secs(10), "a refused connection must not wait for the time limit");
}

#[test]
fn the_health_check_does_not_start_the_emulator_with_other_arguments() {
    // `__healthcheck` is only the first argument; the program still answers --version as before
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
}

#[cfg(unix)]
#[test]
fn sigterm_ends_the_emulator_at_once_with_exit_code_zero() {
    let mut r = Running::start();
    let started = Instant::now();
    let ok = Command::new("kill").args(["-TERM", &r.child.id().to_string()]).status().unwrap();
    assert!(ok.success());
    let status = loop {
        if let Some(s) = r.child.try_wait().unwrap() {
            break s;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "still running 5 s after SIGTERM");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
}

/// A plain HTTP/1.1 GET; returns the status line and the body.
fn get(port: u16, path: &str) -> (String, String) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (head.lines().next().unwrap_or("").to_string(), body.to_string())
}

#[cfg(not(feature = "kafka"))]
#[test]
fn without_the_kafka_feature_its_path_says_the_build_does_not_include_it() {
    let r = Running::start();
    let (status, body) = get(r.port, "/_azemu/kafka/ns/groups");
    assert!(status.contains("501"), "{status} {body}");
    assert!(body.contains("does not include the Kafka endpoint"), "{body}");
}

#[cfg(feature = "kafka")]
#[test]
fn with_the_kafka_feature_its_path_answers_from_the_endpoint() {
    let r = Running::start();
    let (status, body) = get(r.port, "/_azemu/kafka/ns/groups");
    assert!(status.contains("200"), "{status} {body}");
}
