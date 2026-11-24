//! Fault injection: rules that make the emulator misbehave on purpose, so that a test can see what its code does when
//! Azure throttles, is slow, or drops a connection. Off by default: with no rule nothing here costs more than one atomic read.
//!
//! The rules live in [`Faults`], one per emulator. Core asks it about every HTTP request after the service is known (see
//! `server.rs`); the AMQP and Kafka code asks it about the operations of a connection ([`Faults::messaging`]). The admin API
//! is in `admin.rs`, the JSON of a rule in `spec.rs`, and the documentation in `docs/faults.md`.

mod admin;
mod spec;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub(crate) use admin::{handle as handle_admin, is_faults_path};
pub(crate) use spec::load_file;
use spec::{glob, path_matches, query_has, MESSAGING_SERVICES};
pub use spec::{FaultSpec, Operation, RuleSpec};

use crate::Response;

/// The header on an answer that a rule made.
pub const FAULT_HEADER: &str = "x-azemu-fault";

/// What a rule asks of a service that has to word an answer: a throttling answer, or the error a rule spells out. `None` fields
/// are left to the service's own choice.
#[derive(Debug, Clone, PartialEq)]
pub struct FaultAnswer {
    /// The rule that made it.
    pub rule: u64,
    /// `true`: the answer the service gives when it throttles (the status, code and message are the service's own unless the
    /// rule is an `error`). `false`: an `error` rule.
    pub throttle: bool,
    pub status: Option<u16>,
    pub code: Option<String>,
    pub message: Option<String>,
    pub retry_after: Option<Duration>,
    /// Headers the rule sets (core adds them to the service's answer).
    pub headers: Vec<(String, String)>,
}

impl FaultAnswer {
    pub fn status_or(&self, default: u16) -> u16 {
        self.status.unwrap_or(default)
    }
    pub fn code_or(&self, default: &str) -> String {
        self.code.clone().unwrap_or_else(|| default.to_string())
    }
    pub fn message_or(&self, default: &str) -> String {
        self.message.clone().unwrap_or_else(|| default.to_string())
    }
    /// The wait in whole seconds, rounded up (a header such as `Retry-After` has no fraction).
    pub fn retry_after_secs(&self) -> Option<u64> {
        self.retry_after.map(|d| d.as_millis().div_ceil(1000) as u64)
    }
    pub fn retry_after_ms(&self) -> Option<u64> {
        self.retry_after.map(|d| d.as_millis() as u64)
    }
    /// A code for a status the rule gave without one.
    pub fn default_code(status: u16) -> &'static str {
        match status {
            400 => "BadRequest",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "NotFound",
            408 => "RequestTimeout",
            409 => "Conflict",
            412 => "PreconditionFailed",
            429 => "TooManyRequests",
            500 => "InternalServerError",
            502 => "BadGateway",
            503 => "ServerBusy",
            504 => "GatewayTimeout",
            s if s < 500 => "ClientError",
            _ => "ServerError",
        }
    }
    pub const DEFAULT_MESSAGE: &'static str = "The request was failed on purpose by an azemu fault rule.";
}

/// The answer for a service that has no shape of its own (the default of `Service::fault_response`): the JSON error body
/// `{"error":{"code":..,"message":..}}`, status 429 for a throttle, `Retry-After` in seconds.
pub fn generic_response(a: &FaultAnswer) -> Response {
    let status = a.status_or(429);
    let code = a.code_or(if a.throttle { "TooManyRequests" } else { FaultAnswer::default_code(status) });
    let message = a.message_or(if a.throttle {
        "Too many requests. Retry after the time in the Retry-After header."
    } else {
        FaultAnswer::DEFAULT_MESSAGE
    });
    let mut r = crate::resp::Resp::new(status);
    if let Some(s) = a.retry_after_secs() {
        r = r.header("Retry-After", s);
    }
    let mut r = r.json(&crate::json_error::body(&code, &message, None)).build();
    // keep the body shape of json_error::response for the status
    *r.status_mut() = http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
    r
}

/// What happens to the request, besides a wait.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Answer in the service's shape.
    Answer(FaultAnswer),
    /// Close the connection without an answer.
    Drop,
    /// Hold the request for this long (`None`: until the client gives up), then close the connection.
    Timeout(Option<Duration>),
}

/// What the rules decided for one request or operation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// Wait this long before going on (the `latency` rules that fired, added up).
    pub delay: Duration,
    /// What replaces the normal handling, from the first rule of a kind other than `latency` that fired.
    pub action: Option<Action>,
    /// The rules that fired, with their kind, in rule order. For the log line.
    pub fired: Vec<(u64, &'static str)>,
    /// `ruBudget` rules that let the request through: the charge it reports is added to them afterwards.
    pub charge: Vec<u64>,
}

impl Outcome {
    pub fn is_none(&self) -> bool {
        self.delay.is_zero() && self.action.is_none() && self.fired.is_empty() && self.charge.is_empty()
    }
    /// `throttle` or `latency,throttle`: for the log line and the `x-azemu-fault` header of an answer.
    pub fn kinds(&self) -> String {
        let mut v: Vec<&str> = Vec::new();
        for (_, k) in &self.fired {
            if !v.contains(k) {
                v.push(k);
            }
        }
        v.join(",")
    }
    /// `3` or `2,3`.
    pub fn rules(&self) -> String {
        self.fired.iter().map(|(id, _)| id.to_string()).collect::<Vec<_>>().join(",")
    }
}

/// What is being asked about.
#[derive(Debug, Clone, Copy)]
pub enum Probe<'a> {
    Http { service: &'a str, account: &'a str, method: &'a str, path: &'a str, query: &'a str },
    Messaging { service: &'a str, account: &'a str, entity: &'a str, operation: Operation },
}

impl Probe<'_> {
    fn service(&self) -> &str {
        match self {
            Probe::Http { service, .. } | Probe::Messaging { service, .. } => service,
        }
    }
    fn account(&self) -> &str {
        match self {
            Probe::Http { account, .. } | Probe::Messaging { account, .. } => account,
        }
    }
}

struct Rule {
    id: u64,
    spec: RuleSpec,
    /// Requests that matched (those let through by `skip` included).
    matched: u64,
    fired: u64,
    /// `ruBudget`: the second (since the emulator started) the count is for, and the request units used in it.
    ru_second: u64,
    ru_used: f64,
}

impl Rule {
    fn new(id: u64, spec: RuleSpec) -> Rule {
        Rule { id, spec, matched: 0, fired: 0, ru_second: 0, ru_used: 0.0 }
    }

    fn matches(&self, p: &Probe<'_>) -> bool {
        let s = &self.spec;
        if s.service.as_deref().is_some_and(|v| v != p.service())
            || s.account.as_deref().is_some_and(|v| !v.eq_ignore_ascii_case(p.account()))
        {
            return false;
        }
        // Kafka has no answer in the service's shape: a rule that would need one is not looked at there.
        if p.service() == "kafka" && !s.fault.needs_no_answer() {
            return false;
        }
        match p {
            Probe::Http { method, path, query, .. } => {
                if s.operation.is_some() {
                    return false;
                }
                if s.method.as_deref().is_some_and(|m| !m.eq_ignore_ascii_case(method)) {
                    return false;
                }
                if let Some(pat) = &s.path {
                    // an entity name (no leading `/`) names a messaging entity, not an HTTP path
                    if !(pat.starts_with('/') || pat.starts_with('*')) || !path_matches(pat, path) {
                        return false;
                    }
                }
                s.query.as_deref().is_none_or(|q| query_has(query, q))
            }
            Probe::Messaging { entity, operation, .. } => {
                if s.method.is_some() || s.query.is_some() || s.operation.is_some_and(|o| o != *operation) {
                    return false;
                }
                match &s.path {
                    None => true,
                    Some(pat) if pat.starts_with('/') => false,
                    // the name the client used (`orders/Subscriptions/billing`, `hub/Partitions/0`), or its first part, the entity
                    Some(pat) => glob(pat, entity) || glob(pat, entity.split('/').next().unwrap_or(entity)),
                }
            }
        }
    }

    fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("id".into(), self.id.into());
        if let Ok(serde_json::Value::Object(spec)) = serde_json::to_value(&self.spec) {
            m.extend(spec);
        }
        m.insert("matched".into(), self.matched.into());
        m.insert("fired".into(), self.fired.into());
        serde_json::Value::Object(m)
    }
}

struct State {
    rules: Vec<Rule>,
    next_id: u64,
    /// Rules from `--faults`: they are put back by a full reset.
    file: Vec<RuleSpec>,
    rng: u64,
}

struct Inner {
    state: Mutex<State>,
    /// Set while there is at least one rule: the check every request makes first.
    active: AtomicBool,
    started: Instant,
}

/// The fault rules of one emulator. Cheap to clone; every clone is the same list.
#[derive(Clone)]
pub struct Faults {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Faults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Faults({} rules)", self.inner.state.lock().rules.len())
    }
}

fn clock_seed() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    t ^ (u64::from(std::process::id()) << 32)
}

impl State {
    /// splitmix64: small, and the same sequence for the same seed.
    fn next_u64(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// 0 up to but not including 1.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn reload(&mut self, specs: Vec<RuleSpec>) {
        self.rules.clear();
        self.next_id = 1;
        for s in specs {
            let id = self.next_id;
            self.next_id += 1;
            self.rules.push(Rule::new(id, s));
        }
    }
}

impl Default for Faults {
    fn default() -> Self {
        Faults::with_seed(clock_seed())
    }
}

impl Faults {
    pub fn new() -> Faults {
        Faults::default()
    }

    /// With a fixed seed for the `probability` and `jitterMs` draws, so that a test sees the same sequence every time.
    pub fn with_seed(seed: u64) -> Faults {
        Faults {
            inner: Arc::new(Inner {
                state: Mutex::new(State { rules: Vec::new(), next_id: 1, file: Vec::new(), rng: seed }),
                active: AtomicBool::new(false),
                started: Instant::now(),
            }),
        }
    }

    fn sync_active(&self, st: &State) {
        self.inner.active.store(!st.rules.is_empty(), Ordering::Release);
    }

    /// Is any rule held? Callers check this before building a [`Probe`].
    pub fn is_active(&self) -> bool {
        self.inner.active.load(Ordering::Acquire)
    }

    pub fn len(&self) -> usize {
        self.inner.state.lock().rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Rules that exist from the start and again after a full reset (`--faults`). Replaces the rules held now too.
    pub(crate) fn set_file_rules(&self, specs: Vec<RuleSpec>) {
        let mut st = self.inner.state.lock();
        st.file = specs.clone();
        st.reload(specs);
        self.sync_active(&st);
    }

    /// A full reset: the rules from the API go, those from the file come back (with the ids they had at the start).
    pub fn reset(&self) {
        let mut st = self.inner.state.lock();
        let file = st.file.clone();
        st.reload(file);
        self.sync_active(&st);
    }

    pub(crate) fn add(&self, spec: RuleSpec) -> Result<serde_json::Value, String> {
        let mut st = self.inner.state.lock();
        if st.rules.len() >= spec::MAX_RULES {
            return Err(format!("at most {} rules can be held; remove some first", spec::MAX_RULES));
        }
        let id = st.next_id;
        st.next_id += 1;
        st.rules.push(Rule::new(id, spec));
        self.sync_active(&st);
        Ok(st.rules.last().map(Rule::to_json).unwrap_or_default())
    }

    /// Replace every rule (those of the file too; a full reset brings the file's back).
    pub(crate) fn replace(&self, specs: Vec<RuleSpec>) {
        let mut st = self.inner.state.lock();
        st.reload(specs);
        self.sync_active(&st);
    }

    pub(crate) fn clear(&self) -> usize {
        let mut st = self.inner.state.lock();
        let n = st.rules.len();
        st.rules.clear();
        self.sync_active(&st);
        n
    }

    pub(crate) fn remove(&self, id: u64) -> bool {
        let mut st = self.inner.state.lock();
        let before = st.rules.len();
        st.rules.retain(|r| r.id != id);
        self.sync_active(&st);
        st.rules.len() != before
    }

    pub(crate) fn list_json(&self) -> serde_json::Value {
        serde_json::json!({"rules": self.inner.state.lock().rules.iter().map(Rule::to_json).collect::<Vec<_>>()})
    }

    /// Decide what happens to one HTTP request or one messaging operation. Counters move: ask once per request.
    pub fn decide(&self, probe: &Probe<'_>) -> Outcome {
        let mut out = Outcome::default();
        if !self.is_active() {
            return out;
        }
        let elapsed = self.inner.started.elapsed();
        let mut guard = self.inner.state.lock();
        let st = &mut *guard;
        for i in 0..st.rules.len() {
            if !st.rules[i].matches(probe) {
                continue;
            }
            let (skip, times, probability) = {
                let s = &st.rules[i].spec;
                (s.skip.unwrap_or(0), s.times, s.probability)
            };
            let rule = &mut st.rules[i];
            rule.matched += 1;
            if let FaultSpec::RuBudget { ru_per_second } = rule.spec.fault {
                let second = elapsed.as_secs();
                if rule.ru_second != second {
                    rule.ru_second = second;
                    rule.ru_used = 0.0;
                }
                if rule.ru_used >= ru_per_second {
                    rule.fired += 1;
                    // to the start of the next second, so that a client that waits that long finds a new budget
                    let wait = Duration::from_millis(1000 - u64::from(elapsed.subsec_millis())).max(Duration::from_millis(1));
                    out.fired.push((rule.id, "ruBudget"));
                    out.action = Some(Action::Answer(FaultAnswer {
                        rule: rule.id,
                        throttle: true,
                        status: None,
                        code: None,
                        message: None,
                        retry_after: Some(wait),
                        headers: Vec::new(),
                    }));
                    break;
                }
                out.charge.push(rule.id);
                continue;
            }
            if rule.matched <= skip || times.is_some_and(|t| rule.fired >= t) {
                continue;
            }
            if let Some(p) = probability {
                if p < 1.0 {
                    let draw = st.next_f64();
                    if draw >= p {
                        continue;
                    }
                }
            }
            let jitter = match &st.rules[i].spec.fault {
                FaultSpec::Latency { jitter_ms: Some(j), .. } if *j > 0 => Some(*j),
                _ => None,
            };
            let jitter_draw = jitter.map(|j| st.next_u64() % (j + 1));
            let rule = &mut st.rules[i];
            rule.fired += 1;
            out.fired.push((rule.id, rule.spec.fault.kind()));
            match &rule.spec.fault {
                FaultSpec::Latency { ms, .. } => {
                    out.delay += Duration::from_millis(*ms + jitter_draw.unwrap_or(0));
                    continue;
                }
                FaultSpec::Throttle { retry_after_seconds, retry_after_ms } => {
                    out.action = Some(Action::Answer(FaultAnswer {
                        rule: rule.id,
                        throttle: true,
                        status: None,
                        code: None,
                        message: None,
                        retry_after: spec::retry_after(*retry_after_seconds, *retry_after_ms),
                        headers: Vec::new(),
                    }));
                }
                FaultSpec::Error { status, code, message, retry_after_seconds, retry_after_ms, headers } => {
                    out.action = Some(Action::Answer(FaultAnswer {
                        rule: rule.id,
                        throttle: false,
                        status: Some(*status),
                        code: code.clone(),
                        message: message.clone(),
                        retry_after: spec::retry_after(*retry_after_seconds, *retry_after_ms),
                        headers: headers.iter().flatten().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    }));
                }
                FaultSpec::Drop {} => out.action = Some(Action::Drop),
                FaultSpec::Timeout { ms } => out.action = Some(Action::Timeout(ms.map(Duration::from_millis))),
                FaultSpec::RuBudget { .. } => {}
            }
            break;
        }
        out
    }

    /// Ask about one operation of a messaging connection (AMQP, Kafka): `account` is the namespace, `entity` the address the
    /// client named (`orders`, `hub/Partitions/0`; empty for a connect).
    pub fn messaging(&self, service: &str, account: &str, entity: &str, operation: Operation) -> Outcome {
        if !self.is_active() {
            return Outcome::default();
        }
        debug_assert!(MESSAGING_SERVICES.contains(&service));
        self.decide(&Probe::Messaging { service, account, entity, operation })
    }

    /// Add the request units a request used to the `ruBudget` rules that let it through.
    pub fn add_charge(&self, rules: &[u64], charge: f64) {
        if rules.is_empty() || !charge.is_finite() || charge <= 0.0 {
            return;
        }
        let second = self.inner.started.elapsed().as_secs();
        let mut st = self.inner.state.lock();
        for r in st.rules.iter_mut().filter(|r| rules.contains(&r.id)) {
            if r.ru_second != second {
                r.ru_second = second;
                r.ru_used = 0.0;
            }
            r.ru_used += charge;
        }
    }
}

#[cfg(test)]
mod tests;
