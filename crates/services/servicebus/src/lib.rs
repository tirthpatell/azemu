//! Azure Service Bus: an AMQP 1.0 data plane (`ServiceBusAmqp`, a core `RawProtocol`) and the
//! ATOM management API over HTTP (`ServiceBusService`, mounted at `{ns}.servicebus.localhost`),
//! both on one shared in-memory state. Use [`pair`] to get them wired together.

use std::sync::Arc;

use azemu_amqp::api::{ConnectionInfo, ServerConfig};
use azemu_core::{async_trait, Mount, RawConn, RawProtocol, Request, Response, Service};

pub mod admin;
pub mod amqp_conn;
pub mod config;
pub mod core;
pub mod duration;
pub mod enqueue;
pub mod errors;
mod inspect;
pub mod mgmt;
pub mod model;
pub mod msgutil;
pub mod persist;
pub mod rules;
pub mod state;
pub mod store;
pub mod strict;
pub mod txn;

pub use state::Shared;

/// The HTTP management side.
pub struct ServiceBusService {
    pub shared: Arc<Shared>,
    /// What the last save left on disk (see `persist.rs`).
    pub(crate) saved: persist::SaveMemory,
}

/// The AMQP side.
pub struct ServiceBusAmqp {
    factory: amqp_conn::SbFactory,
    cfg: Arc<ServerConfig>,
}

/// Build both halves on one shared state, configured from the environment
/// (`AZEMU_SERVICEBUS_AUTOCREATE`, `AZEMU_SERVICEBUS_CONFIG`).
pub fn pair() -> (Arc<ServiceBusService>, Arc<ServiceBusAmqp>) {
    pair_with(Shared::from_env())
}

pub fn pair_with(shared: Arc<Shared>) -> (Arc<ServiceBusService>, Arc<ServiceBusAmqp>) {
    shared.ensure_ticker();
    let cfg = ServerConfig {
        container_id: format!("azemu-servicebus-{}", uuid::Uuid::new_v4().simple()),
        max_frame_size: 1_048_576,
        ..Default::default()
    };
    (
        Arc::new(ServiceBusService { shared: shared.clone(), saved: Default::default() }),
        Arc::new(ServiceBusAmqp { factory: amqp_conn::SbFactory { shared }, cfg: Arc::new(cfg) }),
    )
}

#[async_trait]
impl RawProtocol for ServiceBusAmqp {
    fn sniff(&self, first_bytes: &[u8]) -> bool {
        first_bytes.starts_with(b"AMQP")
    }

    async fn serve(&self, conn: RawConn) {
        let info = ConnectionInfo { peer: conn.peer, local: conn.local, tls: conn.tls, server_name: conn.server_name.clone() };
        // wrapped for fault rules (`docs/faults.md`); with no rule a call costs one atomic read
        let factory: azemu_amqp::api::SharedFactory = Arc::new(azemu_amqp::faults::FaultFactory {
            inner: Arc::new(amqp_conn::SbFactory { shared: self.factory.shared.clone() }),
            faults: conn.faults.clone(),
            service: "servicebus",
            namespace_of: amqp_conn::namespace_from_hostname,
        });
        azemu_amqp::serve_connection(conn.io, info, factory, self.cfg.clone()).await;
    }
}

impl ServiceBusService {
    /// Strict authorisation (`docs/strict-auth.md`): hand over the handle. With the mode off nothing changes. The AMQP side and the
    /// administration API both check credentials and rights; the authorization rules are `shared.sas`.
    pub fn set_auth(&self, auth: azemu_core::auth::Auth) {
        self.shared.set_auth(auth);
    }

    /// Where Resource Manager's authorization rules are read from (`azemu_core::auth::sas::SasSource`).
    pub fn set_sas_source(&self, source: Arc<dyn azemu_core::auth::sas::SasSource>) {
        self.shared.sas.set_source(source);
    }
}

#[async_trait]
impl Service for ServiceBusService {
    fn name(&self) -> &'static str {
        "servicebus"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("servicebus"), Mount::PathPrefix("/_azemu/servicebus")]
    }

    /// Credentials, rights and the administration API in strict mode (the AMQP side checks with the same handle).
    fn enforces_strict_auth(&self) -> bool {
        true
    }

    /// This service's HTTP API is the management (Atom XML) API: entity descriptions and rules, no documented limit. Messages travel over AMQP (up to 100 MB, not here). 1 MiB is generous.
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(1024 * 1024)
    }

    fn fault_response(&self, req: &Request, answer: &azemu_core::FaultAnswer) -> Response {
        admin::fault_response(req, answer)
    }

    fn inspect(&self, req: &azemu_core::inspect::InspectRequest) -> azemu_core::inspect::InspectResult {
        inspect::node(&self.shared, req)
    }

    async fn handle(&self, req: Request) -> Response {
        self.shared.ensure_ticker();
        if req.account.is_empty() {
            let segs = req.segments();
            let rest: Vec<&str> = segs.iter().skip(2).map(String::as_str).collect();
            return azemu_core::auth::sas_admin::handle(&self.shared.sas, &req, &rest).unwrap_or_else(|| {
                azemu_core::json_error::response(
                    404,
                    "NotFound",
                    "servicebus: the only admin path is /_azemu/servicebus/{namespace}/authorization-rules[/{name}]",
                )
            });
        }
        admin::handle(&self.shared, req)
    }

    fn reset(&self) {
        self.shared.reset();
        self.saved.0.lock().forget();
    }

    fn reset_account(&self, account: &str) -> bool {
        self.shared.reset_namespace(account);
        self.saved.0.lock().forget(); // as a full reset: the next save compares with nothing and drops the files of what is gone
        true
    }

    /// The highest revision any store, entity or definition has taken (see `store::next_revision`): it moves with
    /// every change to what is saved, and with nothing else. Time passing is not a change until something acts on it
    /// (a message whose time to live ran out is removed when the next tick or receive applies it).
    fn persist_generation(&self) -> Option<u64> {
        Some(self.persist_generation_value())
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.save_dir(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.load_dir(dir)
    }
}
