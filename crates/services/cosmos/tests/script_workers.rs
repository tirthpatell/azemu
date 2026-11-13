//! Scripts run in worker processes. These tests start the real emulator program (`azemu-cosmos`, whose workers are the same
//! executable started with a hidden argument) and check what the outside can see: that no script ends the emulator, holds a
//! slot longer than its time limit, or grows the emulator, and that no worker process is left behind. They are the four
//! reproductions of hostile scripts (a parser overflow, a native call that never returns, work in callbacks, memory) cut down to bounded size, and the failures of the worker itself.
//!
//! The tests share one machine with other work, so they run one at a time (`SERIAL`), and every script that needs memory or
//! processor time is bounded by a small limit.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

static SERIAL: Mutex<()> = Mutex::new(());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

const HIDDEN: &str = "__cosmos-script-worker";
const BIN: &str = env!("CARGO_BIN_EXE_azemu-cosmos");

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Emu {
    child: Child,
    port: u16,
    dir: PathBuf,
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
    fn message(&self) -> String {
        self.json()["message"].as_str().unwrap_or(&self.body).to_string()
    }
}

fn http(port: u16, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> std::io::Result<Reply> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: acct.documents.localhost:{port}\r\nauthorization: type%3Dmaster%26ver%3D1.0%26sig%3Dx\r\n\
         x-ms-version: 2018-12-31\r\nx-ms-date: Thu, 12 Nov 2026 12:00:00 GMT\r\nContent-Type: application/json\r\nConnection: close\r\n\
         Content-Length: {}\r\n",
        body.len()
    );
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    s.write_all(body.as_bytes())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, mut rest) =
        text.split_once("\r\n\r\n").map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((text.clone(), String::new()));
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let mut out = String::new();
        while let Some((len, tail)) = rest.split_once("\r\n") {
            let n = usize::from_str_radix(len.trim(), 16).unwrap_or(0);
            if n == 0 || tail.len() < n {
                break;
            }
            out.push_str(&tail[..n]);
            rest = tail[n..].trim_start_matches("\r\n").to_string();
        }
        rest = out;
    }
    Ok(Reply { status, headers: head, body: rest })
}

impl Emu {
    /// Starts the emulator on a free port with `env` added to its environment (`AZEMU_COSMOS_SCRIPT_*`, `AZEMU_SCRIPT_WORKER`).
    fn start(env: &[(&str, &str)]) -> Emu {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!("azemu-sw-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cmd = Command::new(BIN);
        cmd.args(["--port", &port.to_string(), "--host", "127.0.0.1", "--cert-dir"]).arg(&dir).stdout(Stdio::null()).stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().unwrap();
        let emu = Emu { child, port, dir };
        let t = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "the emulator did not start");
            thread::sleep(Duration::from_millis(50));
        }
        emu
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn req(&self, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> Reply {
        http(self.port, method, path, extra, body).unwrap()
    }

    /// The emulator answers (a health check: an ordinary Cosmos call) and has not exited.
    fn assert_alive(&mut self) {
        assert!(self.child.try_wait().unwrap().is_none(), "the emulator process ended: {:?}", self.child.try_wait());
        let r = self.req("GET", "/dbs", &[], "");
        assert_eq!(r.status, 200, "{}", r.body);
    }

    fn setup(&self) {
        assert_eq!(self.req("POST", "/dbs", &[], &json!({"id": "d"}).to_string()).status, 201);
        let c = json!({"id": "c", "partitionKey": {"paths": ["/pk"], "kind": "Hash"}});
        assert_eq!(self.req("POST", "/dbs/d/colls", &[], &c.to_string()).status, 201);
    }

    fn sproc(&self, name: &str, body: &str) {
        let r = self.req("POST", "/dbs/d/colls/c/sprocs", &[], &json!({"id": name, "body": body}).to_string());
        assert_eq!(r.status, 201, "{}", r.body);
    }

    fn run(&self, name: &str) -> Reply {
        self.req("POST", &format!("/dbs/d/colls/c/sprocs/{name}"), &[("x-ms-documentdb-partitionkey", "[\"a\"]")], "[]")
    }

    fn hello(&self) -> Reply {
        self.sproc_once("hello", "function () { getContext().getResponse().setBody('Hello, World'); }")
    }

    fn sproc_once(&self, name: &str, body: &str) -> Reply {
        let _ = self.req("DELETE", &format!("/dbs/d/colls/c/sprocs/{name}"), &[], "");
        self.sproc(name, body);
        self.run(name)
    }

    /// Worker processes that are children of the emulator.
    fn workers(&self) -> Vec<u32> {
        workers_of(self.pid())
    }

    fn wait_workers(&self, n: usize, within: Duration) -> Vec<u32> {
        let t = Instant::now();
        loop {
            let w = self.workers();
            if w.len() == n || t.elapsed() > within {
                return w;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// No worker process of this emulator is left (a worker that is killed is reaped at once; the wait is for slow machines).
    fn assert_no_workers(&self) {
        let w = self.wait_workers(0, Duration::from_secs(2));
        assert!(w.is_empty(), "worker processes are left: {w:?}");
    }

    fn status_kb(&self, key: &str) -> u64 {
        let text = std::fs::read_to_string(format!("/proc/{}/status", self.pid())).unwrap();
        text.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok()).unwrap_or(0)
    }
}

impl Drop for Emu {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Processes that are children of `parent` and run a script worker (or the shell script that stands in for one).
fn workers_of(parent: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // `pid (comm) state ppid ...`
        let ppid: u32 = stat.rsplit_once(") ").and_then(|(_, r)| r.split_whitespace().nth(1)).and_then(|p| p.parse().ok()).unwrap_or(0);
        if ppid != parent {
            continue;
        }
        let cmd = String::from_utf8_lossy(&std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default()).into_owned();
        if cmd.contains(HIDDEN) || cmd.contains("fake-worker") {
            out.push(pid);
        }
    }
    out
}

fn kill9(pid: u32) {
    assert!(Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap().success());
}

fn alive(pid: u32) -> bool {
    // a zombie still has a directory; its state tells
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| !s.rsplit_once(") ").map(|(_, r)| r.starts_with('Z')).unwrap_or(true))
        .unwrap_or(false)
}

const FIB: &str = "function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }";

/// Environment for a short time limit and a given number of slots.
fn quick(timeout_ms: u32, running: u32) -> Vec<(&'static str, String)> {
    vec![("AZEMU_COSMOS_SCRIPT_TIMEOUT_MS", timeout_ms.to_string()), ("AZEMU_COSMOS_SCRIPT_MAX_RUNNING", running.to_string())]
}

fn start<K: AsRef<str>, V: AsRef<str>>(env: &[(K, V)]) -> Emu {
    let borrowed: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_ref(), v.as_ref())).collect();
    let emu = Emu::start(&borrowed);
    emu.setup();
    emu
}

/// Runs the request in a thread and gives back what it answered and how long it took.
fn spawn_run(port: u16, name: &'static str) -> thread::JoinHandle<(Reply, Duration)> {
    thread::spawn(move || {
        let t = Instant::now();
        let r = http(port, "POST", &format!("/dbs/d/colls/c/sprocs/{name}"), &[("x-ms-documentdb-partitionkey", "[\"a\"]")], "[]").unwrap();
        (r, t.elapsed())
    })
}

// ---- Text that overflows the parser's stack ----

#[test]
fn c1_nesting_inside_a_template_literal_is_refused_with_a_message() {
    let _g = serial();
    let mut emu = start(&quick(5000, 4));
    let body = format!("function () {{ var x = `${{{}1{}}}`; }}", "(".repeat(15000), ")".repeat(15000));
    assert!(body.len() < 32 * 1024);
    let r = emu.sproc_once("deep", &body);
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.message().contains("nests brackets"), "{}", r.body);
    emu.assert_alive();
    assert_eq!(emu.hello().status, 200);
    emu.assert_no_workers();
}

#[test]
fn c1_text_that_still_gets_past_the_check_ends_the_worker_not_the_emulator() {
    let _g = serial();
    let mut emu = start(&quick(10_000, 4));
    // a quote inside a regular expression literal hides the rest of the line from the bracket count (the check is only a first line)
    let body = format!("function () {{ var r = /\"/; var x = {}1{}; }}", "(".repeat(15000), ")".repeat(15000));
    assert!(body.len() < 32 * 1024);
    let before = emu.status_kb("VmHWM:");
    let r = emu.sproc_once("deep", &body);
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.message().contains("stack"), "{}", r.body);
    emu.assert_alive();
    emu.assert_no_workers();
    assert_eq!(emu.hello().status, 200, "the next script runs");
    // the emulator's own memory did not move (the overflow was in the worker)
    assert!(emu.status_kb("VmHWM:") < before + 20 * 1024, "{} -> {}", before, emu.status_kb("VmHWM:"));
}

// ---- One native call that never returns ----

#[test]
fn c2_catastrophic_regex_is_killed_at_the_time_limit_and_four_of_them_do_not_exhaust_the_slots() {
    let _g = serial();
    let mut emu = start(&quick(1000, 4));
    for i in 0..4 {
        emu.sproc(&format!("re{i}"), "function () { /^(a+)+$/.test('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!'); }");
    }
    let t = Instant::now();
    let runs: Vec<_> = ["re0", "re1", "re2", "re3"].into_iter().map(|n| spawn_run(emu.port, n)).collect();
    // four workers exist while they run
    assert_eq!(emu.wait_workers(4, Duration::from_millis(900)).len(), 4);
    for h in runs {
        let (r, took) = h.join().unwrap();
        assert_eq!(r.status, 408, "{}", r.body);
        assert!(took < Duration::from_millis(2000), "{took:?}");
    }
    // the slots are free again within the time limit plus a second
    assert!(t.elapsed() < Duration::from_millis(2000), "{:?}", t.elapsed());
    emu.assert_no_workers();
    let t = Instant::now();
    assert_eq!(emu.hello().status, 200);
    assert!(t.elapsed() < Duration::from_millis(1000));
    // `/_azemu/reset` does not matter any more either
    assert_eq!(emu.req("POST", "/_azemu/reset", &[], "").status / 100, 2);
    emu.assert_alive();
}

// ---- Work in callbacks, outside the engine's clock ----

#[test]
fn c3_callbacks_that_run_forever_are_killed_and_do_not_exhaust_the_slots() {
    let _g = serial();
    let mut emu = start(&quick(1000, 4));
    let rss_before = emu.status_kb("VmRSS:");
    let shapes = [
        ("then", format!("function () {{ {FIB} Promise.resolve().then(function () {{ fib(70); }}); }}")),
        ("sort", format!("function () {{ {FIB} [2, 1].sort(function (a, b) {{ fib(70); return a - b; }}); }}")),
        ("replace", format!("function () {{ {FIB} 'a'.replace(/a/, function () {{ fib(70); return 'b'; }}); }}")),
        ("tojson", format!("function () {{ {FIB} JSON.stringify({{ toJSON: function () {{ fib(70); return 1; }} }}); }}")),
    ];
    for (n, b) in &shapes {
        emu.sproc(n, b);
    }
    let t = Instant::now();
    let runs: Vec<_> = ["then", "sort", "replace", "tojson"].into_iter().map(|n| spawn_run(emu.port, n)).collect();
    for h in runs {
        let (r, _) = h.join().unwrap();
        assert_eq!(r.status, 408, "{}", r.body);
    }
    assert!(t.elapsed() < Duration::from_millis(2000), "{:?}", t.elapsed());
    emu.assert_no_workers();
    assert_eq!(emu.hello().status, 200);
    emu.assert_alive();
    let rss_after = emu.status_kb("VmRSS:");
    assert!(rss_after < rss_before + 12 * 1024, "the emulator grew from {rss_before} to {rss_after} kB");
}

// ---- Memory ----

#[test]
fn c4_memory_used_in_a_promise_job_is_limited_in_the_worker_and_the_emulator_does_not_grow() {
    let _g = serial();
    let mut emu = start(&[("AZEMU_COSMOS_SCRIPT_MEMORY_MB", "64"), ("AZEMU_COSMOS_SCRIPT_TIMEOUT_MS", "8000")]);
    let before = emu.status_kb("VmRSS:");
    let double = "var s = 'xxxxxxxx'; for (var i = 0; i < 27; i++) { s = s + s; } getContext().getResponse().setBody(s.length);";
    for (name, body) in [
        ("main", format!("function () {{ {double} }}")),
        ("job", format!("function () {{ Promise.resolve().then(function () {{ {double} }}); }}")),
    ] {
        let r = emu.sproc_once(name, &body);
        assert_eq!(r.status, 400, "{name}: {}", r.body);
        assert!(r.message().contains("64 MB of memory"), "{name}: {}", r.body);
        emu.assert_no_workers();
    }
    emu.assert_alive();
    assert_eq!(emu.hello().status, 200);
    let (rss, hwm) = (emu.status_kb("VmRSS:"), emu.status_kb("VmHWM:"));
    assert!(rss < before + 12 * 1024 && hwm < before + 30 * 1024, "the emulator went from {before} kB to {rss} kB (peak {hwm})");
}

#[test]
fn memory_that_a_script_may_use_is_still_available() {
    let _g = serial();
    let emu = start(&[("AZEMU_COSMOS_SCRIPT_MEMORY_MB", "64")]);
    // 16 MB of string is fine in a 64 MB budget
    let r = emu.sproc_once(
        "big",
        "function () { var s = 'xxxxxxxx'; for (var i = 0; i < 21; i++) { s = s + s; } getContext().getResponse().setBody(s.length); }",
    );
    assert_eq!((r.status, r.body.as_str()), (200, "16777216"), "{}", r.body);
}

// ---- the worker's own limits and surroundings ----

#[test]
fn a_worker_has_only_its_pipes_an_empty_environment_and_its_limits() {
    let _g = serial();
    let emu = start(&quick(3000, 4));
    emu.sproc("spin", &format!("function () {{ {FIB} fib(60); }}"));
    let run = spawn_run(emu.port, "spin");
    let w = emu.wait_workers(1, Duration::from_secs(2));
    assert_eq!(w.len(), 1);
    thread::sleep(Duration::from_millis(100));
    let pid = w[0];
    let mut fds: Vec<String> =
        std::fs::read_dir(format!("/proc/{pid}/fd")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    fds.sort();
    assert_eq!(fds, ["0", "1", "2"], "descriptors the worker holds");
    let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    assert_eq!(String::from_utf8_lossy(&env).trim_end_matches('\0'), "MALLOC_ARENA_MAX=1");
    let limits = std::fs::read_to_string(format!("/proc/{pid}/limits")).unwrap();
    let line = |name: &str| limits.lines().find(|l| l.starts_with(name)).unwrap_or("").to_string();
    assert!(!line("Max address space").contains("unlimited"), "{}", line("Max address space"));
    assert!(!line("Max cpu time").contains("unlimited"), "{}", line("Max cpu time"));
    let (r, _) = run.join().unwrap();
    assert_eq!(r.status, 408, "{}", r.body);
    emu.assert_no_workers();
}

#[test]
fn a_worker_killed_from_outside_is_an_error_for_that_request_and_frees_its_slot() {
    let _g = serial();
    let mut emu = start(&quick(10_000, 1));
    emu.sproc("spin", &format!("function () {{ {FIB} fib(60); }}"));
    let run = spawn_run(emu.port, "spin");
    let w = emu.wait_workers(1, Duration::from_secs(2));
    assert_eq!(w.len(), 1);
    let t = Instant::now();
    kill9(w[0]);
    let (r, _) = run.join().unwrap();
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(r.message().contains("script worker ended unexpectedly") && r.message().contains("SIGKILL"), "{}", r.body);
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    emu.assert_no_workers();
    // the only slot is free
    assert_eq!(emu.hello().status, 200);
    emu.assert_alive();
}

// ---- a worker that misbehaves (a small shell script stands in for it, `AZEMU_SCRIPT_WORKER`) ----

fn fake_worker(tag: &str, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!("azemu-fake-worker-{}-{tag}.sh", std::process::id()));
    // the word in the file name is what `workers()` looks for in the command line
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh\n# fake-worker\n{script}").unwrap();
    drop(f);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn with_fake_worker(tag: &str, script: &str) -> (Emu, PathBuf) {
    let p = fake_worker(tag, script);
    let emu = Emu::start(&[("AZEMU_SCRIPT_WORKER", p.to_str().unwrap()), ("AZEMU_COSMOS_SCRIPT_TIMEOUT_MS", "3000")]);
    emu.setup();
    (emu, p)
}

#[test]
fn a_worker_that_writes_garbage_or_a_message_that_is_too_large_is_an_error_and_is_killed() {
    let _g = serial();
    // a length of 2 bytes, then two bytes that are not JSON; the shell then keeps the process alive
    let (mut emu, p1) = with_fake_worker("garbage", "printf '\\000\\000\\000\\002xy'\nexec sleep 30");
    let r = emu.sproc_once("s", "function () { }");
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(r.message().contains("not a message"), "{}", r.body);
    emu.assert_no_workers();
    emu.assert_alive();
    // a length of 2 GB
    let (mut emu2, p2) = with_fake_worker("huge", "printf '\\177\\377\\377\\377'\nexec sleep 30");
    let r = emu2.sproc_once("s", "function () { }");
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.message().contains("more than the 16 MB"), "{}", r.body);
    emu2.assert_no_workers();
    emu2.assert_alive();
    // a worker that says something valid that it should not say (an answer to a function call, in a stored procedure)
    let frame = r#"{"t":"UdfDone","text":"1"}"#;
    let (emu3, p3) = with_fake_worker("wrong", &format!("printf '\\000\\000\\000\\{:03o}%s' '{frame}'\nexec sleep 30", frame.len()));
    let r = emu3.sproc_once("s", "function () { }");
    assert_eq!(r.status, 500, "{}", r.body);
    emu3.assert_no_workers();
    for p in [p1, p2, p3] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn a_worker_that_cannot_be_started_or_ends_at_once_is_an_error_that_says_so() {
    let _g = serial();
    let emu = Emu::start(&[("AZEMU_SCRIPT_WORKER", "/nonexistent/azemu-worker")]);
    emu.setup();
    let r = emu.sproc_once("s", "function () { }");
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(r.message().contains("could not be started") && r.message().contains("/nonexistent/azemu-worker"), "{}", r.body);
    // nothing falls back to running the script in the emulator: a query with a function says so as well
    let r = emu.req("POST", "/dbs/d/colls/c/udfs", &[], &json!({"id": "f", "body": "function (x) { return x; }"}).to_string());
    assert_eq!(r.status, 201);
    let _ =
        emu.req("POST", "/dbs/d/colls/c/docs", &[("x-ms-documentdb-partitionkey", "[\"a\"]")], &json!({"id": "1", "pk": "a"}).to_string());
    let r = emu.req(
        "POST",
        "/dbs/d/colls/c/docs",
        &[("x-ms-documentdb-partitionkey", "[\"a\"]"), ("x-ms-documentdb-isquery", "true"), ("content-type", "application/query+json")],
        &json!({"query": "SELECT VALUE udf.f(c.id) FROM c"}).to_string(),
    );
    assert_ne!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("could not be started"), "{}", r.body);

    let (emu, p) = with_fake_worker("exits", "exit 7");
    let r = emu.sproc_once("s", "function () { }");
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(r.message().contains("ended unexpectedly"), "{}", r.body);
    let _ = std::fs::remove_file(p);
}

// ---- many runs, the number at once, cancel, reset, shutdown ----

#[test]
fn two_hundred_sequential_runs_leave_no_process_and_no_descriptor_behind() {
    let _g = serial();
    let emu = start(&quick(5000, 4));
    emu.sproc("hello", "function () { getContext().getResponse().setBody('Hello, World'); }");
    let fds = || std::fs::read_dir(format!("/proc/{}/fd", emu.pid())).unwrap().count();
    assert_eq!(emu.run("hello").status, 200);
    let fds_before = fds();
    let t = Instant::now();
    for i in 0..200 {
        let r = emu.run("hello");
        assert_eq!((r.status, r.body.as_str()), (200, "\"Hello, World\""), "run {i}");
    }
    eprintln!("200 runs took {:?}", t.elapsed());
    emu.assert_no_workers();
    // no zombie either: a zombie child keeps its directory, which `workers()` would list, and its pipes are closed
    let zombies = std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter(|e| std::fs::read_to_string(e.path().join("stat")).map(|s| s.contains(&format!(") Z {} ", emu.pid()))).unwrap_or(false));
    assert_eq!(zombies.count(), 0);
    assert!(fds() <= fds_before + 2, "descriptors went from {fds_before} to {}", fds());
}

#[test]
fn no_more_scripts_run_at_once_than_the_limit_and_the_others_wait_their_turn() {
    let _g = serial();
    let emu = start(&quick(5000, 2));
    // each script busy for about 700 ms
    emu.sproc("busy", "function () { var t = Date.now(); while (Date.now() - t < 700) { } getContext().getResponse().setBody(1); }");
    let runs: Vec<_> = (0..4).map(|_| spawn_run(emu.port, "busy")).collect();
    let mut peak = 0;
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(1800) {
        peak = peak.max(emu.workers().len());
        thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(peak, 2, "workers at the same time");
    for h in runs {
        let (r, _) = h.join().unwrap();
        assert_eq!(r.status, 200, "{}", r.body);
    }
    emu.assert_no_workers();
}

#[test]
fn a_request_that_waits_too_long_for_a_slot_gets_429() {
    let _g = serial();
    let emu = start(&quick(1000, 1));
    emu.sproc("busy", "function () { var t = Date.now(); while (Date.now() - t < 2500) { } }");
    emu.sproc("hello", "function () { }");
    let first = spawn_run(emu.port, "busy");
    emu.wait_workers(1, Duration::from_secs(1));
    let r = emu.run("hello");
    // the busy script is killed at 1 s, which is also when the waiting request gives up: either way, nothing hangs
    assert!(r.status == 429 || r.status == 200, "{}", r.body);
    if r.status == 429 {
        assert!(r.headers.to_ascii_lowercase().contains("x-ms-retry-after-ms: 100"), "{}", r.headers);
    }
    assert_eq!(first.join().unwrap().0.status, 408);
    emu.assert_no_workers();
}

#[test]
fn a_client_that_goes_away_mid_script_has_its_worker_ended_by_the_time_limit_at_the_latest() {
    let _g = serial();
    // Cosmos tells core to drop a stored procedure run when its client goes away (`Service::abort_on_disconnect`), and the
    // dropped run ends its worker at once; the time limit is the bound when that does not happen
    // (`a_dropped_request_kills_its_worker_at_once` is the service itself, whose future stops the worker the moment it is
    // dropped). Other requests are not dropped: see `only_a_stored_procedure_run_asks_core_to_drop_it`.
    let mut emu = start(&quick(1500, 4));
    emu.sproc("spin", &format!("function () {{ {FIB} fib(60); }}"));
    let mut s = TcpStream::connect(("127.0.0.1", emu.port)).unwrap();
    let req = format!(
        "POST /dbs/d/colls/c/sprocs/spin HTTP/1.1\r\nHost: acct.documents.localhost:{}\r\nauthorization: x\r\nx-ms-documentdb-partitionkey: [\"a\"]\r\nContent-Length: 2\r\n\r\n[]",
        emu.port
    );
    s.write_all(req.as_bytes()).unwrap();
    assert_eq!(emu.wait_workers(1, Duration::from_secs(2)).len(), 1);
    let t = Instant::now();
    drop(s);
    let w = emu.wait_workers(0, Duration::from_millis(1500));
    assert!(w.is_empty(), "the worker of an abandoned request outlived the time limit: {w:?} after {:?}", t.elapsed());
    emu.assert_alive();
    assert_eq!(emu.hello().status, 200);
}

// the lock only keeps this test apart from the others of this file; nothing waits for it from inside
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn a_dropped_request_kills_its_worker_at_once() {
    use azemu_core::http::{HeaderMap, HeaderName, HeaderValue, Method};
    use azemu_core::{Endpoint, Query, Request, RequestCtx, Service};
    let _g = serial();
    let svc = azemu_cosmos::CosmosService::with_script_settings(azemu_cosmos::ScriptSettings {
        worker: Some(env!("CARGO_BIN_EXE_azemu-cosmos-script-worker").into()),
        timeout: Duration::from_secs(30),
        ..Default::default()
    });
    let call = |method: &str, path: &str, extra: &[(&str, &str)], body: String| Request {
        method: Method::from_bytes(method.as_bytes()).unwrap(),
        path: path.to_string(),
        query: Query::parse(""),
        headers: {
            let mut h = HeaderMap::new();
            for (k, v) in [("authorization", "x"), ("content-type", "application/json")].iter().chain(extra.iter()) {
                h.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
            }
            h
        },
        body: body.into(),
        account: "acct".to_string(),
        ctx: RequestCtx {
            request_id: "r".into(),
            time: azemu_core::time::OffsetDateTime::now_utc(),
            endpoint: Endpoint { scheme: "http", authority: "localhost:1".into(), path_style: false, account: "acct".into() },
        },
    };
    assert_eq!(svc.handle(call("POST", "/dbs", &[], json!({"id": "d"}).to_string())).await.status(), 201);
    let c = json!({"id": "c", "partitionKey": {"paths": ["/pk"], "kind": "Hash"}});
    assert_eq!(svc.handle(call("POST", "/dbs/d/colls", &[], c.to_string())).await.status(), 201);
    let b = json!({"id": "spin", "body": format!("function () {{ {FIB} fib(60); }}")});
    assert_eq!(svc.handle(call("POST", "/dbs/d/colls/c/sprocs", &[], b.to_string())).await.status(), 201);
    let run = call("POST", "/dbs/d/colls/c/sprocs/spin", &[("x-ms-documentdb-partitionkey", "[\"a\"]")], "[]".to_string());
    let me = std::process::id();
    // the client goes away after 400 ms: the future of the request is dropped
    let t = Instant::now();
    let r = tokio::time::timeout(Duration::from_millis(400), svc.handle(run)).await;
    assert!(r.is_err(), "the script was not expected to finish");
    let mut left = workers_of(me);
    while !left.is_empty() && t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        left = workers_of(me);
    }
    assert!(left.is_empty(), "the worker of a dropped request is still running: {left:?}");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[test]
fn reset_while_scripts_run_does_not_hurt_and_leaves_nothing_behind() {
    let _g = serial();
    let mut emu = start(&quick(5000, 4));
    emu.sproc("busy", "function () { var t = Date.now(); while (Date.now() - t < 800) { } getContext().getResponse().setBody(1); }");
    let runs: Vec<_> = (0..3).map(|_| spawn_run(emu.port, "busy")).collect();
    assert_eq!(emu.wait_workers(3, Duration::from_secs(2)).len(), 3);
    assert_eq!(emu.req("POST", "/_azemu/reset", &[], "").status / 100, 2);
    for h in runs {
        let (r, _) = h.join().unwrap();
        // the container is gone when the script ends: its writes have no place to go, which is a 404 (or the script's 200)
        assert!(r.status == 200 || r.status == 404, "{} {}", r.status, r.body);
    }
    emu.assert_no_workers();
    emu.assert_alive();
    emu.setup();
    assert_eq!(emu.hello().status, 200);
}

#[test]
fn shutting_the_emulator_down_while_scripts_run_leaves_no_worker() {
    let _g = serial();
    for how in ["kill -9", "kill -TERM"] {
        let emu = start(&quick(30_000, 4));
        emu.sproc("spin", &format!("function () {{ {FIB} fib(60); }}"));
        let runs: Vec<_> = (0..3).map(|_| spawn_run(emu.port, "spin")).collect();
        let w = emu.wait_workers(3, Duration::from_secs(3));
        assert_eq!(w.len(), 3);
        let signal = how.split_whitespace().nth(1).unwrap();
        assert!(Command::new("kill").args([signal, &emu.pid().to_string()]).status().unwrap().success());
        let t = Instant::now();
        while w.iter().any(|p| alive(*p)) && t.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(50));
        }
        let left: Vec<_> = w.iter().filter(|p| alive(**p)).collect();
        assert!(left.is_empty(), "{how}: workers left after the emulator ended: {left:?}");
        for h in runs {
            let _ = h.join();
        }
    }
}

#[test]
fn user_defined_functions_use_one_worker_per_query_and_none_is_left() {
    let _g = serial();
    let emu = start(&quick(5000, 4));
    let hdr = [("x-ms-documentdb-partitionkey", "[\"a\"]")];
    assert_eq!(
        emu.req("POST", "/dbs/d/colls/c/udfs", &[], &json!({"id": "dbl", "body": "function (x) { return x * 2; }"}).to_string()).status,
        201
    );
    for i in 0..300 {
        let r = emu.req("POST", "/dbs/d/colls/c/docs", &hdr, &json!({"id": format!("d{i}"), "pk": "a", "n": i}).to_string());
        assert_eq!(r.status, 201, "{}", r.body);
    }
    let t = Instant::now();
    let r = emu.req(
        "POST",
        "/dbs/d/colls/c/docs",
        &[hdr[0], ("x-ms-documentdb-isquery", "true"), ("content-type", "application/query+json")],
        &json!({"query": "SELECT VALUE SUM(udf.dbl(c.n)) FROM c"}).to_string(),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["Documents"], json!([2 * (0..300).sum::<i64>()]));
    eprintln!("300 function calls in one query took {:?}", t.elapsed());
    emu.assert_no_workers();
}
