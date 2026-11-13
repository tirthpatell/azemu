//! The Kafka endpoint of Event Hubs: `{namespace}.servicebus.windows.net:9093` in Azure, here a raw protocol on the
//! emulator's port (plain and inside TLS) and on a pinned port (`--pin kafka=9093`). A topic is an event hub, a Kafka
//! partition is its partition, a Kafka offset is the event's sequence number, a Kafka consumer group is stored
//! apart from the AMQP consumer groups (`groups.rs`). Documented in `docs/services/kafka.md`.
//!
//! One task per connection, requests answered one at a time in the order they came (as Kafka does: a connection is
//! muted while a request is being answered), so a client can pipeline and the socket gives the back pressure. Nothing
//! waits while holding the Event Hubs state lock: a fetch that waits for data waits on a notification and looks again.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use azemu_core::{async_trait, Mount, RawConn, RawProtocol, Request, Response, Service};
use azemu_kafka::{api, header, RequestHeader};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::state::Shared;

pub mod convert;
mod fetch;
pub mod groups;
mod handlers;
mod limits;
mod produce;
pub mod store;

/// Largest request the server reads (Kafka's default `socket.request.max.bytes` is 100 MiB).
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// Largest answer to a Fetch.
pub const MAX_FETCH_BYTES: usize = 16 * 1024 * 1024;
/// Open connections the endpoint accepts; more are closed at once.
pub const MAX_CONNECTIONS: usize = 2_000;
/// A connection that sends no request for this long is closed (Kafka: `connections.max.idle.ms`, 10 minutes).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Time to receive the rest of a request once its length arrived, and to write an answer.
const FRAME_TIMEOUT: Duration = Duration::from_secs(60);

/// Settings read from the environment.
#[derive(Clone, Debug, Default)]
pub struct KafkaConfig {
    /// `AZEMU_KAFKA_ADVERTISED_HOST`: the broker address put in `Metadata` and `FindCoordinator`, for a plain
    /// connection (nothing in the protocol names the host the client used).
    pub advertised_host: Option<String>,
    /// `AZEMU_KAFKA_ADVERTISED_PORT`: when the port the client sees is not the one the emulator listens on.
    pub advertised_port: Option<u16>,
}

impl KafkaConfig {
    pub fn from_env() -> KafkaConfig {
        use azemu_core::settings;
        settings::describe(
            "AZEMU_KAFKA_ADVERTISED_HOST",
            "none (the host the client connected to)",
            "Broker host name the Kafka endpoint puts in Metadata and FindCoordinator answers, for plain connections that do not name one.",
        );
        settings::describe(
            "AZEMU_KAFKA_ADVERTISED_PORT",
            "none (the port the emulator listens on)",
            "Broker port the Kafka endpoint advertises when clients reach the emulator through another port.",
        );
        let port = settings::number("AZEMU_KAFKA_ADVERTISED_PORT", 0);
        KafkaConfig {
            advertised_host: settings::text("AZEMU_KAFKA_ADVERTISED_HOST"),
            advertised_port: u16::try_from(port).ok().filter(|p| *p != 0),
        }
    }
}

/// Raw protocol and service (named `kafka`, so that `--pin kafka=9093` works) in one.
pub struct KafkaEndpoint {
    pub(crate) broker: Arc<handlers::Broker>,
    connections: Arc<AtomicUsize>,
}

/// Log a request frame as hex at `trace`, except a `SaslAuthenticate` request: it carries the user name and the password (for
/// Event Hubs, the connection string), and a log must not hold a credential.
fn trace_frame(frame: &[u8], what: &str) {
    if !tracing::enabled!(tracing::Level::TRACE) {
        return;
    }
    if header::peek_header(frame).map(|h| h.api_key == api::SASL_AUTHENTICATE).unwrap_or(false) {
        tracing::trace!(bytes = frame.len(), "{what} (SaslAuthenticate: the bytes carry credentials and are not shown)");
        return;
    }
    tracing::trace!(frame = %frame.iter().map(|b| format!("{b:02x}")).collect::<String>(), "{what}");
}

pub fn endpoint(shared: Arc<Shared>) -> Arc<KafkaEndpoint> {
    endpoint_with(shared, KafkaConfig::from_env())
}

pub fn endpoint_with(shared: Arc<Shared>, cfg: KafkaConfig) -> Arc<KafkaEndpoint> {
    Arc::new(KafkaEndpoint { broker: Arc::new(handlers::Broker { shared, cfg }), connections: Arc::new(AtomicUsize::new(0)) })
}

/// Does the start of a connection look like a Kafka request? A 4-byte big-endian length (the first byte is 0: under 16 MiB,
/// and no HTTP method, TLS record, `AMQP` or `PRI ` starts with a zero byte), then a known API key and a plausible version.
pub fn looks_like_kafka(b: &[u8]) -> bool {
    if b.len() < 4 || b[0] != 0 {
        return false;
    }
    let len = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
    // api key + version + correlation id is the least a request holds
    if !(8..=MAX_REQUEST_BYTES).contains(&len) {
        return false;
    }
    if b.len() >= 6 {
        let key = i16::from_be_bytes([b[4], b[5]]);
        if azemu_kafka::schema::request_schema(key).is_none() && !(0..=74).contains(&key) {
            return false;
        }
    }
    if b.len() >= 8 {
        let v = i16::from_be_bytes([b[6], b[7]]);
        if !(0..=20).contains(&v) {
            return false;
        }
    }
    true
}

struct ConnGuard(Arc<AtomicUsize>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[async_trait]
impl RawProtocol for KafkaEndpoint {
    fn sniff(&self, first_bytes: &[u8]) -> bool {
        looks_like_kafka(first_bytes)
    }

    async fn serve(&self, conn: RawConn) {
        if self.connections.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
            self.connections.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        let _guard = ConnGuard(self.connections.clone());
        serve_connection(self.broker.clone(), conn).await;
    }
}

async fn serve_connection(broker: Arc<handlers::Broker>, conn: RawConn) {
    let mut c = handlers::Conn::new(&conn);
    let mut io = conn.io;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // the length prefix, within the idle time
        let mut len_bytes = [0u8; 4];
        match tokio::time::timeout(IDLE_TIMEOUT, io.read_exact(&mut len_bytes)).await {
            Ok(Ok(_)) => {}
            _ => return,
        }
        let len = u32::from_be_bytes(len_bytes) as usize;
        if !(8..=MAX_REQUEST_BYTES).contains(&len) {
            tracing::debug!(len, "kafka: bad request length, closing");
            return;
        }
        // the body grows as the bytes arrive; a declared length never sizes the buffer
        buf.clear();
        let read = tokio::time::timeout(FRAME_TIMEOUT, async {
            let mut chunk = [0u8; 16 * 1024];
            while buf.len() < len {
                let want = (len - buf.len()).min(chunk.len());
                let n = io.read(&mut chunk[..want]).await?;
                if n == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Ok::<(), std::io::Error>(())
        })
        .await;
        if !matches!(read, Ok(Ok(()))) {
            return;
        }
        trace_frame(&buf, "kafka: frame");
        let out = match header::read_request(&buf) {
            Ok(req) => {
                let h = req.header.clone();
                match broker.handle(&mut c, req).await {
                    handlers::Reply::Answer(body) => header::write_response(&h, &body),
                    handlers::Reply::Nothing => continue,
                    handlers::Reply::Close => return,
                }
            }
            Err(azemu_kafka::Error::Unsupported { api_key, version }) if api_key == api::API_VERSIONS => {
                // a client newer than the server asks for ApiVersions v4+: answer in v0 with the error and the list,
                // so it can ask again with a version we serve
                let Ok(h) = header::peek_header(&buf) else { return };
                let h = RequestHeader { version: 0, ..h };
                let _ = version;
                header::write_response(&h, &broker.api_versions(api::err::UNSUPPORTED_VERSION))
            }
            Err(e) => {
                let h = header::peek_header(&buf).ok();
                tracing::debug!(error = %e, api_key = h.as_ref().map(|h| h.api_key), version = h.as_ref().map(|h| h.version), "kafka: request refused, closing");
                trace_frame(&buf, "kafka: refused frame");
                return;
            }
        };
        match tokio::time::timeout(FRAME_TIMEOUT, io.write_all(&out)).await {
            Ok(Ok(())) => {}
            _ => return,
        }
        if buf.capacity() > 1 << 20 {
            buf = Vec::new();
        }
    }
}

#[async_trait]
impl Service for KafkaEndpoint {
    fn name(&self) -> &'static str {
        "kafka"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::PathPrefix("/_azemu/kafka")]
    }

    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(64 * 1024)
    }

    /// `GET /_azemu/kafka/{namespace}/groups`: the consumer groups, for looking at what a test left behind.
    async fn handle(&self, req: Request) -> Response {
        let segs = req.segments();
        let seg: Vec<&str> = segs.iter().map(String::as_str).collect();
        match (req.method.as_str(), seg.as_slice()) {
            ("GET", ["_azemu", "kafka", ns, "groups"]) => {
                let list: Vec<_> = self
                    .broker
                    .shared
                    .kafka
                    .list(ns)
                    .into_iter()
                    .map(|g| {
                        azemu_core::serde_json::json!({
                            "group": g.group, "state": g.state, "protocolType": g.protocol_type, "protocol": g.protocol,
                            "generation": g.generation, "members": g.members.iter().map(|m| m.member_id.clone()).collect::<Vec<_>>(),
                            "offsets": self.broker.shared.kafka.all_offsets(ns, &g.group).into_iter()
                                .map(|(t, p, c)| azemu_core::serde_json::json!({"topic": t, "partition": p, "offset": c.offset}))
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                azemu_core::resp::Resp::new(200).json(&azemu_core::serde_json::json!({ "groups": list })).build()
            }
            _ => azemu_core::json_error::response(
                404,
                "NotFound",
                "kafka: this port speaks the Apache Kafka protocol; the only HTTP path is GET /_azemu/kafka/{namespace}/groups",
            ),
        }
    }

    /// `Shared::reset` clears the Kafka state together with the event hubs.
    fn reset(&self) {}

    /// `Shared::reset_namespace` (called by the Event Hubs service) ends the Kafka groups of the namespace as well.
    fn reset_account(&self, _account: &str) -> bool {
        true
    }
}
