//! appconfig: the Azure App Configuration data-plane service. Each
//! `{store}.azconfig.localhost` host is a separate store, created on first use.
//! See `docs/services/appconfig.md` for what is and is not built.

// Errors in this crate are ready-to-send HTTP responses (`Result<_, Response>`); they only
// happen on rejected requests, so their size does not matter.
#![allow(clippy::result_large_err)]

mod auth;
mod errors;
mod events;
mod filters;
mod inspect;
mod keyvalues;
mod lists;
mod locks;
mod paging;
mod persist;
mod provision;
mod render;
mod router;
mod snapshots;
mod state;
mod strict;
mod timefmt;

use std::sync::atomic::{AtomicU64, Ordering};

use azemu_core::{async_trait, Mount, Request, Response, Service};
use parking_lot::Mutex;

pub use events::{StoreEvent, StoreEventSink};

pub use state::Limits;

#[derive(Default)]
pub struct AppConfigService {
    state: Mutex<state::State>,
    /// Source of the increasing number in the `Sync-Token` header.
    sync_number: AtomicU64,
    /// Where App Configuration events go (see `events.rs`). Unset: no events.
    sink: parking_lot::RwLock<Option<std::sync::Arc<dyn StoreEventSink>>>,
    /// Raised on every change to the state that a save would write (see `persist_generation`).
    generation: AtomicU64,
    /// Strict authorisation (`strict.rs`); does nothing until the program hands over a real handle.
    auth: parking_lot::RwLock<azemu_core::auth::Auth>,
}

impl AppConfigService {
    /// Check roles for bearer tokens in strict mode (`docs/strict-auth.md`).
    pub fn set_auth(&self, auth: azemu_core::auth::Auth) {
        auth.declare_enforced("appconfig");
        *self.auth.write() = auth;
    }

    /// Limits from the environment (`AZEMU_APPCONFIG_*`, see `docs/services/appconfig.md`).
    pub fn new() -> Self {
        Self::with_limits(Limits::from_env())
    }

    /// With the given limits on revisions, store size and snapshots.
    pub fn with_limits(limits: Limits) -> Self {
        AppConfigService { state: Mutex::new(state::State::new(limits)), ..Self::default() }
    }
}

#[async_trait]
impl Service for AppConfigService {
    fn name(&self) -> &'static str {
        "appconfig"
    }

    fn mounts(&self) -> Vec<Mount> {
        // https://{store}.azconfig.localhost:4577
        vec![Mount::AccountHost("azconfig")]
    }

    fn enforces_strict_auth(&self) -> bool {
        true
    }

    fn inspect(&self, req: &azemu_core::inspect::InspectRequest) -> azemu_core::inspect::InspectResult {
        inspect::node(&self.state.lock(), req)
    }

    fn default_account(&self) -> &'static str {
        "default"
    }

    /// A key-value is at most about 10 KB (key, label and value) in Azure's docs; snapshot and batch bodies have no documented limit. 1 MiB is generous.
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(1024 * 1024)
    }

    /// `429` with `retry-after-ms` and a `application/problem+json` body of type `too-many-requests`, as the App Configuration
    /// throttling page shows (learn.microsoft.com/azure/azure-app-configuration/rest-api-throttling): "The 429 response will
    /// contain a retry-after-ms header providing the client with a suggested wait time (in milliseconds)". Azure always sends the
    /// header, so a throttle with no wait in the rule gets the page's example, 10 ms.
    fn fault_response(&self, req: &Request, a: &azemu_core::FaultAnswer) -> Response {
        let status = a.status_or(429);
        let mut body = serde_json::Map::new();
        if a.throttle {
            body.insert("type".into(), "https://azconfig.io/errors/too-many-requests".into());
            body.insert("title".into(), a.message_or("Resource utilization has surpassed the assigned quota").into());
            body.insert("policy".into(), "Total Requests".into());
        } else {
            let kind = a.code.clone().unwrap_or_else(|| azemu_core::FaultAnswer::default_code(status).to_string());
            body.insert("type".into(), format!("https://azconfig.io/errors/{kind}").into());
            body.insert("title".into(), a.message_or(azemu_core::FaultAnswer::DEFAULT_MESSAGE).into());
        }
        body.insert("status".into(), status.into());
        let mut r = azemu_core::resp::Resp::new(status)
            .header("Content-Type", errors::PROBLEM_CT)
            .body(serde_json::to_vec(&serde_json::Value::Object(body)).unwrap_or_default());
        if let Some(ms) = a.retry_after_ms().or(if a.throttle { Some(10) } else { None }) {
            r = r.header("retry-after-ms", ms);
        }
        let sn = self.sync_number.fetch_add(1, Ordering::Relaxed) + 1;
        auth::finish(r.build(), req, sn)
    }

    async fn handle(&self, req: Request) -> Response {
        let strict_auth = self.auth.read().clone();
        let resp = match auth::precheck(&req) {
            Some(r) => r,
            // strict mode: the bearer token and the role come before the operation (`strict.rs`)
            None => match strict::gate(&strict_auth, &req) {
                Some(refused) => refused,
                None => {
                    let mut state = self.state.lock();
                    let resp = router::route(state.store(&req.account), &req);
                    // Only a read leaves the state alone (old revisions are dropped lazily, and a restart that still
                    // has them drops them the same way). Inside the lock and after the change.
                    if !matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS") {
                        self.generation.fetch_add(1, Ordering::SeqCst);
                    }
                    resp
                }
            },
        };
        let sn = self.sync_number.fetch_add(1, Ordering::Relaxed) + 1;
        let resp = auth::finish(resp, &req, sn);
        let sink = self.sink.read().clone();
        if let Some(sink) = sink {
            events::after(sink.as_ref(), &req, &resp);
        }
        resp
    }

    fn reset(&self) {
        let mut st = self.state.lock();
        let limits = st.limits;
        *st = state::State::new(limits);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Empty one store: key-values, history, snapshots. Persistence sees a change.
    fn reset_account(&self, account: &str) -> bool {
        self.state.lock().stores.retain(|k, _| !k.eq_ignore_ascii_case(account));
        self.generation.fetch_add(1, Ordering::SeqCst);
        true
    }

    fn persist_generation(&self) -> Option<u64> {
        Some(self.generation.load(Ordering::SeqCst))
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.save_state(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.load_state(dir)
    }
}
