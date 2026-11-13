//! azemu ARM: the Azure Resource Manager control plane (`management.azure.com`).
//!
//! Mounted at `Host("management")`. Resource groups, resources, tags, providers and
//! deployments live here; resources of the providers listed in `providers/` also reach
//! the emulated data plane through [`Backends`]. See `docs/services/arm.md`.

use azemu_core::resp::Resp;
use azemu_core::{async_trait, Mount, Request, Response, Service};
use parking_lot::Mutex;

pub mod backends;
mod basics;
mod catalog;
mod cx;
mod deployments;
mod error;
mod eventsubs;
mod groups;
mod history;
mod ids;
mod limits;
mod metadata;
mod persist;
mod provider;
mod providers;
mod resources;
mod router;
mod state;
mod tags;
mod template;
mod template_parse;
mod util;
mod writers;

pub use backends::Backends;
pub use error::ArmError;

use provider::Provider;
use state::State;

/// Shared by the service and the per-request context.
pub struct Inner {
    pub(crate) state: Mutex<State>,
    /// `AZEMU_ARM_STRICT=1`: resource types ARM has no provider for answer 501 on writes.
    pub(crate) strict: bool,
    /// Bytes of deployment records one subscription keeps (`AZEMU_ARM_DEPLOYMENT_HISTORY_MB`).
    pub(crate) history_bytes: usize,
    providers: Vec<Box<dyn Provider>>,
    pub(crate) backends: Backends,
}

impl Inner {
    pub(crate) fn provider(&self, ns: &str) -> Option<&dyn Provider> {
        self.providers.iter().find(|p| p.namespace().eq_ignore_ascii_case(ns)).map(|b| b.as_ref())
    }
}

pub struct ArmService {
    inner: Inner,
    /// What the last save left on disk (see `persist.rs`).
    saved: persist::SaveMemory,
}

impl ArmService {
    /// ARM with no emulated services behind it: resources are stored and nothing else happens.
    pub fn standalone() -> ArmService {
        ArmService::new(Backends::none())
    }

    /// Reads its settings (`AZEMU_ARM_STRICT`, `AZEMU_ARM_DEPLOYMENT_HISTORY_MB`) through `azemu_core::settings`.
    pub fn new(backends: Backends) -> ArmService {
        azemu_core::settings::describe(
            "AZEMU_ARM_STRICT",
            "off",
            "ARM: a write for a resource type that no provider emulates (Microsoft.Web/sites) answers 501 instead of being stored as plain JSON.",
        );
        let strict = azemu_core::settings::flag("AZEMU_ARM_STRICT", false);
        ArmService::build(backends, strict, history::history_bytes_from_settings())
    }

    /// Strict mode set by the caller; the other settings have their defaults (no variable is read).
    pub fn with_strict(backends: Backends, strict: bool) -> ArmService {
        ArmService::build(backends, strict, history::default_history_bytes())
    }

    fn build(backends: Backends, strict: bool, history_bytes: usize) -> ArmService {
        ArmService {
            inner: Inner { state: Mutex::new(State::default()), strict, history_bytes, providers: providers::all(&backends), backends },
            saved: Default::default(),
        }
    }
}

#[async_trait]
impl Service for ArmService {
    fn name(&self) -> &'static str {
        "arm"
    }

    fn mounts(&self) -> Vec<Mount> {
        // https://management.localhost:4577 (and management.azure.com when it is routed here)
        vec![Mount::Host("management"), Mount::PathPrefix("/metadata/endpoints")]
    }

    /// Azure Resource Manager: a template is at most 4 MB and a parameters file 64 KB (Azure subscription limits); 4 MiB plus room for the deployment wrapper.
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(4 * 1024 * 1024 + 256 * 1024)
    }

    fn body_too_large(&self, limit: usize) -> Option<Response> {
        let e =
            error::ArmError::new(413, "RequestEntityTooLarge", format!("The request content is larger than the limit of {limit} bytes."));
        Some(Resp::new(e.status).json(&e.body()).build())
    }

    async fn handle(&self, req: Request) -> Response {
        let stored_only = std::sync::atomic::AtomicBool::new(false);
        let result = router::route(&self.inner, &req, &stored_only).await;
        let mut resp = match result {
            Ok(r) => r,
            Err(e) => Resp::new(e.status).json(&e.body()).build(),
        };
        if resp.status().as_u16() == 501 && azemu_core::unsupported::marked(&resp).is_none() {
            resp = azemu_core::unsupported::mark(resp, operation_name(&req));
        }
        // values stored without the emulator's port get the port of this request
        if resp.body().windows(util::PORT_TOKEN.len()).any(|w| w == util::PORT_TOKEN.as_bytes()) {
            let suffix = util::port_suffix_of(&req.ctx.endpoint.authority);
            if let Some(filled) = util::fill_port(resp.body(), &suffix) {
                *resp.body_mut() = filled.into();
            }
        }
        if stored_only.load(std::sync::atomic::Ordering::Relaxed) {
            resp.headers_mut().insert("x-azemu-stored-only", azemu_core::http::HeaderValue::from_static("true"));
        }
        if !matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS") {
            // after the work, so that a save that began in between still finds the number moved at its next look
            state::next_revision();
        }
        finish(resp, &req)
    }

    fn reset(&self) {
        *self.inner.state.lock() = State::default();
        self.saved.0.lock().forget();
        state::next_revision();
    }

    /// For ARM an account is a subscription: `POST /_azemu/reset?service=arm&account=<subscription id>` forgets that
    /// subscription's resource groups, resources, deployments and history, tags and tag names, and removes it from the list of
    /// subscriptions that were used. Other subscriptions are not touched. It is ARM's own record that goes: the objects ARM made
    /// in the other services (a storage account's blobs, a queue, a vault) stay there; reset those services by account. A write
    /// or a group delete that is running in the subscription is not waited for. Nothing to forget is not an error.
    fn reset_account(&self, account: &str) -> bool {
        let sub = account.trim().to_ascii_lowercase();
        let scope = format!("/subscriptions/{sub}");
        let prefix = format!("{scope}/");
        {
            let mut st = self.inner.state.lock();
            st.groups.retain(|_, g| !g.sub.eq_ignore_ascii_case(&sub));
            let keys: Vec<String> =
                st.resources.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).map(|(k, _)| k.clone()).collect();
            for k in keys {
                st.resources.remove(&k);
            }
            st.scope_tags.retain(|k, _| k != &scope && !k.starts_with(&prefix));
            st.tag_names.remove(&sub);
            st.subs.retain(|s| !s.eq_ignore_ascii_case(&sub));
        }
        state::next_revision();
        true
    }

    /// The highest revision any scope or definition has taken (`state::next_revision`): it moves with every request that
    /// is not a read, and with nothing else.
    fn persist_generation(&self) -> Option<u64> {
        Some(self.generation())
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.save_dir(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.load_dir(dir)
    }
}

/// The name of the operation of a request, for `GET /_azemu/unsupported`: the method and the resource type it is about
/// (`PUT Microsoft.Web/sites`, `POST Microsoft.KeyVault/vaults/purge`, `GET Microsoft.Storage/storageAccounts/blobServices`);
/// names, subscriptions and groups are left out, so one missing feature is one row however often it is used.
fn operation_name(req: &Request) -> String {
    let segs = req.segments();
    let last = segs.iter().rposition(|s| s.eq_ignore_ascii_case("providers"));
    let Some(i) = last.filter(|i| i + 1 < segs.len()) else {
        // no provider in the path: subscriptions, groups, locations
        let shape: Vec<&str> = segs.iter().enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, s)| s.as_str()).collect();
        return format!("{} /{}", req.method, shape.join("/"));
    };
    let ns = &segs[i + 1];
    let rest = &segs[i + 2..];
    // type, name, type, name, ...: the types are the even positions; a last odd segment is a collection or an action
    let mut parts: Vec<&str> = rest.iter().enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, s)| s.as_str()).collect();
    if parts.is_empty() {
        return format!("{} {ns}", req.method);
    }
    parts.insert(0, ns);
    format!("{} {}", req.method, parts.join("/"))
}

/// The headers every ARM answer carries.
fn finish(mut resp: Response, req: &Request) -> Response {
    let rid = req.ctx.request_id.clone();
    let corr = req.header("x-ms-correlation-request-id").map(str::to_string).unwrap_or_else(|| rid.clone());
    let h = resp.headers_mut();
    let mut put = |k: &'static str, v: String| {
        if !h.contains_key(k) {
            if let Ok(v) = azemu_core::http::HeaderValue::from_str(&v) {
                h.insert(k, v);
            }
        }
    };
    put("x-ms-request-id", rid.clone());
    put("x-ms-correlation-request-id", corr.clone());
    put("x-ms-routing-request-id", format!("EMULATOR:{}:{}", req.ctx.time.unix_timestamp(), corr));
    put("Cache-Control", "no-cache".to_string());
    put("Pragma", "no-cache".to_string());
    put("Expires", "-1".to_string());
    put("X-Content-Type-Options", "nosniff".to_string());
    let remaining = if req.method == azemu_core::http::Method::GET || req.method == azemu_core::http::Method::HEAD {
        ("x-ms-ratelimit-remaining-subscription-reads", "11999")
    } else {
        ("x-ms-ratelimit-remaining-subscription-writes", "1199")
    };
    put(remaining.0, remaining.1.to_string());
    if let Some(c) = req.header("x-ms-client-request-id") {
        if req.header("x-ms-return-client-request-id").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false) {
            put("x-ms-client-request-id", c.to_string());
        }
    }
    resp
}
