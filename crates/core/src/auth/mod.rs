//! Strict authorisation (`--strict-auth`, `AZEMU_STRICT_AUTH=1`): bearer tokens are checked, and what a caller may do is decided by role
//! assignments, with Azure's own answers. See `docs/strict-auth.md`.
//!
//! The pieces:
//! * [`Auth`], the handle every service holds. With the mode off it does nothing and nothing about a request changes.
//! * [`token`], who issued a token and what it says; the identity service is the authority, handed over where the program is put together.
//! * [`roles::Registry`], the role definitions and assignments, and [`roles::Registry::check`], the question "may principal P do action A to
//!   resource R". A service that is not HTTP (Service Bus, Event Hubs) asks the same way.
//! * [`AuthService`], the admin API (`/_azemu/auth`), the saved state and the `--auth-config` file.

mod builtin;
pub mod cbs;
pub mod claims;
pub mod config;
mod inspect;
pub mod roles;
pub mod sas;
pub mod sas_admin;
pub mod scope;
mod service;
pub mod token;
pub mod wildcard;

use std::path::PathBuf;
use std::sync::Arc;

pub use roles::{
    builtin_roles, AssignError, Assignment, CosmosAssignment, Decision, Looked, Need, Origin, Registry, RoleDef, RoleError, VaultAccess,
    VaultPolicy,
};
pub use scope::Resource;
pub use service::AuthService;
pub use token::{bearer_token, Aud, Claims, Fault, Principal, TokenAuthority, TokenFailure};

#[cfg(feature = "testing")]
pub use token::TestAuthority;

tokio::task_local! {
    static IN_PROCESS: ();
}

/// Run `fut` as a call from one of the emulator's own services to another (Resource Manager creating a container, the seed file): strict
/// mode does not check a bearer token inside it. This is a mark on the task, not on anything the request says, and the HTTP server never
/// sets it, so no client can ask for it. Wrap only the call into the other service, nothing that handles a client's request.
pub async fn in_process<F: std::future::Future>(fut: F) -> F::Output {
    IN_PROCESS.scope((), fut).await
}

fn is_in_process() -> bool {
    IN_PROCESS.try_with(|_| ()).is_ok()
}

struct Inner {
    strict: bool,
    authority: parking_lot::RwLock<Option<Arc<dyn TokenAuthority>>>,
    registry: Registry,
    config_file: Option<PathBuf>,
    enforced: parking_lot::Mutex<std::collections::BTreeSet<&'static str>>,
}

/// The handle. Cheap to clone; every clone is the same registry.
#[derive(Clone)]
pub struct Auth(Arc<Inner>);

impl Default for Auth {
    fn default() -> Self {
        Auth::disabled()
    }
}

impl Auth {
    fn build(strict: bool, config_file: Option<PathBuf>) -> Auth {
        Auth(Arc::new(Inner {
            strict,
            authority: parking_lot::RwLock::new(None),
            registry: Registry::new(),
            config_file,
            enforced: Default::default(),
        }))
    }

    /// The mode is off: nothing is checked. The registry still stores what it is given (Resource Manager answers role assignment requests
    /// whatever the mode).
    pub fn disabled() -> Auth {
        Auth::build(false, None)
    }

    /// The mode is on.
    pub fn strict() -> Auth {
        Auth::build(true, None)
    }

    /// The mode and the config file given directly (what `from_settings` reads from the variables).
    pub fn with(strict: bool, config_file: Option<PathBuf>) -> Auth {
        Auth::build(strict, config_file)
    }

    /// `AZEMU_STRICT_AUTH` and `AZEMU_AUTH_CONFIG`.
    pub fn from_settings() -> Auth {
        crate::settings::describe_core();
        let strict = crate::settings::flag("AZEMU_STRICT_AUTH", false);
        let config = crate::settings::text("AZEMU_AUTH_CONFIG").map(PathBuf::from);
        Auth::build(strict, config)
    }

    /// Is strict mode on?
    pub fn is_strict(&self) -> bool {
        self.0.strict
    }

    pub fn registry(&self) -> &Registry {
        &self.0.registry
    }

    pub fn config_file(&self) -> Option<&std::path::Path> {
        self.0.config_file.as_deref()
    }

    /// Hand over the token authority (the identity service).
    pub fn set_authority(&self, a: Arc<dyn TokenAuthority>) {
        *self.0.authority.write() = Some(a);
    }

    pub fn authority(&self) -> Option<Arc<dyn TokenAuthority>> {
        self.0.authority.read().clone()
    }

    /// A service says that it checks roles in strict mode (the start-up log and `GET /_azemu/auth` list what does).
    pub fn declare_enforced(&self, service: &'static str) {
        self.0.enforced.lock().insert(service);
    }

    pub fn enforced_services(&self) -> Vec<&'static str> {
        self.0.enforced.lock().iter().copied().collect()
    }

    /// Does this request go through the checks of strict mode? Yes when the mode is on and it carries a bearer token (a key or a SAS keeps
    /// the checks it has), unless it is a call between the emulator's own services ([`in_process`]).
    pub fn applies(&self, req: &crate::Request) -> bool {
        self.0.strict && !is_in_process() && bearer_token(req).is_some()
    }

    /// Verify the bearer token of a request for the audiences a service accepts. Call it only when [`applies`](Auth::applies).
    pub fn authenticate(&self, req: &crate::Request, auds: &[Aud]) -> Result<Principal, TokenFailure> {
        let Some(token) = bearer_token(req) else {
            return Err(TokenFailure::Malformed);
        };
        self.authenticate_token(token, req.ctx.time.unix_timestamp(), auds)
    }

    /// The same for a token that did not come in an HTTP header (AMQP `put-token`, Kafka SASL).
    pub fn authenticate_token(&self, token: &str, now_unix: i64, auds: &[Aud]) -> Result<Principal, TokenFailure> {
        match self.authority() {
            Some(a) => token::check(a.as_ref(), token, now_unix, auds),
            // nothing to check with: no token can be trusted
            None => Err(TokenFailure::BadSignature),
        }
    }

    /// May the principal do what `need` says to `res`? Logs a refusal in one line (`what` names the request: `GET /c/b`) and returns the decision.
    pub fn authorize(&self, what: &str, who: &Principal, need: &Need, res: &Resource) -> Result<(), Box<Decision>> {
        let d = self.0.registry.check(&who.oid, need, res);
        if d.allowed {
            return Ok(());
        }
        tracing::info!(
            "refused {what}: principal {} has no role assignment that grants {} on {} ({}; GET /_azemu/auth/check explains)",
            who.oid,
            d.needs,
            d.resource,
            d.reason
        );
        Err(Box::new(d))
    }

    /// Log a refusal that a service decided itself (access policies, Event Grid): one line with the request, the principal and the reason.
    pub fn log_refusal(&self, what: &str, who: &Principal, reason: &str) {
        tracing::info!("refused {what}: principal {} {reason}", who.oid);
    }

    /// The same for Cosmos DB's own roles.
    pub fn authorize_cosmos(&self, what: &str, who: &Principal, account: &str, action: &str, path: &str) -> Result<(), Box<Decision>> {
        let d = self.0.registry.cosmos_check(account, &who.oid, action, path);
        if d.allowed {
            return Ok(());
        }
        tracing::info!(
            "refused {what}: principal {} has no Cosmos DB role assignment that grants {action} on {account}{path} ({})",
            who.oid,
            d.reason
        );
        Err(Box::new(d))
    }

    /// The object id a client id has (what its tokens carry as `oid`).
    pub fn object_id_of_client(&self, client_id: &str) -> Option<String> {
        self.authority().map(|a| a.object_id_of_client(client_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_bearer(token: &str) -> crate::Request {
        use crate::request::{Endpoint, Query, RequestCtx};
        let mut headers = http::HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        crate::Request {
            method: http::Method::GET,
            path: "/".into(),
            query: Query::default(),
            headers,
            body: Default::default(),
            account: String::new(),
            ctx: RequestCtx {
                request_id: "x".into(),
                time: crate::time::OffsetDateTime::UNIX_EPOCH,
                endpoint: Endpoint { scheme: "http", authority: "h".into(), path_style: false, account: String::new() },
            },
        }
    }

    #[tokio::test]
    async fn only_a_call_marked_in_process_skips_the_checks() {
        let a = Auth::strict();
        // the text itself means nothing: it is a bearer token like any other
        for t in ["azemu-arm", "azemu-seed", "anything"] {
            assert!(a.applies(&with_bearer(t)), "{t}");
        }
        let r = with_bearer("azemu-arm");
        assert!(!in_process(async { a.applies(&r) }).await);
        // the mark is on the task: a task started inside it does not carry it
        let a2 = a.clone();
        let r2 = r.clone();
        assert!(in_process(async move { tokio::spawn(async move { a2.applies(&r2) }).await.unwrap() }).await);
        assert!(a.applies(&r));
    }
}
