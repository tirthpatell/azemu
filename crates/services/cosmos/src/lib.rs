//! cosmos: the Azure Cosmos DB (NoSQL API) service, gateway mode over the REST API.
//! Each `{account}.documents.localhost` host is a separate account, created on first use.
//! See `docs/services/cosmos.md` for what is and is not built.

#![allow(clippy::result_large_err)]

mod account;
mod batch;
mod change_feed;
mod containers;
mod databases;
mod doc_query;
mod documents;
mod epk;
mod errors;
mod hybridrow;
mod js;
mod limits;
mod offers;
mod out;
mod paging;
mod patch;
mod persist;
mod pk;
mod pkranges;
mod query;
mod resources;
mod rid;
mod router;
mod script_host;
mod script_run;
mod scripts;
mod select;
mod state;
mod udf;
mod users;

use azemu_core::{async_trait, Mount, Request, Response, Service};
use parking_lot::Mutex;
use std::sync::Arc;

use crate::errors::CosmosError;
use crate::out::{Body, Cx, Out};

pub struct CosmosService {
    state: Mutex<state::State>,
    /// Results of queries with more pages (see `paging.rs`). Has its own small lock.
    pages: Arc<paging::PageCache>,
    /// One permit per query that may run now (`limits::MAX_RUNNING_QUERIES`): the others wait for a permit
    /// before they take a blocking thread.
    query_slots: Arc<tokio::sync::Semaphore>,
    /// Settings for stored procedures, triggers and user-defined functions.
    settings: Arc<js::ScriptSettings>,
    /// One permit per script that may run now (`ScriptSettings::max_running`). The permit is held until the script's worker
    /// process has ended, which the emulator makes happen at the time limit (plus a short grace) at the latest.
    script_slots: Arc<tokio::sync::Semaphore>,
    /// For the tests: the next script or user-defined function call panics.
    panic_next: std::sync::atomic::AtomicBool,
    /// Raised after every request that may have changed the state (see `persist_generation`).
    generation: std::sync::atomic::AtomicU64,
    /// What the last save left on disk (see `persist.rs`).
    saved: Mutex<persist::Saved>,
    /// Scripts (and queries with user-defined functions) that run now, by account, so that a reset can end their workers.
    runs: Arc<Mutex<Vec<(u64, String, js::proc::RunHandle)>>>,
    next_run: std::sync::atomic::AtomicU64,
}

/// Takes a running script out of `CosmosService::runs` when it is over.
struct RunGuard {
    runs: Arc<Mutex<Vec<(u64, String, js::proc::RunHandle)>>>,
    id: u64,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.runs.lock().retain(|(id, _, _)| *id != self.id);
    }
}

pub use js::ScriptSettings;

/// Makes this process a script worker if it was started as one (first argument `__cosmos-script-worker`), and then never
/// returns. Scripts run in a worker process, and the emulator starts the worker by running its own executable again, so
/// the `main` of every program that mounts this service must call this first, before it starts a runtime or reads its
/// command line. In every other case it does nothing.
pub fn script_worker_main_if_requested() {
    js::worker::run_if_requested();
}

/// Runs a script worker with these command line arguments (without the program name) and never returns. For the small
/// `azemu-cosmos-script-worker` program that tests and embedders point `ScriptSettings::worker` or `AZEMU_SCRIPT_WORKER` at.
pub fn script_worker_main(args: Vec<String>) -> ! {
    js::worker::run(args)
}

impl Default for CosmosService {
    fn default() -> Self {
        Self::with_script_settings(js::ScriptSettings::from_env())
    }
}

impl CosmosService {
    pub fn new() -> Self {
        Self::default()
    }

    /// A service with its own script limits (the defaults are `ScriptSettings::default()`; `new` also reads the
    /// `AZEMU_COSMOS_SCRIPT_*` environment variables).
    pub fn with_script_settings(settings: js::ScriptSettings) -> Self {
        CosmosService {
            state: Mutex::default(),
            pages: Arc::default(),
            query_slots: Arc::new(tokio::sync::Semaphore::new(limits::MAX_RUNNING_QUERIES)),
            script_slots: Arc::new(tokio::sync::Semaphore::new(settings.max_running.max(1))),
            settings: Arc::new(settings),
            panic_next: std::sync::atomic::AtomicBool::new(false),
            generation: std::sync::atomic::AtomicU64::new(1),
            saved: Mutex::default(),
            runs: Arc::default(),
            next_run: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Lists a running script under its account until the guard is dropped.
    fn track(&self, account: &str, cancel: &js::proc::CancelOnDrop) -> (RunGuard, js::proc::RunHandle) {
        let id = self.next_run.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let handle = cancel.handle();
        self.runs.lock().push((id, account.to_string(), handle.clone()));
        (RunGuard { runs: self.runs.clone(), id }, handle)
    }

    /// Ends the workers of the scripts that run now: of one account, or of all.
    fn end_runs(&self, account: Option<&str>) {
        let handles: Vec<js::proc::RunHandle> =
            self.runs.lock().iter().filter(|(_, a, _)| account.is_none_or(|x| x == a)).map(|(_, _, h)| h.clone()).collect();
        for h in handles {
            h.end_for_reset();
        }
    }

    /// Makes the next script (or call of a user-defined function) panic inside the engine, to test that a panic is a 500
    /// for that request only.
    #[doc(hidden)]
    pub fn inject_script_panic(&self) {
        self.panic_next.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Runs a script job (step 2 of `script_run.rs`), applies its writes under the lock (step 3), and starts again from
    /// step 1 when another request changed the partition meanwhile. `req` is the request that started it.
    async fn run_script(
        &self,
        req: &Request,
        first: Box<script_run::ScriptJob>,
        alt: Option<String>,
        content: Option<String>,
    ) -> Result<Out, CosmosError> {
        const ATTEMPTS: usize = 5;
        let mut job = first;
        job.panic = self.panic_next.swap(false, std::sync::atomic::Ordering::SeqCst);
        for attempt in 1..=ATTEMPTS {
            let permit = match tokio::time::timeout(self.settings.timeout, self.script_slots.clone().acquire_owned()).await {
                Ok(Ok(p)) => p,
                _ => {
                    let mut e =
                        CosmosError::new(429, "TooManyRequests", "Too many scripts are running at the same time; try again shortly.");
                    e.headers.push(("x-ms-retry-after-ms".into(), "100".into()));
                    return Err(e);
                }
            };
            // the script runs in a worker process; this blocking thread answers its collection calls. When this future is
            // dropped (the client went away) `cancel` goes with it and the worker is killed.
            let (cancel, exec) = js::proc::exec_pair();
            let (_listed, ended) = self.track(&req.account, &cancel);
            let task = tokio::task::spawn_blocking(move || {
                let _entered = js::proc::enter(exec);
                let done = job.run();
                drop(permit);
                done
            });
            // the worker is killed at the time limit plus a short grace, which ends the thread; this is only a net
            let wait = self.settings.timeout + self.settings.kill_grace + std::time::Duration::from_secs(5);
            let done = match tokio::time::timeout(wait, task).await {
                Ok(Ok(done)) => done,
                Ok(Err(_)) => return Err(CosmosError::new(500, "InternalServerError", "The script engine failed unexpectedly.")),
                Err(_) => {
                    drop(cancel);
                    return Err(CosmosError::new(408, "RequestTimeout", "The script did not stop in time. Its changes are not applied."));
                }
            };
            drop(cancel);
            if ended.was_ended_by_reset() {
                return Err(CosmosError::not_found_default());
            }
            let mut out = done.out?;
            if let Some(c) = done.commit {
                let applied = {
                    let mut st = self.state.lock();
                    st.container_by_rid(&c.target.account, &c.target.db, &c.target.coll, &c.target.rid)
                        .map(|cont| cont.apply_snapshot(&c.snap, c.now))
                };
                match applied {
                    Some(Ok(lsn)) => out = out.lsn(lsn),
                    None => return Err(CosmosError::not_found_default()),
                    Some(Err(_)) => {
                        if attempt == ATTEMPTS {
                            return Err(CosmosError::new(
                                449,
                                "RetryWith",
                                "The partition was changed by other requests while the script ran, five times in a row. Nothing was applied; retry the request.",
                            ));
                        }
                        let next = {
                            let mut cx = Cx::new(req);
                            cx.cache = Some(self.pages.clone());
                            cx.settings = Some(self.settings.clone());
                            cx.preparse();
                            router::dispatch(&mut self.state.lock(), &cx)
                        };
                        match next {
                            Ok(Out { body: out::Body::Script(j), .. }) => {
                                job = j;
                                continue;
                            }
                            Ok(other) => return Ok(other),
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
            out.alt_path = alt;
            out.content_path = content;
            return Ok(out);
        }
        Err(CosmosError::new(449, "RetryWith", "The script could not be applied."))
    }

    /// Number of document queries this service ran. A page of a query that is served from a
    /// kept result does not count. For tests.
    #[doc(hidden)]
    pub fn query_executions(&self) -> u64 {
        self.pages.executions()
    }

    /// The most document queries that ran at the same time. For tests.
    #[doc(hidden)]
    pub fn peak_running_queries(&self) -> usize {
        self.pages.peak_running()
    }

    /// Keeps at most `n` changes in the history of the "all versions and deletes" change feed (default 100,000), for
    /// every container. For tests that need a reader to fall behind without 100,000 writes; the limit is global to the process.
    #[doc(hidden)]
    pub fn set_change_log_record_limit(n: usize) {
        state::set_record_limit(n);
    }

    /// Kept query results (continuation tokens) at the moment. For tests.
    #[doc(hidden)]
    pub fn kept_results(&self) -> usize {
        self.pages.len()
    }
}

#[async_trait]
impl Service for CosmosService {
    fn name(&self) -> &'static str {
        "cosmos"
    }

    fn mounts(&self) -> Vec<Mount> {
        // https://{account}.documents.localhost:4577
        vec![Mount::AccountHost("documents")]
    }

    fn default_account(&self) -> &'static str {
        "localhost"
    }

    /// 2 MB per request (Azure: "Service quotas and default limits", per-request limits). Refused before the body is read.
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(limits::MAX_REQUEST_BYTES)
    }

    /// A stored procedure run (`POST /dbs/{db}/colls/{coll}/sprocs/{id}`) is dropped when its client goes away, which ends its
    /// worker process at once instead of at the time limit. That is safe because a script works on a copy and its changes
    /// are applied in one step that has no wait in it, so a dropped run applies nothing (and a finished one is never cut).
    /// Nothing else asks: the other requests change state without a worker, and a query with a user-defined function or
    /// a write with a trigger cannot be told from a plain one by method and path (their worker ends at the time limit).
    fn abort_on_disconnect(&self, method: &azemu_core::http::Method, path: &str) -> bool {
        if method != azemu_core::http::Method::POST {
            return false;
        }
        let segs: Vec<&str> = path.split(['/']).filter(|s| !s.is_empty()).collect();
        matches!(segs.as_slice(), ["dbs", _, "colls", _, "sprocs", _])
    }

    fn body_too_large(&self, _limit: usize) -> Option<Response> {
        let activity = uuid::Uuid::new_v4().to_string();
        Some(
            azemu_core::resp::Resp::new(413u16)
                .header("x-ms-activity-id", &activity)
                .json(&CosmosError::too_large().body(&activity))
                .build(),
        )
    }

    async fn handle(&self, req: Request) -> Response {
        let mut cx = Cx::new(&req);
        cx.cache = Some(self.pages.clone());
        cx.settings = Some(self.settings.clone());
        self.pages.sweep(cx.now);
        // Parse the body before taking the lock; run queries after releasing it. What happens
        // under the lock is only the work on the stored state.
        let result = if req.header("authorization").map(str::is_empty).unwrap_or(true) {
            Err(CosmosError::unauthorized("Required Header authorization is missing. Ensure a valid Authorization token is passed."))
        } else if req.body.len() > limits::MAX_REQUEST_BYTES {
            Err(CosmosError::too_large())
        } else {
            cx.preparse();
            let step = router::dispatch(&mut self.state.lock(), &cx);
            match step {
                Ok(Out { body: Body::Deferred(job), .. }) => {
                    let pages = self.pages.clone();
                    // wait for a free slot here, on the async side: a queue of queries costs no threads
                    match self.query_slots.clone().acquire_owned().await {
                        Ok(slot) if job.uses_udf() => {
                            // user-defined functions run in a worker process; this blocking thread serves it and is cancelled,
                            // with the worker killed, when this future is dropped
                            let boom = self.panic_next.swap(false, std::sync::atomic::Ordering::SeqCst);
                            let (cancel, exec) = js::proc::exec_pair();
                            let (_listed, ended) = self.track(&req.account, &cancel);
                            let task = tokio::task::spawn_blocking(move || {
                                let _entered = js::proc::enter(exec);
                                if boom {
                                    panic!("injected script engine panic");
                                }
                                let r = job.run(Some(&pages));
                                drop(slot);
                                r
                            });
                            let wait = self.settings.timeout + self.settings.kill_grace + std::time::Duration::from_secs(5);
                            let r = match tokio::time::timeout(wait, task).await {
                                Ok(Ok(r)) => r,
                                Ok(Err(_)) => Err(CosmosError::new(500, "InternalServerError", "The query failed unexpectedly.")),
                                Err(_) => Err(CosmosError::new(408, "RequestTimeout", "A user-defined function did not stop in time.")),
                            };
                            drop(cancel);
                            if ended.was_ended_by_reset() {
                                Err(CosmosError::not_found_default())
                            } else {
                                r
                            }
                        }
                        Ok(slot) => match tokio::task::spawn_blocking(move || {
                            let r = job.run(Some(&pages));
                            drop(slot);
                            r
                        })
                        .await
                        {
                            Ok(r) => r,
                            Err(_) => Err(CosmosError::new(500, "InternalServerError", "The query failed unexpectedly.")),
                        },
                        Err(_) => Err(CosmosError::new(500, "InternalServerError", "The query failed unexpectedly.")),
                    }
                }
                Ok(Out { body: Body::Script(job), alt_path, content_path, .. }) => self.run_script(&req, job, alt_path, content_path).await,
                other => other,
            }
        };
        // after the work, so that a save that starts in between and sees the change still finds the number moved
        // at the next look (core reads the number before it saves)
        if !matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS") {
            self.mark_changed();
        }
        out::finish(&cx, result)
    }

    fn reset(&self) {
        self.end_runs(None);
        *self.state.lock() = state::State::default();
        self.pages.clear();
        self.saved.lock().clear();
        self.mark_changed();
    }

    /// Drops one account: its databases, containers, documents, users, scripts and offers, the kept query results (those of
    /// every account: losing one is harmless), the scripts that run for it (their workers end and the requests answer 404, as
    /// when a container is removed), and the account's files at the next save. Other accounts are not touched.
    fn reset_account(&self, account: &str) -> bool {
        // accounts are lower case: routing lower-cases the host label
        let account = account.to_ascii_lowercase();
        let account = account.as_str();
        self.end_runs(Some(account));
        self.state.lock().accounts.remove(account);
        self.pages.clear();
        self.saved.lock().clear();
        self.mark_changed();
        true
    }

    /// Raised after every request that is not a read (a query is a POST and raises it too: the save that follows
    /// then finds nothing to write, see `persist.rs`). Time passing is not a change; a document whose time to live
    /// ran out leaves when something reads it, and is gone again after a restart because the time is absolute.
    fn persist_generation(&self) -> Option<u64> {
        Some(self.generation.load(std::sync::atomic::Ordering::SeqCst))
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.save_to(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.load_from(dir)?;
        self.mark_changed();
        Ok(())
    }
}
