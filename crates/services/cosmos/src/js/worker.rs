//! The script worker: the process that runs the JavaScript engine (`azemu __cosmos-script-worker <settings>`).
//!
//! It is the same executable as the emulator (or the small `azemu-cosmos-script-worker` program of this crate), started by
//! `proc.rs` with a hidden first argument that `main` looks at before anything else starts. One worker serves one stored
//! procedure or trigger run, or all function calls of one query, and is then killed.
//!
//! What it does, in order:
//!
//! 1. sets a processor-time limit on itself (`RLIMIT_CPU`, a little above the emulator's time limit; Linux and macOS);
//! 2. starts a thread that reads the emulator's messages from standard input and **ends the process when the emulator goes away**
//!    (standard input closes), so a worker is never left behind, even one that is stuck in a native call;
//! 3. loads the engine and the script API on a thread with a large stack (reserved, not used: pages count when touched), then
//!    limits its address space to what it uses now plus the memory a script may add (`RLIMIT_AS`, Linux only). A script that
//!    allocates more than that makes an allocation fail and the worker end; the emulator reports it as the memory limit;
//! 4. waits for a job. The job's collection calls go out as messages and come back as answers; nothing else is reachable.
//!
//! The worker opens no file and no socket and does not need any. The emulator starts it with an empty environment and only
//! the three standard streams (everything else the emulator holds is close-on-exec).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::proto::{read_frame, write_frame, FromWorker, ToWorker, WorkerConfig, MAX_TO_WORKER};
#[cfg(feature = "scripts")]
use super::Session;
use super::{Failure, Host};

/// The word that makes `azemu` or `azemu-cosmos` behave as a script worker. It is not shown in `--help`.
pub const HIDDEN_ARG: &str = "__cosmos-script-worker";

/// Stack of the thread the engine runs on. A deep recursion through native code (a `sort` comparator that sorts) and the deepest
/// text that `check_source` lets through need far more than the usual 2 MB: measured in a debug build, 32 MiB overflows and 64 MiB
/// does not (`the_deepest_allowed_text_fits_the_workers_stack`), so this is twice the larger. The space is reserved, not touched.
/// It is the worker's own stack: running out of it ends the worker only (the emulator reports an error for that request).
pub const ENGINE_STACK: usize = 128 * 1024 * 1024;
/// Address space above "what the engine uses once loaded plus the memory growth" that is let through: the thread that reads the
/// emulator's messages, allocator bookkeeping.
const SPACE_SLACK: u64 = 64 * 1024 * 1024;

/// Exit codes the emulator may see (anything else is a crash or a signal).
const EXIT_BAD_START: i32 = 2;
const EXIT_PROTOCOL: i32 = 3;
const EXIT_ENGINE: i32 = 4;

/// If this process was started as a worker (first argument [`HIDDEN_ARG`]), becomes one and never returns. Call it first in `main`.
pub fn run_if_requested() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HIDDEN_ARG) {
        run(args);
    }
}

/// Runs as a worker. `args` are the command line arguments without the program name: the hidden word (optional) and the settings.
pub fn run(args: Vec<String>) -> ! {
    let cfg: WorkerConfig = match args.iter().find(|a| a.as_str() != HIDDEN_ARG).and_then(|a| serde_json::from_str(a).ok()) {
        Some(c) => c,
        None => {
            eprintln!("script worker: the settings argument is missing or not valid");
            std::process::exit(EXIT_BAD_START);
        }
    };
    limit_cpu(cfg.cpu_secs);
    let (tx, rx) = channel::<ToWorker>();
    let reader = std::thread::Builder::new().name("script-worker-input".into()).spawn(move || {
        let mut stdin = std::io::stdin().lock();
        loop {
            match read_frame::<_, ToWorker>(&mut stdin, MAX_TO_WORKER) {
                Ok(m) => {
                    if tx.send(m).is_err() {
                        return;
                    }
                }
                // the emulator ended or closed the pipe: nothing is left to do, even if the engine is busy in a native call
                Err(_) => std::process::exit(0),
            }
        }
    });
    if reader.is_err() {
        eprintln!("script worker: could not start the input thread");
        std::process::exit(EXIT_BAD_START);
    }
    let engine =
        std::thread::Builder::new().name("script-worker-engine".into()).stack_size(ENGINE_STACK).spawn(move || engine_loop(cfg, rx));
    let code = match engine.map(|h| h.join()) {
        Ok(Ok(code)) => code,
        _ => EXIT_ENGINE,
    };
    std::process::exit(code)
}

fn send(msg: &FromWorker) {
    let mut out = std::io::stdout().lock();
    if write_frame(&mut out, msg).is_err() {
        // the emulator is gone
        std::process::exit(0);
    }
}

/// Asks the emulator for the collection calls of the script and waits for each answer.
struct PipeHost {
    rx: Rc<Receiver<ToWorker>>,
}

impl Host for PipeHost {
    fn call(&mut self, op: &str, payload: &str) -> String {
        send(&FromWorker::Host { op: op.to_string(), payload: payload.to_string() });
        if op == "log" {
            // a log line has no answer on the emulator's side either
            return String::new();
        }
        match self.rx.recv() {
            Ok(ToWorker::Reply { text }) => text,
            Ok(_) => {
                eprintln!("script worker: the emulator sent a message that is not an answer");
                std::process::exit(EXIT_PROTOCOL)
            }
            Err(_) => std::process::exit(0),
        }
    }
}

fn panic_text(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown".to_string())
}

#[cfg(not(feature = "scripts"))]
fn engine_loop(_cfg: WorkerConfig, _rx: Receiver<ToWorker>) -> i32 {
    send(&FromWorker::Failed { failure: Failure::NotBuilt });
    EXIT_ENGINE
}

#[cfg(feature = "scripts")]
fn engine_loop(cfg: WorkerConfig, rx: Receiver<ToWorker>) -> i32 {
    let rx = Rc::new(rx);
    let far = Instant::now() + Duration::from_secs(3600);
    let mut session = match Session::new(&cfg.engine, far) {
        Ok(s) => s,
        Err(f) => {
            send(&FromWorker::Failed { failure: f });
            return EXIT_ENGINE;
        }
    };
    limit_memory(cfg.memory_growth);
    let udf_call = RefCell::new(None::<boa_engine::Script>);
    loop {
        let Ok(msg) = rx.recv() else { return 0 };
        match msg {
            ToWorker::Run { body, cfg: run_cfg, args, remaining_ms, panic } => {
                session.set_deadline(Instant::now() + Duration::from_millis(remaining_ms));
                session.set_host(Some(Box::new(PipeHost { rx: rx.clone() })));
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if panic {
                        panic!("injected script engine panic");
                    }
                    run_job(&mut session, &body, &run_cfg, &args)
                }));
                let reply = match r {
                    Ok(Ok(outcome)) => FromWorker::Done { outcome },
                    Ok(Err(failure)) => FromWorker::Failed { failure },
                    Err(p) => FromWorker::Failed { failure: Failure::Internal(format!("the script engine panicked: {}", panic_text(p))) },
                };
                send(&reply);
                // one job per worker: the emulator ends it now, and if it does not, it ends here
                return 0;
            }
            ToWorker::Udf { name, def, args, undefined, remaining_ms } => {
                session.set_deadline(Instant::now() + Duration::from_millis(remaining_ms));
                session.set_host(None);
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call_udf(&mut session, &udf_call, &name, def.as_deref(), &args, &undefined)
                }));
                let reply = match r {
                    Ok(Ok(text)) => FromWorker::UdfDone { text },
                    Ok(Err(failure)) => FromWorker::Failed { failure },
                    Err(p) => FromWorker::Failed { failure: Failure::Internal(format!("the script engine panicked: {}", panic_text(p))) },
                };
                send(&reply);
            }
            ToWorker::Reply { .. } => {
                eprintln!("script worker: an answer arrived without a question");
                return EXIT_PROTOCOL;
            }
        }
    }
}

#[cfg(feature = "scripts")]
fn run_job(session: &mut Session, body: &str, cfg: &Value, args: &Value) -> Result<super::Outcome, Failure> {
    session.set_global_json("__cfg", cfg)?;
    session.run_source("__azemu.init(__cfg)")?;
    session.run_function(body, args)
}

#[cfg(feature = "scripts")]
fn call_udf(
    session: &mut Session,
    call: &RefCell<Option<boa_engine::Script>>,
    name: &str,
    def: Option<&str>,
    args: &[Value],
    undefined: &[usize],
) -> Result<String, Failure> {
    if call.borrow().is_none() {
        *call.borrow_mut() = Some(session.prepare("__azemu.callUdf(__n, __a, __u)")?);
    }
    if let Some(def) = def {
        let key = Value::String(name.to_string()).to_string();
        session.run_source(&format!("__azemu.defineUdf({key}, ({def}\n));"))?;
    }
    session.set_global_text("__n", name)?;
    session.set_global_text("__a", &Value::Array(args.to_vec()).to_string())?;
    session.set_global_text("__u", &serde_json::to_string(undefined).unwrap_or_else(|_| "[]".into()))?;
    let script = call.borrow().clone().ok_or_else(|| Failure::Internal("the function call script is gone".into()))?;
    session.run_prepared_text(&script)
}

/// Processor time: `cpu_secs` of CPU, then the operating system ends the worker (the signal `SIGXCPU`; a second later `SIGKILL`).
#[cfg(unix)]
fn limit_cpu(cpu_secs: u64) {
    if cpu_secs > 0 {
        let _ = rlimit::setrlimit(rlimit::Resource::CPU, cpu_secs, cpu_secs + 1);
    }
}

#[cfg(not(unix))]
fn limit_cpu(_cpu_secs: u64) {}

/// The address space of this process now, in bytes (Linux). Read from `VmSize` in `/proc/self/status`, which the kernel reports in
/// kilobytes; `/proc/self/statm` counts pages, and the size of a page (4 KiB, 16 KiB or 64 KiB, depending on the kernel) is not
/// known here without a system call.
#[cfg(target_os = "linux")]
fn address_space() -> Option<u64> {
    parse_vm_size(&std::fs::read_to_string("/proc/self/status").ok()?)
}

/// `VmSize:   204800 kB` in the text of `/proc/self/status`, as bytes. Only a line in kilobytes is understood.
#[cfg(target_os = "linux")]
fn parse_vm_size(status: &str) -> Option<u64> {
    let rest = status.lines().find_map(|l| l.strip_prefix("VmSize:"))?;
    let mut parts = rest.split_ascii_whitespace();
    let kb: u64 = parts.next()?.parse().ok()?;
    (parts.next()? == "kB").then(|| kb.saturating_mul(1024))
}

/// Address space: what the worker uses now plus `growth`. Only Linux enforces this the same way for every allocation; macOS
/// accepts the setting but does not enforce it for ordinary allocations, and it has no cheap way to read the current size, so it
/// is not set there. Windows has no such limit for a plain process.
#[cfg(target_os = "linux")]
fn limit_memory(growth: u64) {
    if let Some(now) = address_space() {
        let limit = now.saturating_add(growth).saturating_add(SPACE_SLACK);
        let _ = rlimit::setrlimit(rlimit::Resource::AS, limit, limit);
    }
}

#[cfg(not(target_os = "linux"))]
fn limit_memory(_growth: u64) {}

#[cfg(all(test, target_os = "linux"))]
mod address_space_tests {
    use super::*;

    const STATUS: &str =
        "Name:\tazemu\nUmask:\t0022\nVmPeak:\t  210000 kB\nVmSize:\t  204800 kB\nVmLck:\t       0 kB\nRssAnon:\t   1000 kB\n";

    #[test]
    fn the_size_comes_from_vmsize_in_kilobytes_whatever_the_page_size() {
        // 204800 kB is 200 MiB: the same on 4 KiB, 16 KiB and 64 KiB page kernels, because the kernel already did the multiplication
        assert_eq!(parse_vm_size(STATUS), Some(204_800 * 1024));
        // the line is found wherever it stands, and the "VmPeak" line before it is not mistaken for it
        assert_eq!(parse_vm_size("VmSize:\t1 kB\n"), Some(1024));
    }

    #[test]
    fn a_status_without_a_readable_vmsize_gives_nothing() {
        assert_eq!(parse_vm_size(""), None);
        assert_eq!(parse_vm_size("Name:\tazemu\nVmPeak:\t 5 kB\n"), None);
        assert_eq!(parse_vm_size("VmSize:\tlots kB\n"), None);
        assert_eq!(parse_vm_size("VmSize:\t 5 MB\n"), None, "only kilobytes are understood; anything else is not guessed");
    }

    /// On this machine: the page size the kernel reports for this process (`KernelPageSize` in smaps) times the pages in statm is the
    /// size the worker must see. A fixed 4096 would be off by a factor of 4 or 16 on a kernel with larger pages.
    #[test]
    fn the_size_agrees_with_pages_times_the_real_page_size() {
        let smaps = std::fs::read_to_string("/proc/self/smaps").expect("smaps");
        let page_kb: u64 = smaps
            .lines()
            .find_map(|l| l.strip_prefix("KernelPageSize:"))
            .and_then(|r| r.split_ascii_whitespace().next()?.parse().ok())
            .expect("KernelPageSize");
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
        let pages: u64 = statm.split_ascii_whitespace().next().unwrap().parse().unwrap();
        let expected = pages * page_kb * 1024;
        let got = address_space().expect("address space");
        let diff = got.abs_diff(expected);
        assert!(diff * 10 <= expected, "address_space {got} against pages x page size {expected}");
    }
}
