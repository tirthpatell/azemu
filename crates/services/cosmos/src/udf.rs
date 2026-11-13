//! User-defined functions in queries: `udf.name(args)` runs the stored JavaScript function once per call.
//!
//! One worker process (`js/proc.rs`) serves all calls of one query: each function's text is sent the first time it is called, and
//! every call is one message to the worker and one answer back, so no process starts per row. A function is pure: the worker has
//! no collection API (calls answer "not accepted"). All calls of a query together have the script time limit, and the emulator
//! kills the worker at that limit like any other.
//!
//! The query evaluator asks for one value at a time, so the calls are not sent in batches: each costs one round trip on a pipe
//! (measured in `docs/services/cosmos-scripts.md`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::errors::{CosmosError, R};
use crate::js::proc::UdfWorker;
use crate::js::{check_source, Failure, ScriptSettings};

/// The bodies of the functions a query names; a name that is not stored is a 400.
pub fn definitions(all: &[(String, String)], names: &[String]) -> R<Vec<(String, String)>> {
    let mut out = Vec::new();
    for n in names {
        match all.iter().find(|(id, _)| id == n) {
            Some(d) => out.push(d.clone()),
            None => return Err(CosmosError::bad_request(format!("The user-defined function 'udf.{n}' was not found in the container."))),
        }
    }
    Ok(out)
}

pub struct UdfRuntime {
    defs: HashMap<String, String>,
    settings: Arc<ScriptSettings>,
    deadline: Instant,
    live: RefCell<Option<UdfWorker>>,
    /// Functions whose text passed `check_source`.
    checked: RefCell<HashSet<String>>,
}

/// What a query carries to make its runtime on the thread that runs it.
#[derive(Debug)]
pub struct UdfSource {
    defs: Vec<(String, String)>,
    settings: Arc<ScriptSettings>,
    deadline: Instant,
}

impl UdfSource {
    pub fn new(defs: Vec<(String, String)>, settings: &Arc<ScriptSettings>, deadline: Instant) -> UdfSource {
        UdfSource { defs, settings: settings.clone(), deadline }
    }
}

impl azemu_cosmos_sql::UdfFactory for UdfSource {
    fn make(&self) -> Box<dyn azemu_cosmos_sql::UdfHost> {
        Box::new(UdfRuntime::new(self.defs.clone(), &self.settings, self.deadline))
    }
}

impl UdfRuntime {
    pub fn new(defs: Vec<(String, String)>, settings: &Arc<ScriptSettings>, deadline: Instant) -> UdfRuntime {
        UdfRuntime {
            defs: defs.into_iter().collect(),
            settings: settings.clone(),
            deadline,
            live: RefCell::new(None),
            checked: RefCell::new(HashSet::new()),
        }
    }

    fn message(&self, f: Failure) -> String {
        match f {
            Failure::Thrown(m) | Failure::Aborted(m) => format!("Encountered exception while executing function. Exception = {m}"),
            Failure::Timeout => format!(
                "A user-defined function ran out of time: the calls of one query may take {} seconds together.",
                self.settings.timeout.as_secs_f64()
            ),
            Failure::Memory => "A user-defined function used more memory than the emulator allows.".to_string(),
            Failure::Limit(m) => format!("Encountered exception while executing function. Exception = {m}"),
            Failure::Internal(m) => format!("The user-defined function could not run: {m}"),
            Failure::Worker(m) => m,
            Failure::Cancelled => "The request was cancelled while a user-defined function ran.".to_string(),
            Failure::NotBuilt => crate::script_run::NOT_BUILT.to_string(),
        }
    }
}

impl azemu_cosmos_sql::UdfHost for UdfRuntime {
    fn call(&self, name: &str, args: &[Option<Value>]) -> Result<Option<Value>, String> {
        let body = self.defs.get(name).ok_or_else(|| format!("The user-defined function 'udf.{name}' was not found."))?;
        if !self.checked.borrow().contains(name) {
            check_source(body).map_err(|m| format!("The user-defined function 'udf.{name}' cannot run: {m}"))?;
            self.checked.borrow_mut().insert(name.to_string());
        }
        let mut guard = self.live.borrow_mut();
        if guard.is_none() {
            *guard = Some(UdfWorker::start(&self.settings, self.deadline).map_err(|f| self.message(f))?);
        }
        let worker = guard.as_mut().ok_or_else(|| "the user-defined function runtime is gone".to_string())?;
        let text = worker.call(name, body, args).map_err(|f| self.message(f))?;
        if text.is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&text).map(Some).map_err(|_| "The user-defined function returned a value that is not JSON.".to_string())
    }
}
