//! The emulator's side of a script worker (`worker.rs`): start it, talk to it, limit it, kill it.
//!
//! A worker is a separate process, so the emulator can do what it cannot do to a thread: end it at any moment (`SIGKILL`, or
//! `TerminateProcess`), bound its memory with the operating system, and survive whatever it does (a stack overflow, an abort, an
//! allocation that fails). A [`WorkerProc`] is used for one job and then killed and reaped (also when dropped).
//!
//! Who ends a worker, and why:
//!
//! * the time limit: a task on the async runtime (the "watchdog") kills it at the script's deadline plus a short grace, in which
//!   the engine normally reports the time-out itself;
//! * a cancelled request (its future was dropped, for instance because the emulator is shutting down): the [`CancelOnDrop`] of the
//!   request kills every worker of that request, from the thread that drops it;
//! * the worker itself: the operating system ends it at its processor-time or memory limit, and it ends when the emulator's
//!   pipe closes (the emulator exited or crashed).
//!
//! The emulator never waits for a worker in the async runtime: the thread that serves the worker's collection calls is a
//! blocking thread (`spawn_blocking`), no service lock is held while it waits, and the watchdog is what ends a wait that would
//! not end.

use std::cell::RefCell;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use super::proto::{read_frame, write_frame, FrameError, FromWorker, ToWorker, WorkerConfig, MAX_FROM_WORKER, MAX_REPLY};
use super::worker::HIDDEN_ARG;
use super::{Failure, Outcome, ScriptSettings};

const WHY_NONE: u8 = 0;
const WHY_TIMEOUT: u8 = 1;
const WHY_CANCELLED: u8 = 2;

type ChildCell = Mutex<Option<Child>>;

/// The workers started for one request. When the request is cancelled all of them are killed, at once and from whatever thread
/// drops the guard (so it also works while the async runtime is shutting down and no task runs any more).
#[derive(Default)]
struct Registry {
    cancelled: std::sync::atomic::AtomicBool,
    /// Set (before `cancel`) when a reset ended the run, so that the request can answer "not found" as for a dropped container.
    by_reset: std::sync::atomic::AtomicBool,
    workers: Mutex<Vec<(std::sync::Weak<ChildCell>, Arc<AtomicU8>)>>,
}

impl Registry {
    fn add(&self, child: &Arc<ChildCell>, why: &Arc<AtomicU8>) {
        let mut w = self.workers.lock();
        w.retain(|(c, _)| c.strong_count() > 0);
        w.push((Arc::downgrade(child), why.clone()));
        drop(w);
        if self.cancelled.load(Ordering::SeqCst) {
            kill_child(child, why, WHY_CANCELLED);
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        for (c, why) in self.workers.lock().iter() {
            if let Some(c) = c.upgrade() {
                kill_child(&c, why, WHY_CANCELLED);
            }
        }
    }
}

/// What a request gives to the code that runs for it on a blocking thread: the list of its workers (so that cancelling the
/// request kills them), and the runtime on which the watchdogs of its workers run.
#[derive(Clone)]
pub struct Exec {
    reg: Arc<Registry>,
    handle: Handle,
}

/// Held by the async side of a request. When it is dropped (the request finished, or its future was dropped because the client
/// went away or the emulator is shutting down), every worker started for the request is killed.
pub struct CancelOnDrop(Arc<Registry>);

impl CancelOnDrop {
    /// A handle that lets someone else end this request's workers: the service keeps one per running script so that a reset
    /// (all accounts, or one) can stop them at once.
    pub fn handle(&self) -> RunHandle {
        RunHandle(self.0.clone())
    }
}

/// Ends the workers of one request from outside; see [`CancelOnDrop::handle`].
#[derive(Clone)]
pub struct RunHandle(Arc<Registry>);

impl RunHandle {
    /// Kills every worker of the request, and the ones it starts later; the request answers as if its container was removed.
    pub fn end_for_reset(&self) {
        self.0.by_reset.store(true, Ordering::SeqCst);
        self.0.cancel();
    }

    pub fn was_ended_by_reset(&self) -> bool {
        self.0.by_reset.load(Ordering::SeqCst)
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Call on the async side, inside the runtime.
pub fn exec_pair() -> (CancelOnDrop, Exec) {
    let reg = Arc::new(Registry::default());
    (CancelOnDrop(reg.clone()), Exec { reg, handle: Handle::current() })
}

thread_local! {
    static EXEC: RefCell<Option<Exec>> = const { RefCell::new(None) };
}

/// Makes `exec` the one workers started on this thread belong to, until the guard is dropped.
pub struct Entered;

pub fn enter(exec: Exec) -> Entered {
    EXEC.with(|e| *e.borrow_mut() = Some(exec));
    Entered
}

impl Drop for Entered {
    fn drop(&mut self) {
        EXEC.with(|e| *e.borrow_mut() = None);
    }
}

/// The program that runs a worker.
fn program(settings: &ScriptSettings) -> Result<PathBuf, Failure> {
    if let Some(p) = &settings.worker {
        return Ok(p.clone());
    }
    std::env::current_exe().map_err(|e| Failure::Worker(format!("The script worker cannot be started: the path of the running program is not known ({e}). Set AZEMU_SCRIPT_WORKER (or ScriptSettings::worker).")))
}

pub struct WorkerProc {
    child: Arc<ChildCell>,
    why: Arc<AtomicU8>,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: Option<ChildStderr>,
    watchdog: JoinHandle<()>,
}

fn kill_child(child: &ChildCell, why: &AtomicU8, reason: u8) {
    let _ = why.compare_exchange(WHY_NONE, reason, Ordering::SeqCst, Ordering::SeqCst);
    if let Some(c) = child.lock().as_mut() {
        let _ = c.kill();
    }
}

impl WorkerProc {
    /// Starts a worker that is killed at `deadline` plus the grace of the settings. Must be called on a thread that has entered an
    /// [`Exec`] (the blocking threads of `lib.rs` do).
    pub fn start(settings: &ScriptSettings, deadline: Instant) -> Result<WorkerProc, Failure> {
        if cfg!(not(feature = "scripts")) {
            return Err(Failure::NotBuilt);
        }
        let exec = EXEC
            .with(|e| e.borrow().clone())
            .ok_or_else(|| Failure::Worker("A script was started outside a request, which is a bug in the emulator.".into()))?;
        let program = program(settings)?;
        let cfg = WorkerConfig {
            memory_growth: settings.memory_growth as u64,
            // a little more processor time than wall-clock time: the OS ends a worker the emulator could not
            cpu_secs: settings.timeout.as_secs() + 3,
            engine: settings.engine_limits(),
        };
        let cfg_json = serde_json::to_string(&cfg).map_err(|e| Failure::Internal(e.to_string()))?;
        let mut cmd = Command::new(&program);
        cmd.arg(HIDDEN_ARG).arg(cfg_json).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // nothing of the emulator's environment (it can hold secrets) reaches the worker; one allocator arena keeps the address
        // space limit simple
        cmd.env_clear().env("MALLOC_ARENA_MAX", "1");
        #[cfg(windows)]
        {
            // the system directory is needed to load the runtime libraries
            if let Some(root) = std::env::var_os("SystemRoot") {
                cmd.env("SystemRoot", root);
            }
        }
        let mut child = cmd.spawn().map_err(|e| {
            Failure::Worker(format!("The script worker could not be started ({}): {e}. Scripts cannot run without it.", program.display()))
        })?;
        let (stdin, stdout, stderr) = match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
            (Some(i), Some(o), Some(e)) => (i, o, e),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Worker("The script worker started without its pipes.".into()));
            }
        };
        let child = Arc::new(Mutex::new(Some(child)));
        let why = Arc::new(AtomicU8::new(WHY_NONE));
        let kill_at = tokio::time::Instant::from_std(deadline + settings.kill_grace);
        exec.reg.add(&child, &why);
        let watchdog = {
            let (child, why) = (child.clone(), why.clone());
            exec.handle.spawn(async move {
                tokio::time::sleep_until(kill_at).await;
                kill_child(&child, &why, WHY_TIMEOUT);
            })
        };
        Ok(WorkerProc { child, why, stdin: Some(stdin), stdout, stderr: Some(stderr), watchdog })
    }

    pub fn send(&mut self, msg: &ToWorker) -> Result<(), Failure> {
        let Some(stdin) = self.stdin.as_mut() else { return Err(self.ended()) };
        match write_frame(stdin, msg) {
            Ok(()) => Ok(()),
            Err(_) => Err(self.ended()),
        }
    }

    /// The next message of the worker. A message that is too large or not in the language of `proto.rs` ends the worker.
    pub fn recv(&mut self) -> Result<FromWorker, Failure> {
        match read_frame::<_, FromWorker>(&mut self.stdout, MAX_FROM_WORKER) {
            Ok(m) => Ok(m),
            Err(FrameError::Closed) => Err(self.ended()),
            Err(FrameError::TooLarge(n)) => {
                self.kill();
                Err(Failure::Limit(format!(
                    "The script produced a message of {} MB, more than the {} MB the emulator accepts from a script.",
                    n / (1024 * 1024),
                    MAX_FROM_WORKER / (1024 * 1024)
                )))
            }
            Err(FrameError::Garbage(m)) => {
                self.kill();
                Err(Failure::Worker(format!("The script worker sent something that is not a message ({m}).")))
            }
        }
    }

    fn kill(&mut self) {
        kill_child(&self.child, &self.why, 255);
    }

    /// The worker's pipe closed: find out why it ended.
    fn ended(&mut self) -> Failure {
        let status = {
            let mut guard = self.child.lock();
            let mut status = None;
            if let Some(c) = guard.as_mut() {
                // the pipe closes a moment before the exit status can be read; a worker that closed its pipe and lives on is killed
                let started = Instant::now();
                loop {
                    match c.try_wait() {
                        Ok(Some(s)) => {
                            status = Some(s);
                            break;
                        }
                        Ok(None) if started.elapsed() < Duration::from_millis(500) => std::thread::sleep(Duration::from_millis(2)),
                        _ => {
                            let _ = c.kill();
                            status = c.wait().ok();
                            break;
                        }
                    }
                }
            }
            status
        };
        let mut text = String::new();
        if let Some(mut e) = self.stderr.take() {
            let _ = (&mut e).take(8 * 1024).read_to_string(&mut text);
        }
        match self.why.load(Ordering::SeqCst) {
            WHY_TIMEOUT => return Failure::Timeout,
            WHY_CANCELLED => return Failure::Cancelled,
            _ => {}
        }
        if text.contains("memory allocation of") {
            return Failure::Memory;
        }
        if text.contains("overflowed its stack") {
            return Failure::Limit("The script nested its code or its data too deeply: the script engine ran out of stack.".to_string());
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            // SIGXCPU: the processor-time limit the worker set on itself
            if status.and_then(|s| s.signal()) == Some(24) {
                return Failure::Timeout;
            }
        }
        let how = status.map(|s| s.to_string()).unwrap_or_else(|| "unknown".to_string());
        let detail = text.lines().rev().find(|l| !l.trim().is_empty()).map(|l| format!(": {}", l.trim())).unwrap_or_default();
        Failure::Worker(format!("The script worker ended unexpectedly ({how}){detail}"))
    }
}

impl Drop for WorkerProc {
    fn drop(&mut self) {
        self.watchdog.abort();
        self.stdin = None;
        if let Some(mut c) = self.child.lock().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Anything the worker said that is really "the memory ran out" (a panic that carries an allocation error) is the memory limit.
fn classify(f: Failure) -> Failure {
    match f {
        Failure::Internal(m) if m.contains("memory allocation") || m.contains("AllocError") => Failure::Memory,
        other => other,
    }
}

/// Runs a stored procedure or trigger body in a fresh worker. `host` answers the collection calls the worker asks for; it is
/// called on this (blocking) thread, one call at a time.
pub fn run_job(
    settings: &ScriptSettings,
    deadline: Instant,
    body: &str,
    cfg: Value,
    args: &Value,
    panic: bool,
    host: &mut dyn FnMut(&str, &str) -> String,
) -> Result<Outcome, Failure> {
    let mut w = WorkerProc::start(settings, deadline)?;
    // the host refuses collection calls after `max_operations` of them, but a worker that keeps asking is still asking: a ceiling
    let max_calls = settings.max_operations.saturating_mul(20).saturating_add(1000);
    let mut calls = 0usize;
    let remaining_ms = deadline.saturating_duration_since(Instant::now()).as_millis() as u64;
    w.send(&ToWorker::Run { body: body.to_string(), cfg, args: args.clone(), remaining_ms, panic }).map_err(classify)?;
    loop {
        match w.recv().map_err(classify)? {
            FromWorker::Host { op, payload } => {
                if op != "log" {
                    calls += 1;
                    if calls > max_calls {
                        w.kill();
                        return Err(Failure::Limit(format!("The script made more than {max_calls} collection calls.")));
                    }
                }
                let mut text = host(&op, &payload);
                if op == "log" {
                    continue;
                }
                if text.len() > MAX_REPLY {
                    text = r#"{"accepted":true,"error":{"status":413,"message":"{\"Errors\":[\"The result is too large for a script to receive.\"]}","body":"{}"}}"#.to_string();
                }
                w.send(&ToWorker::Reply { text }).map_err(classify)?;
            }
            FromWorker::Done { outcome } => return Ok(outcome),
            FromWorker::Failed { failure } => return Err(classify(failure)),
            FromWorker::UdfDone { .. } => {
                w.kill();
                return Err(Failure::Worker("The script worker answered a question nobody asked.".into()));
            }
        }
    }
}

/// A worker that serves the calls of the user-defined functions of one query (one worker per query, not per row).
pub struct UdfWorker {
    w: WorkerProc,
    deadline: Instant,
    defined: std::collections::HashSet<String>,
}

impl UdfWorker {
    pub fn start(settings: &ScriptSettings, deadline: Instant) -> Result<UdfWorker, Failure> {
        Ok(UdfWorker { w: WorkerProc::start(settings, deadline)?, deadline, defined: Default::default() })
    }

    /// Calls function `name` (its text `def` is sent the first time). `Ok("")` is "undefined".
    pub fn call(&mut self, name: &str, def: &str, args: &[Option<Value>]) -> Result<String, Failure> {
        let first = self.defined.insert(name.to_string());
        let msg = ToWorker::Udf {
            name: name.to_string(),
            def: first.then(|| def.to_string()),
            args: args.iter().map(|a| a.clone().unwrap_or(Value::Null)).collect(),
            undefined: args.iter().enumerate().filter(|(_, a)| a.is_none()).map(|(i, _)| i).collect(),
            remaining_ms: self.deadline.saturating_duration_since(Instant::now()).as_millis() as u64,
        };
        self.w.send(&msg).map_err(classify)?;
        match self.w.recv().map_err(classify)? {
            FromWorker::UdfDone { text } => Ok(text),
            FromWorker::Failed { failure } => Err(classify(failure)),
            _ => {
                self.w.kill();
                Err(Failure::Worker("The script worker answered a function call with something else.".into()))
            }
        }
    }
}
