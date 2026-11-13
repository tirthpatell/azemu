//! keyvault: the Azure Key Vault data-plane service (secrets and keys; certificates
//! answer 501). Each `{vault}.vault.localhost` host is a separate vault, created on
//! first use. See `docs/services/keyvault.md` for what is and is not built.

// Errors in this crate are ready-to-send HTTP responses (`Result<_, Response>`); they
// only happen on rejected requests, so their size does not matter.
#![allow(clippy::result_large_err)]

mod auth;
mod backup;
mod cert_build;
mod cert_ops;
mod cert_policy;
mod cert_x509;
mod certificates;
mod common;
pub mod crypto;
mod errors;
mod events;
mod jwk;
mod keys;
mod keys_crypto;
mod paging;
mod persist;
mod provision;
mod rotation;
mod router;
mod secrets;
mod state;

use azemu_core::{async_trait, Mount, Request, Response, Service};
use parking_lot::Mutex;

pub use events::{VaultEvent, VaultEventSink};

#[derive(Default)]
pub struct KeyVaultService {
    pub(crate) state: Mutex<state::State>,
    /// Where Key Vault events go (see `events.rs`). Unset: no events.
    sink: parking_lot::RwLock<Option<std::sync::Arc<dyn VaultEventSink>>>,
    /// Raised on every change to the state that a save would write (see `persist_generation`).
    generation: std::sync::atomic::AtomicU64,
}

impl KeyVaultService {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Service for KeyVaultService {
    fn name(&self) -> &'static str {
        "keyvault"
    }

    fn mounts(&self) -> Vec<Mount> {
        // https://{vault}.vault.localhost:4577
        vec![Mount::AccountHost("vault")]
    }

    fn default_account(&self) -> &'static str {
        "default"
    }

    /// Azure documents 25 KB for a secret value; a key, a certificate import and a backup blob are bigger than that and have no documented request limit. 1 MiB is generous.
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(1024 * 1024)
    }

    async fn handle(&self, req: Request) -> Response {
        let resp = match auth::precheck(&req) {
            Some(r) => r,
            None => {
                let r = router::route(self, &req).await;
                // after the change is made: a save that sees the new number also sees the change
                if changes_state(&req) {
                    self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                r
            }
        };
        let resp = auth::finish(resp, &req);
        let sink = self.sink.read().clone();
        if let Some(sink) = sink {
            events::after(sink.as_ref(), &req, &resp);
        }
        resp
    }

    fn reset(&self) {
        let mut st = self.state.lock();
        *st = state::State::default();
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Empty one vault: secrets, keys, certificates, their deleted copies and the vault's own settings. Persistence sees a change.
    fn reset_account(&self, account: &str) -> bool {
        self.state.lock().vaults.remove(&account.to_ascii_lowercase());
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        true
    }

    fn persist_generation(&self) -> Option<u64> {
        Some(self.generation.load(std::sync::atomic::Ordering::SeqCst))
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.save_state(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.load_state(dir)
    }
}

/// Whether a request can change what a save writes. Reads cannot (expired deleted items are dropped lazily, and a
/// restart that still has them drops them the same way); neither can the POST requests that only compute: key
/// operations (sign, verify, encrypt, ...), random bytes and backups.
fn changes_state(req: &Request) -> bool {
    let method = req.method.as_str();
    if matches!(method, "GET" | "HEAD" | "OPTIONS") {
        return false;
    }
    if method == "POST" {
        let segments = req.segments();
        let s: Vec<&str> = segments.iter().map(|s| s.as_str()).collect();
        let reads = match s.as_slice() {
            ["rng"] => true,
            [_, _, "backup"] => true,
            ["keys", _, op] | ["keys", _, _, op] => keys_crypto::Op::parse(op).is_some(),
            _ => false,
        };
        return !reads;
    }
    true
}
