//! Azure Event Hubs: an AMQP 1.0 data plane (`EventHubsAmqp`, a core `RawProtocol`), a small
//! JSON admin API over HTTP (`EventHubsService`, mounted at `{ns}.eventhub.localhost` and
//! `/_azemu/eventhubs`), both on one shared in-memory state. Use [`pair`] to get them wired
//! together, and [`AmqpMux`] to share the AMQP port with Service Bus.

use std::sync::Arc;

use azemu_amqp::api::{ConnectionInfo, HandlerFactory, ServerConfig, SharedFactory};
use azemu_core::{async_trait, Mount, RawConn, RawProtocol, Request, Response, Service};

pub mod address;
pub mod admin;
pub mod amqp_conn;
pub mod config;
pub mod enqueue;
pub mod errors;
pub mod kafka;
mod mgmt;
pub mod model;
pub mod mux;
pub mod persist;
pub mod selector;
pub mod state;

pub use mux::AmqpMux;
pub use state::Shared;

/// The HTTP side: the JSON admin API.
pub struct EventHubsService {
    pub shared: Arc<Shared>,
    /// What the last save left on disk (see `persist.rs`).
    pub(crate) saved: persist::SaveMemory,
}

/// The AMQP side. Standalone, every AMQP connection is an Event Hubs connection.
pub struct EventHubsAmqp {
    shared: Arc<Shared>,
    cfg: Arc<ServerConfig>,
}

/// Build both halves on one shared state, configured from the environment
/// (`AZEMU_EVENTHUBS_AUTOCREATE`, `AZEMU_EVENTHUBS_CONFIG`).
pub fn pair() -> (Arc<EventHubsService>, Arc<EventHubsAmqp>) {
    pair_with(Shared::from_env())
}

pub fn pair_with(shared: Arc<Shared>) -> (Arc<EventHubsService>, Arc<EventHubsAmqp>) {
    let cfg = ServerConfig {
        container_id: format!("azemu-eventhubs-{}", uuid::Uuid::new_v4().simple()),
        max_frame_size: 1_048_576,
        ..Default::default()
    };
    (
        Arc::new(EventHubsService { shared: shared.clone(), saved: Default::default() }),
        Arc::new(EventHubsAmqp { shared, cfg: Arc::new(cfg) }),
    )
}

impl EventHubsAmqp {
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// The handler factory of Event Hubs connections, for a multiplexer.
    pub fn factory(&self) -> Arc<dyn HandlerFactory> {
        Arc::new(amqp_conn::EhFactory { shared: self.shared.clone() })
    }

    /// The Kafka endpoint on the same state: register it as a raw protocol and as a service (named `kafka`, so
    /// that `--pin kafka=9093` finds it). See `kafka/mod.rs`.
    pub fn kafka(&self) -> Arc<kafka::KafkaEndpoint> {
        kafka::endpoint(self.shared.clone())
    }

    /// One raw protocol for both services: `other` is the factory of the other AMQP service
    /// (Service Bus). Register the returned mux instead of both raw protocols.
    pub fn mux(&self, other: Arc<dyn HandlerFactory>) -> Arc<AmqpMux> {
        Arc::new(AmqpMux::new(self.shared.clone(), self.factory(), other, self.cfg.clone()))
    }
}

#[async_trait]
impl RawProtocol for EventHubsAmqp {
    fn sniff(&self, first_bytes: &[u8]) -> bool {
        first_bytes.starts_with(b"AMQP")
    }

    async fn serve(&self, conn: RawConn) {
        let info = ConnectionInfo { peer: conn.peer, local: conn.local, tls: conn.tls, server_name: conn.server_name.clone() };
        let factory: SharedFactory = self.factory();
        azemu_amqp::serve_connection(conn.io, info, factory, self.cfg.clone()).await;
    }
}

#[async_trait]
impl Service for EventHubsService {
    fn name(&self) -> &'static str {
        "eventhubs"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("eventhub"), Mount::AccountHost("eventhubs"), Mount::PathPrefix("/_azemu/eventhubs")]
    }

    /// The HTTP side is the admin API and the management reads; Azure documents 1 MB for an event or batch (over AMQP, not here). 2 MiB leaves room.
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(2 * 1024 * 1024)
    }

    async fn handle(&self, req: Request) -> Response {
        if req.account.is_empty() {
            admin::handle(&self.shared, req)
        } else {
            azemu_core::json_error::response(
                501,
                "NotImplemented",
                &format!(
                    "eventhubs: the Event Hubs HTTP management API ({} {}) is not built; use /_azemu/eventhubs",
                    req.method,
                    req.decoded_path()
                ),
            )
        }
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

    /// The highest revision any partition or definition has taken (`model::next_revision`): it moves with every
    /// change to what is saved, and with nothing else. Event Hubs and the Kafka endpoint are one state, kept in the
    /// files of this service; the `kafka` service object reports no generation of its own.
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
