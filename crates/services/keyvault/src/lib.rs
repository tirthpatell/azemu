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
mod inspect;
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
mod strict;

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
    /// Strict authorisation (`strict.rs`); does nothing until the program hands over a real handle.
    auth: parking_lot::RwLock<azemu_core::auth::Auth>,
}

impl KeyVaultService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check roles (or access policies) for bearer tokens in strict mode (`docs/strict-auth.md`).
    pub fn set_auth(&self, auth: azemu_core::auth::Auth) {
        auth.declare_enforced("keyvault");
        *self.auth.write() = auth;
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

    fn enforces_strict_auth(&self) -> bool {
        true
    }

    /// Azure documents 25 KB for a secret value; a key, a certificate import and a backup blob are bigger than that and have no documented request limit. 1 MiB is generous.
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(1024 * 1024)
    }

    /// `429` with `Retry-After` (seconds) and the JSON error body of the service. Key Vault's throttling guidance says a vault
    /// "returns HTTP status code 429 (Too many requests)" and clients must back off
    /// (learn.microsoft.com/azure/key-vault/general/overview-throttling); the Azure SDKs honour `Retry-After` on a 429. The code
    /// `Throttled` and the text are not from a recorded answer (see docs/faults.md).
    fn fault_response(&self, req: &Request, a: &azemu_core::FaultAnswer) -> Response {
        let status = a.status_or(429);
        let code = a.code_or(if a.throttle { "Throttled" } else { azemu_core::FaultAnswer::default_code(status) });
        let message = a.message_or(if a.throttle {
            "Request was throttled. Retry the request after the time in the Retry-After header."
        } else {
            azemu_core::FaultAnswer::DEFAULT_MESSAGE
        });
        let mut resp = azemu_core::json_error::response(status, &code, &message);
        if let Some(s) = a.retry_after_secs() {
            if let Ok(v) = azemu_core::http::HeaderValue::from_str(&s.to_string()) {
                resp.headers_mut().insert(azemu_core::http::header::RETRY_AFTER, v);
            }
        }
        auth::finish(resp, req)
    }

    async fn handle(&self, req: Request) -> Response {
        let strict_auth = self.auth.read().clone();
        let resp = match auth::precheck(&req) {
            Some(r) => r,
            // strict mode: the bearer token and the role (or the access policy) come before the operation (`strict.rs`)
            None => match strict::gate(&strict_auth, &req) {
                Some(refused) => refused,
                None => {
                    let r = router::route(self, &req).await;
                    // after the change is made: a save that sees the new number also sees the change
                    if changes_state(&req) {
                        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    r
                }
            },
        };
        let resp = auth::finish(resp, &req);
        let sink = self.sink.read().clone();
        if let Some(sink) = sink {
            events::after(sink.as_ref(), &req, &resp);
        }
        resp
    }

    fn inspect(&self, req: &azemu_core::inspect::InspectRequest) -> azemu_core::inspect::InspectResult {
        self.inspect_node(req)
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
