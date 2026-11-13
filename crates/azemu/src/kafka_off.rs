//! What a build made without the `kafka` feature mounts where the Kafka endpoint of Event Hubs would be, so that the
//! absence is a clear answer and not a silent one. The service is named `kafka` (so `--pin kafka=9093` and
//! `--standard-ports` still work) and answers HTTP with 501. The raw protocol takes connections that start like a Kafka
//! request, writes one log line and closes them: the Kafka protocol has no place for a message that a client would show.

use azemu_core::{async_trait, Mount, RawConn, RawProtocol, Request, Response, Service};

const MESSAGE: &str = "This build of azemu does not include the Kafka endpoint (it was built without the `kafka` feature). \
Event Hubs over AMQP works; use a build with the default features for Kafka clients.";

pub struct KafkaNotBuilt {
    warned: std::sync::atomic::AtomicBool,
}

impl KafkaNotBuilt {
    pub fn new() -> Self {
        KafkaNotBuilt { warned: std::sync::atomic::AtomicBool::new(false) }
    }
}

impl Default for KafkaNotBuilt {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Service for KafkaNotBuilt {
    fn name(&self) -> &'static str {
        "kafka"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::PathPrefix("/_azemu/kafka")]
    }

    async fn handle(&self, _req: Request) -> Response {
        azemu_core::json_error::response(501, "NotImplemented", MESSAGE)
    }

    fn reset(&self) {}
}

#[async_trait]
impl RawProtocol for KafkaNotBuilt {
    fn sniff(&self, first_bytes: &[u8]) -> bool {
        azemu_eventhubs::kafka::looks_like_kafka(first_bytes)
    }

    async fn serve(&self, _conn: RawConn) {
        // once per run is enough: a Kafka client retries the connection every few hundred milliseconds
        if !self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!("a Kafka client connected, but {MESSAGE}");
        }
    }
}
