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
pub mod mgmt;
pub mod model;
pub mod msgutil;
pub mod persist;
pub mod rules;
pub mod state;
pub mod store;
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
        let factory: azemu_amqp::api::SharedFactory = Arc::new(amqp_conn::SbFactory { shared: self.factory.shared.clone() });
        azemu_amqp::serve_connection(conn.io, info, factory, self.cfg.clone()).await;
    }
}

#[async_trait]
impl Service for ServiceBusService {
    fn name(&self) -> &'static str {
        "servicebus"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("servicebus")]
    }

    /// This service's HTTP API is the management (Atom XML) API: entity descriptions and rules, no documented limit. Messages travel over AMQP (up to 100 MB, not here). 1 MiB is generous.
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(1024 * 1024)
    }

    async fn handle(&self, req: Request) -> Response {
        self.shared.ensure_ticker();
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
