//! identity: managed identity token endpoints and a Microsoft Entra ID (v1 and v2)
//! token endpoint, so the Azure Identity libraries get RS256-signed JWTs from the
//! emulator instead of from Entra ID. See `docs/services/identity.md`.
//!
//! Mounts: `Host("login")`, `PathPrefix("/_azemu/identity")` and
//! `PathPrefix("/metadata/identity")`. There is no account and nothing to store: the
//! only state is the signing key, kept across `reset()`.

// Handlers return the finished error `Response` through `Result`; it is sent as is.
#![allow(clippy::result_large_err)]

mod entra;
mod jwt;
mod keys;
mod msi;
mod persist;
mod token;
mod util;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use azemu_core::{async_trait, json_error, Mount, Request, Response, Service};

pub use keys::SigningKey;

const PREFIX: &str = "_azemu";

pub struct IdentityService {
    key: Arc<KeySlot>,
}

/// The signing key: made once (or read from the data directory), then never changed.
#[derive(Default)]
pub(crate) struct KeySlot {
    current: parking_lot::Mutex<Option<Arc<SigningKey>>>,
    /// One key generation at a time.
    making: tokio::sync::Mutex<()>,
    /// 1 once there is a key, 0 before (`persist_generation`).
    generation: AtomicU64,
}

impl Default for IdentityService {
    fn default() -> Self {
        Self::new()
    }
}

async fn load_key(slot: &KeySlot) -> Result<Arc<SigningKey>, String> {
    if let Some(k) = slot.current.lock().clone() {
        return Ok(k);
    }
    let _one_at_a_time = slot.making.lock().await;
    if let Some(k) = slot.current.lock().clone() {
        return Ok(k);
    }
    // RSA key generation is slow: keep it off the async executor threads.
    let made = tokio::task::spawn_blocking(SigningKey::generate).await.map_err(|e| format!("key generation task failed: {e}"))??;
    let mut current = slot.current.lock();
    // a key read from the data directory while this one was being made wins
    if let Some(k) = current.clone() {
        return Ok(k);
    }
    let key = Arc::new(made);
    *current = Some(key.clone());
    slot.generation.store(1, Ordering::SeqCst);
    Ok(key)
}

impl IdentityService {
    /// Starts generating the signing key in the background when called inside a
    /// Tokio runtime; otherwise the first request that needs it does.
    pub fn new() -> Self {
        let key = Arc::new(KeySlot::default());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let cell = key.clone();
            handle.spawn(async move {
                let _ = load_key(&cell).await;
            });
        }
        IdentityService { key }
    }

    /// The signing key (its public half is served at the `…/discovery/v2.0/keys` URLs).
    pub async fn signing_key(&self) -> Result<Arc<SigningKey>, String> {
        load_key(&self.key).await
    }
}

fn not_built(req: &Request) -> Response {
    // for `GET /_azemu/unsupported`: the fixed words of the path, not tenants and ids
    const WORDS: &[&str] = &[
        "identity",
        "msi",
        "token",
        "login",
        "oauth2",
        "v2.0",
        "discovery",
        "instance",
        "keys",
        ".well-known",
        "openid-configuration",
        "devicecode",
        "authorize",
        "deviceauth",
        "userrealm",
        "metadata",
    ];
    let shape: Vec<String> = req
        .segments()
        .iter()
        .map(|s| if WORDS.contains(&s.to_ascii_lowercase().as_str()) { s.to_ascii_lowercase() } else { "{}".into() })
        .collect();
    let resp = json_error::response(501, "NotImplemented", &format!("identity: {} {}", req.method, req.decoded_path()));
    azemu_core::unsupported::mark(resp, format!("Identity_Unknown ({} /{})", req.method, shape.join("/")))
}

fn wrong_method(req: &Request, allow: &str) -> Response {
    let mut r = json_error::response(405, "MethodNotAllowed", &format!("identity: {} {} (use {allow})", req.method, req.decoded_path()));
    if let Ok(v) = http_header(allow) {
        r.headers_mut().insert("allow", v);
    }
    r
}

fn http_header(s: &str) -> Result<azemu_core::http::HeaderValue, ()> {
    azemu_core::http::HeaderValue::from_str(s).map_err(|_| ())
}

#[async_trait]
impl Service for IdentityService {
    fn name(&self) -> &'static str {
        "identity"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::Host("login"), Mount::PathPrefix("/_azemu/identity"), Mount::PathPrefix("/metadata/identity")]
    }

    /// Token requests are small forms; Azure documents no body limit. 1 MiB is generous.
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(1024 * 1024)
    }

    async fn handle(&self, req: Request) -> Response {
        let segs = req.segments();
        let s: Vec<&str> = segs.iter().map(String::as_str).collect();
        let base = req.ctx.endpoint.base_url();
        match s.as_slice() {
            [p, "identity", "msi", "token"] if *p == PREFIX => msi::app_service(self, &req).await,
            [p, "identity", "token"] if *p == PREFIX => msi::helper(self, &req).await,
            [p, "identity", "login", rest @ ..] if *p == PREFIX => self.entra(&req, &format!("{base}/{PREFIX}/identity/login"), rest).await,
            ["metadata", "identity", "oauth2", "token"] => msi::imds(self, &req).await,
            [p, "identity", ..] if *p == PREFIX => not_built(&req),
            ["metadata", "identity", ..] => not_built(&req),
            // Host mount `login.*`: Entra paths at the root.
            rest => self.entra(&req, &base, rest).await,
        }
    }

    /// Nothing to drop: tokens and codes are self-contained, and the signing key
    /// stays so tokens issued before the reset still verify.
    fn reset(&self) {}

    /// The only state worth keeping is the signing key, so that tokens issued before a restart still verify.
    fn persist_generation(&self) -> Option<u64> {
        Some(self.key.generation.load(Ordering::SeqCst))
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.save_state(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.load_state(dir)
    }
}

fn tenant_ok(t: &str) -> bool {
    !t.is_empty() && t.len() <= 256 && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

impl IdentityService {
    /// Entra paths below `base` (which is the authority without the tenant).
    async fn entra(&self, req: &Request, base: &str, segs: &[&str]) -> Response {
        let lower: Vec<String> = segs.iter().map(|s| s.to_ascii_lowercase()).collect();
        let l: Vec<&str> = lower.iter().map(String::as_str).collect();
        let m = req.method.as_str();
        let get = m == "GET" || m == "HEAD";
        // (tenant, v1, endpoint name)
        let tenant = *segs.first().unwrap_or(&"");
        let route: Option<(&str, bool)> = match l.as_slice() {
            [_, "discovery", "instance"] => Some(("instance", false)),
            [_, "discovery", "keys"] | [_, "discovery", "v2.0", "keys"] => Some(("keys", false)),
            [_, ".well-known", "openid-configuration"] => Some(("config", true)),
            [_, "v2.0", ".well-known", "openid-configuration"] => Some(("config", false)),
            [_, "oauth2", "token"] => Some(("token", true)),
            [_, "oauth2", "v2.0", "token"] => Some(("token", false)),
            [_, "oauth2", "devicecode"] => Some(("devicecode", true)),
            [_, "oauth2", "v2.0", "devicecode"] => Some(("devicecode", false)),
            [_, "oauth2", "authorize"] => Some(("authorize", true)),
            [_, "oauth2", "v2.0", "authorize"] => Some(("authorize", false)),
            [_, "oauth2", "deviceauth"] => Some(("deviceauth", false)),
            [_, "userrealm", _] => Some(("userrealm", false)),
            _ => None,
        };
        let Some((name, v1)) = route else {
            return not_built(req);
        };
        if !tenant_ok(tenant) {
            return token::entra_error(req, 400, "invalid_request", 90002, &format!("Tenant '{tenant}' not found. Check to make sure you have the correct tenant ID and are signing into the correct cloud."));
        }
        match (name, get, m) {
            ("instance", true, _) => entra::instance_discovery(req, base),
            ("keys", true, _) => entra::keys(self, req).await,
            ("config", true, _) => entra::openid_config(base, tenant, v1),
            ("authorize", true, _) => entra::authorize(req, tenant, v1),
            ("deviceauth", true, _) => entra::device_auth_page(),
            ("userrealm", true, _) => entra::user_realm(segs.get(2).copied().unwrap_or("")),
            ("token", _, "POST") => token::token(self, req, base, tenant, v1).await,
            ("devicecode", _, "POST") => token::devicecode(req, base, tenant, v1),
            ("token" | "devicecode", _, _) => wrong_method(req, "POST"),
            _ => wrong_method(req, "GET"),
        }
    }
}
