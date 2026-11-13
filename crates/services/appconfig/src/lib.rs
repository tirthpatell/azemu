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
}

impl AppConfigService {
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

    fn default_account(&self) -> &'static str {
        "default"
    }

    /// A key-value is at most about 10 KB (key, label and value) in Azure's docs; snapshot and batch bodies have no documented limit. 1 MiB is generous.
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(1024 * 1024)
    }

    async fn handle(&self, req: Request) -> Response {
        let resp = match auth::precheck(&req) {
            Some(r) => r,
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
