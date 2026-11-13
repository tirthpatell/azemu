//! The JavaScript engine behind stored procedures, triggers and user-defined functions.
//!
//! The engine is `boa_engine` (pure Rust, no C toolchain, no V8). **It never runs in the emulator's process**: a script runs in
//! a separate worker process (`worker.rs` is the worker, `proc.rs` is how the emulator starts, serves, limits and kills it,
//! `proto.rs` is the language they speak). A JavaScript engine inside the emulator cannot be stopped from outside (a native call
//! does not yield), cannot be given a hard memory limit, and takes the whole process with it when it overflows its stack.
//!
//! What is here:
//!
//! * [`Session`]: one engine context with the server-side API of `prelude.js` loaded. It lives in the worker process.
//! * First-line limits that give friendly errors (nothing depends on them for safety any more): a wall-clock limit checked every
//!   few VM instructions (the engine's "run with a budget" mode), a loop limit and a recursion limit per call, and calls that
//!   build a big value in one step (`repeat`, `padStart`, `fill`, `join`, ...) wrapped in `prelude.js` to refuse sizes over a
//!   fixed limit. What stops a script that gets past them is the worker's operating system limits and the kill by the emulator.
//!   See `docs/services/cosmos-scripts.md`.
//! * The one native function, `__host(op, json)`, reaches the [`Host`] of the thread (kept in a thread local so the native
//!   function needs no captured state, which the engine would only accept through `unsafe`). In the worker the host sends the
//!   request to the emulator and waits for the answer.

// Without the `scripts` feature the engine's side of the worker is not built, and what only it uses is unused.
#![cfg_attr(not(feature = "scripts"), allow(dead_code, unused_imports))]

#[cfg(feature = "scripts")]
use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::task::{Context as TaskCx, Poll, Waker};
use std::time::{Duration, Instant};

#[cfg(feature = "scripts")]
use boa_engine::error::RuntimeLimitError;
#[cfg(feature = "scripts")]
use boa_engine::{js_string, Context, JsResult, JsValue, NativeFunction, Script, Source};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod proc;
pub mod proto;
pub mod worker;

const PRELUDE: &str = include_str!("prelude.js");

/// Settings of script execution. The defaults are Azure's where Azure documents a number.
#[derive(Clone, Debug)]
pub struct ScriptSettings {
    /// Longest a script (stored procedure, trigger) or all UDF calls of one query may run. Azure: 5 seconds.
    pub timeout: Duration,
    /// Collection calls one execution may make before they answer `false` ("not accepted", the documented way a
    /// script learns it should stop and let the client call it again).
    pub max_operations: usize,
    /// Loop iterations one function call may run (all its loops together).
    pub loop_iterations: u64,
    /// Nested calls (and nested native-to-script calls) allowed.
    pub recursion_depth: usize,
    /// Memory a script may add to the worker process, in bytes. The worker limits its own address space to what it uses when it
    /// is ready plus this (Linux; see `worker.rs`). Beyond that an allocation fails and the worker ends.
    pub memory_growth: usize,
    /// Largest response body, in bytes. Azure: 2 MB.
    pub max_response: usize,
    /// Scripts that may run at the same time; the others wait for their turn.
    pub max_running: usize,
    /// Instruction cost between two looks at the clock.
    pub slice: u32,
    /// The program that runs a script worker (`<program> __cosmos-script-worker <settings>`). `None`: the executable that is running
    /// (`ScriptSettings::from_env` fills it from the setting `AZEMU_SCRIPT_WORKER`) (`azemu` and `azemu-cosmos` handle the hidden argument).
    /// Embedders and tests whose current executable cannot be the worker set it (the crate has `azemu-cosmos-script-worker`).
    pub worker: Option<PathBuf>,
    /// How long after the time limit the emulator waits for the worker to report the time-out itself, before it kills it.
    pub kill_grace: Duration,
}

impl Default for ScriptSettings {
    fn default() -> Self {
        ScriptSettings {
            timeout: Duration::from_secs(5),
            max_operations: 10_000,
            loop_iterations: 5_000_000,
            recursion_depth: 1000,
            memory_growth: 192 * 1024 * 1024,
            max_response: 2 * 1024 * 1024,
            max_running: 4,
            slice: 96,
            worker: None,
            kill_grace: Duration::from_millis(300),
        }
    }
}

impl ScriptSettings {
    /// The defaults, changed by the settings that are set (read through `azemu_core::settings`, described for `docs/SETTINGS.md`):
    /// `AZEMU_COSMOS_SCRIPT_TIMEOUT_MS`, `_MAX_OPERATIONS`, `_LOOP_ITERATIONS`, `_MEMORY_MB`, `_MAX_RUNNING`, and
    /// `AZEMU_SCRIPT_WORKER` (the program that runs a worker). A value of 0 means "the default". Read once, here: the worker
    /// process gets what it needs from its own command line (`proc.rs`) and reads no setting itself.
    pub fn from_env() -> ScriptSettings {
        use azemu_core::settings;
        settings::describe(
            "AZEMU_COSMOS_SCRIPT_TIMEOUT_MS",
            "5000",
            "Cosmos: milliseconds a stored procedure, trigger or user-defined function may run before its worker is stopped.",
        );
        settings::describe(
            "AZEMU_COSMOS_SCRIPT_MAX_OPERATIONS",
            "10000",
            "Cosmos: most collection calls (reads, writes, queries) one script may make.",
        );
        settings::describe("AZEMU_COSMOS_SCRIPT_LOOP_ITERATIONS", "5000000", "Cosmos: most loop iterations one script may run.");
        settings::describe(
            "AZEMU_COSMOS_SCRIPT_MEMORY_MB",
            "192",
            "Cosmos: megabytes of memory one script worker may use beyond its start.",
        );
        settings::describe(
            "AZEMU_COSMOS_SCRIPT_MAX_RUNNING",
            "4",
            "Cosmos: scripts that may run at the same time; more wait their turn, then answer 429.",
        );
        settings::describe(
            "AZEMU_SCRIPT_WORKER",
            "the emulator's own program",
            "Cosmos: the program that runs a script worker (it must handle the worker's hidden first argument); only embedders and tests need it.",
        );
        let mut s = ScriptSettings::default();
        let nonzero = |name: &str, default: u64| Some(settings::number(name, default)).filter(|v| *v > 0);
        if let Some(v) = nonzero("AZEMU_COSMOS_SCRIPT_TIMEOUT_MS", s.timeout.as_millis() as u64) {
            s.timeout = Duration::from_millis(v);
        }
        if let Some(v) = nonzero("AZEMU_COSMOS_SCRIPT_MAX_OPERATIONS", s.max_operations as u64) {
            s.max_operations = v as usize;
        }
        if let Some(v) = nonzero("AZEMU_COSMOS_SCRIPT_LOOP_ITERATIONS", s.loop_iterations) {
            s.loop_iterations = v;
        }
        if let Some(v) = nonzero("AZEMU_COSMOS_SCRIPT_MEMORY_MB", (s.memory_growth / (1024 * 1024)) as u64) {
            s.memory_growth = (v as usize).saturating_mul(1024 * 1024);
        }
        if let Some(v) = nonzero("AZEMU_COSMOS_SCRIPT_MAX_RUNNING", s.max_running as u64) {
            s.max_running = v as usize;
        }
        s.worker = settings::text("AZEMU_SCRIPT_WORKER").map(PathBuf::from);
        s
    }
}

/// What a finished script left: the response body and the (pre-trigger) request body, as JSON text.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Outcome {
    pub body: Option<String>,
    pub body_set: bool,
    pub request_body: Option<String>,
    pub request_body_set: bool,
}

/// Why a script stopped without finishing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Failure {
    /// The script threw (or did not compile): the text Azure puts after "Exception =".
    Thrown(String),
    /// `getContext().abort(err)`.
    Aborted(String),
    /// The time limit passed.
    Timeout,
    /// The worker ran out of the memory it was allowed (an allocation failed, so the worker ended).
    Memory,
    /// A loop, recursion or stack limit of the engine.
    Limit(String),
    /// A bug in the engine glue, or a worker that stopped without saying why.
    Internal(String),
    /// The worker did not answer in the language of `proto.rs`, or ended in a way that no limit explains.
    Worker(String),
    /// The request was cancelled while the script ran.
    Cancelled,
    /// This build was made without the `scripts` feature: there is no script engine to run it.
    NotBuilt,
}

/// What a script reaches through `__host`. In the worker this sends the call to the emulator (`worker.rs`).
pub trait Host {
    /// `op` and the JSON `payload` text come from `prelude.js`; the answer is JSON text (or empty for "nothing").
    fn call(&mut self, op: &str, payload: &str) -> String;
}

/// `eval` and the `Function` constructor compile text at run time, and the compiler recurses on the nesting of that text.
/// A stored script is checked before it runs (`check_source`); text a script builds could not be, so it is refused.
#[cfg(feature = "scripts")]
struct NoCodeFromStrings;

#[cfg(feature = "scripts")]
impl boa_engine::context::HostHooks for NoCodeFromStrings {
    fn ensure_can_compile_strings(
        &self,
        _realm: boa_engine::realm::Realm,
        _parameters: &[boa_engine::JsString],
        _body: &boa_engine::JsString,
        _direct: bool,
        _context: &mut Context,
    ) -> JsResult<()> {
        Err(boa_engine::JsNativeError::eval()
            .with_message("eval and the Function constructor are not supported in server-side scripts")
            .into())
    }
}

/// Longest script text that runs (the compiler needs stack in proportion to the length of an expression chain).
pub const MAX_SOURCE_BYTES: usize = 32 * 1024;
/// Deepest bracket nesting in a script's text.
pub const MAX_SOURCE_DEPTH: usize = 400;
/// Longest run of prefix operators (`!!!!`, `- - -`, `typeof void`) in a script's text.
pub const MAX_PREFIX_RUN: usize = 64;
/// Reads the text of a template literal from `i` (just after the opening backtick or the `}` that closes a `${`) up to the
/// closing backtick, or up to the next `${`, which is counted as an open bracket and remembered in `open` so that the `}`
/// that closes it brings the scan back here. Returns where normal scanning goes on.
fn scan_template(b: &[u8], mut i: usize, depth: &mut usize, open: &mut Vec<usize>) -> Result<usize, String> {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'`' => return Ok(i + 1),
            b'$' if b.get(i + 1) == Some(&b'{') => {
                open.push(*depth);
                *depth += 1;
                if *depth > MAX_SOURCE_DEPTH {
                    return Err(format!("The script nests brackets more than {MAX_SOURCE_DEPTH} levels deep."));
                }
                return Ok(i + 2);
            }
            _ => i += 1,
        }
    }
    Ok(b.len())
}

/// Refuses script text that could make the parser or compiler recurse too deeply. This is the first line only: it gives a
/// friendly error for the common cases, and nothing depends on it for safety, because the script runs in a worker process
/// that can overflow its stack without harming the emulator. Brackets in strings and comments are skipped, brackets inside
/// the `${...}` of a template literal are counted; a regular expression literal is not recognised, which only matters for
/// a script that hides hundreds of brackets after a quote inside one (the worker then ends and the request gets an error).
pub fn check_source(src: &str) -> Result<(), String> {
    if src.len() > MAX_SOURCE_BYTES {
        return Err(format!("The script is larger than {} KB, which is more than the emulator runs.", MAX_SOURCE_BYTES / 1024));
    }
    let b = src.as_bytes();
    let mut depth = 0usize;
    let mut run = 0usize;
    let mut i = 0usize;
    // the depth at which each open `${` of a template literal began
    let mut open: Vec<usize> = Vec::new();
    while i < b.len() {
        match b[i] {
            b'+' | b'-' | b'!' | b'~' => {
                run += 1;
                if run > MAX_PREFIX_RUN {
                    return Err(format!("The script has more than {MAX_PREFIX_RUN} prefix operators in a row."));
                }
                i += 1;
                continue;
            }
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
                continue;
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                    i += 1;
                }
                if matches!(&src[start..i], "typeof" | "void" | "delete" | "new" | "await" | "yield") {
                    run += 1;
                    if run > MAX_PREFIX_RUN {
                        return Err(format!("The script has more than {MAX_PREFIX_RUN} prefix operators in a row."));
                    }
                } else {
                    run = 0;
                }
                continue;
            }
            _ => run = 0,
        }
        match b[i] {
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > MAX_SOURCE_DEPTH {
                    return Err(format!("The script nests brackets more than {MAX_SOURCE_DEPTH} levels deep."));
                }
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if open.last() == Some(&depth) {
                    open.pop();
                    i = scan_template(b, i + 1, &mut depth, &mut open)?;
                    continue;
                }
            }
            b')' | b']' => depth = depth.saturating_sub(1),
            b'`' => {
                i = scan_template(b, i + 1, &mut depth, &mut open)?;
                continue;
            }
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < b.len() && b[i] != q && b[i] != b'\n' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

#[cfg(feature = "scripts")]
thread_local! {
    static HOST: RefCell<Option<Box<dyn Host>>> = const { RefCell::new(None) };
}

#[cfg(feature = "scripts")]
fn host_fn(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = |i: usize, ctx: &mut Context| -> JsResult<String> {
        match args.get(i) {
            Some(v) => Ok(v.to_string(ctx)?.to_std_string_escaped()),
            None => Ok(String::new()),
        }
    };
    let op = text(0, ctx)?;
    let payload = text(1, ctx)?;
    let answer = HOST.with(|h| match h.borrow_mut().as_mut() {
        Some(host) => host.call(&op, &payload),
        None => "{\"accepted\":false}".to_string(),
    });
    Ok(JsValue::from(js_string!(answer.as_str())))
}

/// One engine context with the server-side API loaded.
#[cfg(feature = "scripts")]
pub struct Session {
    ctx: Context,
    deadline: Instant,
    slice: u32,
    pub polls: u64,
}

/// The engine settings a session needs (a part of [`ScriptSettings`] that travels to the worker).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineLimits {
    pub loop_iterations: u64,
    pub recursion_depth: usize,
    pub slice: u32,
}

impl ScriptSettings {
    pub fn engine_limits(&self) -> EngineLimits {
        EngineLimits { loop_iterations: self.loop_iterations, recursion_depth: self.recursion_depth, slice: self.slice }
    }
}

#[cfg(feature = "scripts")]
impl Session {
    /// Makes a context on this thread, with the server-side API loaded. The deadline is set again for every run
    /// ([`Session::set_deadline`]) and a host is given with [`set_host`]; without one, collection calls answer "not accepted".
    pub fn new(limits: &EngineLimits, deadline: Instant) -> Result<Session, Failure> {
        let mut ctx = Context::builder()
            .host_hooks(std::rc::Rc::new(NoCodeFromStrings))
            .build()
            .map_err(|e| Failure::Internal(format!("could not make the engine: {e}")))?;
        {
            let l = ctx.runtime_limits_mut();
            l.set_loop_iteration_limit(limits.loop_iterations);
            l.set_recursion_limit(limits.recursion_depth);
            l.set_stack_size_limit(limits.recursion_depth.saturating_mul(512).max(4096));
        }
        ctx.register_global_builtin_callable(js_string!("__host"), 2, NativeFunction::from_fn_ptr(host_fn))
            .map_err(|e| Failure::Internal(format!("could not register the host function: {e}")))?;
        let mut s = Session { ctx, deadline, slice: limits.slice.max(8), polls: 0 };
        s.run_source(PRELUDE).map_err(|f| Failure::Internal(format!("the script API did not load: {f:?}")))?;
        Ok(s)
    }

    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }

    /// Who answers the collection calls of the script (the host of this thread). `None`: every call answers "not accepted".
    pub fn set_host(&mut self, host: Option<Box<dyn Host>>) {
        HOST.with(|h| *h.borrow_mut() = host);
    }

    fn check(&self) -> Result<(), Failure> {
        if Instant::now() >= self.deadline {
            return Err(Failure::Timeout);
        }
        Ok(())
    }

    /// Runs a parsed script in time slices. The `Ok` value is what the script evaluated to.
    fn run_script(&mut self, script: &Script) -> Result<JsValue, Failure> {
        let slice = self.slice;
        let deadline = self.deadline;
        let result = {
            let fut = script.evaluate_async_with_budget(&mut self.ctx, slice);
            let mut fut = pin!(fut);
            let mut cx = TaskCx::from_waker(Waker::noop());
            loop {
                match fut.as_mut().poll(&mut cx) {
                    Poll::Ready(r) => break r,
                    Poll::Pending => {
                        self.polls += 1;
                        if Instant::now() >= deadline {
                            return Err(Failure::Timeout);
                        }
                    }
                }
            }
        };
        result.map_err(|e| self.failure_of(e))
    }

    /// Compiles and runs source text (budgeted).
    pub fn run_source(&mut self, src: &str) -> Result<JsValue, Failure> {
        let script = Script::parse(Source::from_bytes(src), None, &mut self.ctx).map_err(|e| self.failure_of(e))?;
        self.run_script(&script)
    }

    /// Compiles source text once, to run it often with [`Session::run_prepared_text`].
    pub fn prepare(&mut self, src: &str) -> Result<Script, Failure> {
        Script::parse(Source::from_bytes(src), None, &mut self.ctx).map_err(|e| self.failure_of(e))
    }

    pub fn run_prepared_text(&mut self, script: &Script) -> Result<String, Failure> {
        let v = self.run_script(script)?;
        Ok(v.as_string().map(|s| s.to_std_string_escaped()).unwrap_or_default())
    }

    /// Like [`Session::run_source`], for the answer as text.
    pub fn run_text(&mut self, src: &str) -> Result<String, Failure> {
        let v = self.run_source(src)?;
        match v.as_string() {
            Some(s) => Ok(s.to_std_string_escaped()),
            None => Ok(String::new()),
        }
    }

    pub fn set_global_json(&mut self, name: &str, v: &Value) -> Result<(), Failure> {
        let js = JsValue::from_json(v, &mut self.ctx).map_err(|e| Failure::Internal(format!("bad value: {e}")))?;
        self.ctx
            .register_global_property(js_string!(name), js, boa_engine::property::Attribute::all())
            .map_err(|e| Failure::Internal(format!("could not set {name}: {e}")))
    }

    pub fn set_global_text(&mut self, name: &str, text: &str) -> Result<(), Failure> {
        self.ctx
            .register_global_property(js_string!(name), JsValue::from(js_string!(text)), boa_engine::property::Attribute::all())
            .map_err(|e| Failure::Internal(format!("could not set {name}: {e}")))
    }

    /// The text Azure shows for something a script threw.
    fn failure_of(&mut self, err: boa_engine::JsError) -> Failure {
        if let Some(engine) = err.as_engine() {
            if let boa_engine::error::EngineError::RuntimeLimit(l) = engine {
                return Failure::Limit(match l {
                    RuntimeLimitError::LoopIteration => "The script ran too many loop iterations.".to_string(),
                    RuntimeLimitError::Recursion => "The script nested its calls too deeply (stack overflow).".to_string(),
                    RuntimeLimitError::StackSize => "The script used too much stack.".to_string(),
                });
            }
            return Failure::Internal(engine.to_string());
        }
        match err.into_opaque(&mut self.ctx) {
            Ok(v) => {
                let _ = self.ctx.register_global_property(js_string!("__err"), v, boa_engine::property::Attribute::all());
                let text = self.run_text("__azemu.describe(__err)");
                Failure::Thrown(text.unwrap_or_else(|_| "an exception that cannot be shown".to_string()))
            }
            Err(e) => Failure::Internal(e.to_string()),
        }
    }

    /// Runs the queued callbacks (and the promise jobs they cause) until none is left.
    pub fn drain(&mut self) -> Result<(), Failure> {
        let step = Script::parse(Source::from_bytes("__azemu.step()"), None, &mut self.ctx).map_err(|e| self.failure_of(e))?;
        loop {
            self.check()?;
            self.ctx.run_jobs().map_err(|e| self.failure_of(e))?;
            let more = self.run_script(&step)?;
            if more.as_boolean() != Some(true) {
                break;
            }
        }
        Ok(())
    }

    /// Runs `body` (the text of a function) with `args`, then every callback it queued, and collects what it left.
    pub fn run_function(&mut self, body: &str, args: &Value) -> Result<Outcome, Failure> {
        let run = |s: &mut Session| -> Result<(), Failure> {
            s.run_source(&format!("globalThis.__fn = ({body}\n);"))?;
            s.set_global_json("__args", args)?;
            s.run_source("__azemu.start(__fn, __args)")?;
            s.drain()
        };
        if let Err(f) = run(self) {
            // an abort (`getContext().abort(err)`) throws; the reason is what counts
            if !matches!(f, Failure::Timeout | Failure::Memory | Failure::Internal(_) | Failure::Worker(_) | Failure::Cancelled) {
                if let Some(a) = self.aborted() {
                    return Err(Failure::Aborted(a));
                }
            }
            return Err(f);
        }
        if let Some(a) = self.aborted() {
            return Err(Failure::Aborted(a));
        }
        let text = self.run_text("__azemu.result()")?;
        let v: Value = serde_json::from_str(&text).map_err(|e| Failure::Internal(format!("bad result: {e}")))?;
        Ok(Outcome {
            body: v.get("body").and_then(Value::as_str).map(str::to_string),
            body_set: v.get("bodySet").and_then(Value::as_bool).unwrap_or(false),
            request_body: v.get("requestBody").and_then(Value::as_str).map(str::to_string),
            request_body_set: v.get("requestBodySet").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// `getContext().abort(err)` was called: what it was called with.
    pub fn aborted(&mut self) -> Option<String> {
        self.run_text("__azemu.aborted()").ok().filter(|s| !s.is_empty())
    }
}

#[cfg(all(test, feature = "scripts"))]
mod tests {
    use super::*;
    use serde_json::json;

    struct Null;
    impl Host for Null {
        fn call(&mut self, _op: &str, _payload: &str) -> String {
            String::new()
        }
    }

    /// Runs a script in this process, on a thread with a large stack. This is only for the tests of the engine's own limits:
    /// the emulator never does it (see `proc.rs`; the tests that go through a worker process are in `tests/script_workers.rs`).
    fn run(settings: &ScriptSettings, body: &str, args: Value) -> Result<Outcome, Failure> {
        if let Err(m) = check_source(body) {
            return Err(Failure::Thrown(m));
        }
        let limits = settings.engine_limits();
        let timeout = settings.timeout;
        let body = body.to_string();
        std::thread::Builder::new()
            .stack_size(super::worker::ENGINE_STACK)
            .spawn(move || {
                let mut s = Session::new(&limits, Instant::now() + timeout)?;
                s.set_deadline(Instant::now() + timeout);
                s.set_host(Some(Box::new(Null)));
                s.run_function(&body, &args)
            })
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn hello_world() {
        let o = run(&ScriptSettings::default(), "function () { getContext().getResponse().setBody('Hello, World'); }", json!([])).unwrap();
        assert_eq!(o.body.as_deref(), Some("\"Hello, World\""));
    }

    #[test]
    fn engine_errors_have_text() {
        let f = run(&ScriptSettings::default(), "function () { throw new Error('boom'); }", json!([])).unwrap_err();
        assert_eq!(f, Failure::Thrown("Error: boom".into()));
        let f = run(&ScriptSettings::default(), "function () { throw 'text'; }", json!([])).unwrap_err();
        assert_eq!(f, Failure::Thrown("text".into()));
    }

    fn quick() -> ScriptSettings {
        ScriptSettings { timeout: Duration::from_millis(1500), ..ScriptSettings::default() }
    }

    /// For a test that expects a limit other than the clock: on a slow or busy machine an unoptimised build can take
    /// longer than `quick()` allows to reach that limit, and the clock would answer first.
    fn patient() -> ScriptSettings {
        ScriptSettings { timeout: Duration::from_secs(60), ..ScriptSettings::default() }
    }

    fn fails(settings: &ScriptSettings, body: &str) -> Failure {
        match run(settings, body, json!([])) {
            Err(f) => f,
            Ok(o) => panic!("the script was expected to fail, it answered {o:?}: {body}"),
        }
    }

    fn thrown_has(f: Failure, text: &str) {
        match f {
            Failure::Thrown(m) => assert!(m.contains(text), "{m}"),
            other => panic!("expected a thrown error with {text:?}, got {other:?}"),
        }
    }

    #[test]
    fn a_loop_that_never_ends_stops_at_the_loop_limit() {
        let f = fails(&patient(), "function () { while (true) {} }");
        assert!(matches!(f, Failure::Limit(ref m) if m.contains("loop")), "{f:?}");
        // the limit is per function call, so calling a looping function in a loop is caught by the clock instead
        let st = ScriptSettings { loop_iterations: u64::MAX, ..quick() };
        let t = Instant::now();
        assert_eq!(fails(&st, "function () { while (true) {} }"), Failure::Timeout);
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        assert_eq!(
            fails(&quick(), "function () { function f() { for (var i = 0; i < 4000000; i++) {} } while (true) { f(); } }"),
            Failure::Timeout
        );
    }

    #[test]
    fn recursion_is_limited_whatever_the_shape() {
        for body in [
            "function () { function f(n) { return f(n + 1) + 1; } f(0); }",
            "function () { function f() { return [0].map(f); } f(); }",
            "function () { function f() { return f.apply(null, []); } f(); }",
            "function () { var o = {}; Object.defineProperty(o, 'x', { get: function () { return o.x; } }); o.x; }",
            "function () { function f() { return JSON.stringify({ toJSON: f }); } f(); }",
            "function () { function f() { return [2, 1].sort(f); } f(); }",
            "function () { function f() { return 'a'.replace(/a/, f); } f(); }",
            "function () { var a = []; for (var i = 0; i < 200000; i++) { a = [a]; } String(a); }",
        ] {
            let f = fails(&patient(), body);
            assert!(matches!(f, Failure::Limit(ref m) if m.contains("too deeply")), "{f:?}: {body}");
        }
    }

    #[test]
    fn work_without_loops_is_stopped_by_the_clock() {
        let t = Instant::now();
        assert_eq!(fails(&quick(), "function () { function f(n) { return n < 2 ? n : f(n - 1) + f(n - 2); } f(60); }"), Failure::Timeout);
        // the callbacks of array methods are script calls too
        assert_eq!(
            fails(
                &quick(),
                "function () { function f(n) { return n < 2 ? n : f(n - 1) + f(n - 2); } [0].map(function () { return f(60); }); }"
            ),
            Failure::Timeout
        );
        assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());
    }

    #[test]
    fn calls_that_build_a_big_value_in_one_step_are_refused() {
        for body in [
            "function () { 'abc'.repeat(Math.pow(2, 31)); }",
            "function () { 'abc'.padEnd(Math.pow(2, 31)); }",
            "function () { new Array(1e9).fill(0); }",
            "function () { Array.from({length: 1e9}); }",
            "function () { Array.prototype.join.call({length: 1e9}); }",
            "function () { Array(5000000).join('0123456789'); }",
        ] {
            thrown_has(fails(&quick(), body), "RangeError");
        }
        // a script can handle such an error itself
        let o =
            run(&quick(), "function () { try { 'x'.repeat(1e10); } catch (e) { getContext().getResponse().setBody(e.name); } }", json!([]))
                .unwrap();
        assert_eq!(o.body.as_deref(), Some("\"RangeError\""));
    }

    #[test]
    fn deeply_nested_values_do_not_reach_native_recursion() {
        thrown_has(
            fails(&patient(), "function () { var a = {}; for (var i = 0; i < 200000; i++) { a = {x: a}; } JSON.stringify(a); }"),
            "nested too deeply",
        );
        let f = fails(&quick(), "function () { var a = []; for (var i = 0; i < 200000; i++) { a = [a]; } a.flat(Infinity); }");
        assert!(matches!(f, Failure::Timeout | Failure::Thrown(_) | Failure::Limit(_)), "{f:?}");
        let o = run(&quick(), "function () { getContext().getResponse().setBody([[1, [2, [3]]], 4].flat(Infinity)); }", json!([])).unwrap();
        assert_eq!(o.body.as_deref(), Some("[1,2,3,4]"));
        let o = run(&quick(), "function () { getContext().getResponse().setBody([[1, [2, [3]]], 4].flat()); }", json!([])).unwrap();
        assert_eq!(o.body.as_deref(), Some("[1,[2,[3]],4]"));
    }

    #[test]
    fn eval_and_the_function_constructor_are_refused() {
        thrown_has(fails(&quick(), "function () { eval('1 + 1'); }"), "not supported");
        thrown_has(fails(&quick(), "function () { new Function('return 1'); }"), "not supported");
        thrown_has(fails(&quick(), "function () { (function () {}).constructor('return 1')(); }"), "not supported");
    }

    #[test]
    fn script_text_that_could_exhaust_the_parser_stack_is_refused() {
        assert!(check_source(&format!("function () {{ return {}1{}; }}", "(".repeat(500), ")".repeat(500))).is_err());
        assert!(check_source(&format!("function () {{ return {}1; }}", "!".repeat(100))).is_err());
        assert!(check_source(&format!("function () {{ return {}1; }}", "typeof ".repeat(100))).is_err());
        assert!(check_source(&"a".repeat(MAX_SOURCE_BYTES + 1)).is_err());
        // brackets and operators in strings and comments do not count
        assert!(check_source(&format!("function () {{ var s = '{}'; // {}\n return s; }}", "(".repeat(1000), "-".repeat(1000))).is_ok());
        assert!(check_source("function () { return a - -b + !c + (typeof d); }").is_ok());
        // brackets inside the `${...}` of a template literal count (they used to be skipped as text), nested templates too
        assert!(check_source(&format!("function () {{ var x = `${{{}1{}}}`; }}", "(".repeat(2000), ")".repeat(2000))).is_err());
        assert!(check_source(&format!("function () {{ var x = `a${{`b${{{}1{}}}`}}`; }}", "[".repeat(500), "]".repeat(500))).is_err());
        assert!(
            check_source("function () { var x = `a${(1 + (2))}b${[3].map(function (v) { return `c${v}`; })}\\`${{}}`; return x; }").is_ok()
        );
        // text in the template that only looks like brackets does not count, and the scan comes back to code after `}`
        assert!(check_source(&format!("function () {{ var x = `{}`; return (1); }}", "(".repeat(2000))).is_ok());
        assert!(check_source(&format!("function () {{ var x = `${{1}}`; return {}1{}; }}", "(".repeat(500), ")".repeat(500))).is_err());
    }

    /// The deepest text `check_source` lets through must run on the stack the worker has (`worker::ENGINE_STACK`), in a debug build too.
    #[test]
    fn the_deepest_allowed_text_fits_the_workers_stack() {
        let n = MAX_SOURCE_DEPTH / 2 - 2;
        let shapes = [
            format!("function () {{ var x = {}1{}; }}", "(!".repeat(n), ")".repeat(n)),
            format!("function () {{ var x = {}1{}; }}", "[".repeat(MAX_SOURCE_DEPTH - 5), "]".repeat(MAX_SOURCE_DEPTH - 5)),
            format!("function () {{ var x = {}1{}; }}", "{a:".repeat(MAX_SOURCE_DEPTH - 5), "}".repeat(MAX_SOURCE_DEPTH - 5)),
            format!("function () {{ {} {} }}", "function f() {".repeat(MAX_SOURCE_DEPTH - 5), "}".repeat(MAX_SOURCE_DEPTH - 5)),
            format!("function () {{ var x = {}1{}; }}", "(typeof ".repeat(n), ")".repeat(n)),
            format!("function () {{ var x = {}1; }}", "!".repeat(MAX_PREFIX_RUN)),
            format!("function () {{ var x = 1{}; }}", " + (1".repeat(MAX_SOURCE_DEPTH - 6) + &")".repeat(MAX_SOURCE_DEPTH - 6)),
            format!("function () {{ var x = {}1{}; }}", "a ? (".repeat(n), " : 2)".repeat(n)).replace("a ?", "true ?"),
            format!("function () {{ var x = {}1{}; }}", "`${".repeat(MAX_SOURCE_DEPTH - 5), "}`".repeat(MAX_SOURCE_DEPTH - 5)),
        ];
        for body in shapes {
            assert!(check_source(&body).is_ok(), "{}", &body[..60]);
            match run(&quick(), &body, json!([])) {
                Ok(_) => {}
                Err(Failure::Thrown(m)) if m.contains("SyntaxError") => {}
                Err(f) => panic!("{f:?}: {}", &body[..60]),
            }
        }
    }

    #[test]
    fn the_prelude_array_methods_behave_like_the_native_ones() {
        let body = "function () { var r = {}; var a = [3, 1, 2]; r.map = a.map(function (x, i) { return x * 2 + i; }); r.filter = a.filter(function (x) { return x > 1; }); \
            r.reduce = a.reduce(function (s, x) { return s + x; }); r.reduceRight = a.reduceRight(function (s, x) { return s + '' + x; }, ''); r.some = a.some(function (x) { return x == 2; }); \
            r.every = a.every(function (x) { return x > 0; }); r.find = a.find(function (x) { return x < 3; }); r.findIndex = a.findIndex(function (x) { return x == 2; }); var seen = []; \
            [1, , 3].forEach(function (x, i) { seen.push(i); }); r.seen = seen; r.thisArg = [1].map(function () { return this.k; }, {k: 9}); r.arrayLike = Array.prototype.map.call({length: 2, 0: 'a', 1: 'b'}, function (x) { return x + x; }); \
            try { [].reduce(function () {}); } catch (e) { r.empty = e.name; } try { [1].map(5); } catch (e) { r.notFn = e.name; } getContext().getResponse().setBody(r); }";
        let o = run(&quick(), body, json!([])).unwrap();
        let v: Value = serde_json::from_str(o.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"map": [6, 3, 6], "filter": [3, 2], "reduce": 6, "reduceRight": "213", "some": true, "every": true, "find": 1, "findIndex": 2,
                   "seen": [0, 2], "thisArg": [9], "arrayLike": ["aa", "bb"], "empty": "TypeError", "notFn": "TypeError"})
        );
    }

    #[test]
    fn current_javascript_features_run() {
        let body = r#"function () {
            class Shape { constructor(n) { this.n = n; } get double() { return this.n * 2; } static make(n) { return new Shape(n); } }
            const { n, ...rest } = { n: 1, a: 2, b: 3 };
            const m = new Map([[1, 'a']]); const st = new Set([1, 1, 2]);
            const gen = function* () { yield 1; yield 2; };
            const re = /(?<y>\d{4})-(?<m>\d{2})/u.exec('on 2026-11-12');
            const d = new Date(Date.UTC(2026, 10, 12, 8, 30, 0));
            const out = {
              cls: Shape.make(4).double, rest: Object.keys(rest), map: m.get(1), set: st.size, gen: [...gen()],
              named: re.groups.y + '/' + re.groups.m, iso: d.toISOString(), tpl: `${n}-${[1, 2].join('+')}`,
              entries: Object.entries({ x: 1 }), includes: 'abc'.includes('b'), starts: 'abc'.startsWith('a'),
              arrow: [1, 2, 3].map(x => x * x), opt: ({ a: null })?.a?.b ?? 'dflt', big: typeof 10n,
              json: JSON.parse(JSON.stringify({ a: [1, { b: 2 }] })).a[1].b, sorted: [3, 1, 2].sort(), math: Math.max(1, 5),
              lc: 'İ'.toLowerCase().length, fixed: (1.005).toFixed(2), num: parseFloat('1e3'), sym: typeof Symbol('x'),
            };
            getContext().getResponse().setBody(out);
        }"#;
        let o = run(&quick(), body, json!([])).unwrap();
        let v: Value = serde_json::from_str(o.body.as_deref().unwrap()).unwrap();
        assert_eq!(v["cls"], 8);
        assert_eq!(v["rest"], json!(["a", "b"]));
        assert_eq!((v["map"].clone(), v["set"].clone(), v["gen"].clone()), (json!("a"), json!(2), json!([1, 2])));
        assert_eq!(v["named"], "2026/11");
        assert_eq!(v["iso"], "2026-11-12T08:30:00.000Z");
        assert_eq!(v["tpl"], "1-1+2");
        assert_eq!(v["arrow"], json!([1, 4, 9]));
        assert_eq!(v["opt"], "dflt");
        assert_eq!(v["big"], "bigint");
        assert_eq!(v["json"], 2);
        assert_eq!(v["sorted"], json!([1, 2, 3]));
    }
}
