#![allow(dead_code)]
//! Shared helpers for the Event Hubs integration tests: a server on a free port and an AMQP
//! client built on the test client of `azemu-amqp`.

use std::sync::Arc;
use std::time::Duration;

use azemu_amqp::client::{Received, TestClient};
use azemu_amqp::message::{Message, BATCH_MESSAGE_FORMAT};
use azemu_amqp::performatives::{Attach, Role};
use azemu_amqp::terminus::{Source, Target, TargetKind};
use azemu_amqp::types::Value;
use azemu_amqp::Error;
use azemu_core::testing::TestServer;
use azemu_eventhubs::state::Shared;

pub const SELECTOR: &str = "apache.org:selector-filter:string";

pub struct Env {
    pub srv: TestServer,
    pub shared: Arc<Shared>,
}

pub async fn env() -> Env {
    env_with(Shared::new(true, None)).await
}

pub async fn env_with(shared: Arc<Shared>) -> Env {
    let (svc, amqp) = azemu_eventhubs::pair_with(shared.clone());
    let srv = TestServer::start_with(vec![svc], vec![amqp]).await;
    Env { srv, shared }
}

/// A server on a data directory (no timed saves: `srv.save_now()` / `srv.shutdown()` write, dropping does not). The
/// Kafka endpoint is on it too.
pub async fn env_persistent(dir: &std::path::Path) -> Env {
    let shared = Shared::new(true, None);
    let (svc, amqp) = azemu_eventhubs::pair_with(shared.clone());
    let kafka = azemu_eventhubs::kafka::endpoint_with(shared.clone(), Default::default());
    let data = dir.to_path_buf();
    let srv =
        TestServer::start_configured(vec![svc, kafka.clone()], vec![amqp, kafka], move |b| b.data_dir(data).save_interval(Duration::ZERO))
            .await;
    Env { srv, shared }
}

impl Env {
    pub async fn client(&self) -> TestClient {
        self.client_for("myns.eventhub.localhost").await
    }

    pub async fn client_for(&self, host: &str) -> TestClient {
        connect(self.srv.port(), host).await
    }

    /// Create an event hub straight in the state (namespace `myns`).
    pub fn hub(&self, name: &str, partitions: u32) {
        self.shared.state.lock().create_hub("myns", name, partitions, 86_400_000, azemu_eventhubs::state::now_ms()).unwrap();
    }
}

pub async fn connect(port: u16, host: &str) -> TestClient {
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    let mut c = TestClient::connect_sasl(addr, "ANONYMOUS", None).await;
    c.open_session(host).await;
    c
}

pub fn event(body: &str) -> Message {
    Message::data(body.as_bytes().to_vec())
}

pub fn keyed(body: &str, key: &str) -> Message {
    let mut m = event(body);
    m.set_annotation("x-opt-partition-key", Value::str(key));
    m
}

/// Send one event and wait for the broker's outcome.
pub async fn send_one(c: &mut TestClient, link: u32, m: &Message) {
    let state = c.send_message_wait(link, m).await;
    assert_eq!(state, Some(azemu_amqp::terminus::DeliveryState::Accepted), "send outcome");
}

/// Send a batch (format 0x80013700) and wait for the outcome.
pub async fn send_batch(c: &mut TestClient, link: u32, events: &[Message]) -> Option<azemu_amqp::terminus::DeliveryState> {
    let encoded: Vec<Vec<u8>> = events.iter().map(|m| m.encode()).collect();
    let id = c.send_payload(link, &Message::encode_batch(&encoded), BATCH_MESSAGE_FORMAT, false).await;
    c.wait_disposition(id).await
}

pub fn body_of(r: &Received) -> String {
    match r.message().body {
        azemu_amqp::message::Body::Data(parts) => {
            parts.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect::<Vec<_>>().join("")
        }
        other => panic!("not a data body: {other:?}"),
    }
}

pub fn ann_i64(m: &Message, key: &str) -> i64 {
    m.annotation(key).and_then(|v| v.as_i64()).unwrap_or_else(|| panic!("annotation {key} missing in {:?}", m.message_annotations))
}

pub fn ann_str(m: &Message, key: &str) -> String {
    m.annotation(key).and_then(|v| v.as_str()).map(str::to_string).unwrap_or_else(|| panic!("annotation {key} missing"))
}

pub fn consumer_address(hub: &str, cg: &str, pid: u32) -> String {
    format!("amqps://myns.eventhub.localhost:1/{hub}/ConsumerGroups/{cg}/Partitions/{pid}")
}

#[derive(Default, Clone)]
pub struct Consumer {
    pub filter: Option<String>,
    pub epoch: Option<i64>,
    pub metrics: bool,
    pub credit: u32,
}

pub fn from_start() -> Consumer {
    Consumer { filter: Some("amqp.annotation.x-opt-offset > '-1'".into()), credit: 300, ..Default::default() }
}

pub fn with_filter(f: &str) -> Consumer {
    Consumer { filter: Some(f.into()), credit: 300, ..Default::default() }
}

/// Attach a receiver on a partition of a consumer group the way the SDKs do.
pub async fn consume(c: &mut TestClient, hub: &str, cg: &str, pid: u32, o: &Consumer) -> Result<u32, Option<Error>> {
    let mut src = Source::with_address(consumer_address(hub, cg, pid));
    if let Some(f) = &o.filter {
        let described = Value::described(Value::sym(SELECTOR), Value::String(f.clone()));
        src.filter = Some(Value::Map(vec![(Value::sym(SELECTOR), described)]));
    }
    let mut a = Attach::new(format!("consumer-{hub}-{cg}-{pid}"), 0, Role::Receiver);
    a.source = Some(src);
    a.target = Some(TargetKind::Target(Target::with_address("receiver")));
    if o.metrics {
        a.desired_capabilities = vec!["com.microsoft:enable-receiver-runtime-metric".into()];
    }
    if let Some(e) = o.epoch {
        a.properties = Some(Value::Map(vec![(Value::sym("com.microsoft:epoch"), Value::Long(e))]));
    }
    let h = c.attach(a).await?;
    if o.credit > 0 {
        c.flow(h, o.credit, false).await;
    }
    settled(c, h).await
}

/// Event Hubs answers a bad attach with the attach and then a detach carrying the error (not
/// with null termini): give that detach a moment to arrive and report it as the refusal.
pub async fn settled(c: &mut TestClient, h: u32) -> Result<u32, Option<Error>> {
    c.pump_for(Duration::from_millis(80)).await;
    match c.links[&h].detached.clone() {
        Some(e) => Err(e),
        None => Ok(h),
    }
}

pub async fn attach_sender(c: &mut TestClient, address: &str) -> Result<u32, Option<Error>> {
    let h = c.attach_sender(address).await?;
    settled(c, h).await
}

pub async fn attach_receiver(c: &mut TestClient, address: &str, credit: u32) -> Result<u32, Option<Error>> {
    let h = c.attach_receiver(address, credit).await?;
    settled(c, h).await
}

/// Read `n` events from a receiver link and return their bodies.
pub async fn take(c: &mut TestClient, link: u32, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(body_of(&c.recv(link).await));
    }
    out
}

/// Nothing arrives within 300 ms.
pub async fn expect_quiet(c: &mut TestClient, link: u32) {
    if let Some(r) = c.recv_for(link, Duration::from_millis(300)).await {
        panic!("unexpected event {:?}", body_of(&r));
    }
}

/// The condition of a refused attach.
pub fn condition(e: Option<Error>) -> String {
    e.expect("an error").condition
}

/// A request/response pair on `$management` (the SDK shape): returns (request link, response link).
pub async fn mgmt_links(c: &mut TestClient) -> (u32, u32) {
    let resp = c.attach_receiver("$management", 10).await.expect("response link");
    let req = c.attach_sender("$management").await.expect("request link");
    (req, resp)
}

/// One management request; returns (status code, description, body).
pub async fn mgmt(c: &mut TestClient, links: (u32, u32), props: Vec<(&str, &str)>) -> (i64, String, Value) {
    let mut m = TestClient::value_message(Value::Null, props.into_iter().map(|(k, v)| (k, Value::str(v))).collect());
    let id = uuid_like();
    m.properties = Some(azemu_amqp::message::Properties { message_id: Some(Value::Uuid(id)), ..Default::default() });
    m.body = azemu_amqp::message::Body::Empty;
    send_one(c, links.0, &m).await;
    let r = c.recv(links.1).await.message();
    assert_eq!(r.properties.as_ref().and_then(|p| p.correlation_id.clone()), Some(Value::Uuid(id)), "correlation id");
    let status = r.app_property("status-code").and_then(|v| v.as_i64()).expect("status-code");
    let desc = r.app_property("status-description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let body = match r.body {
        azemu_amqp::message::Body::Value(v) => v,
        other => panic!("management body {other:?}"),
    };
    (status, desc, body)
}

fn uuid_like() -> [u8; 16] {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut b = [7u8; 16];
    b[..8].copy_from_slice(&n.to_be_bytes());
    b
}

pub fn map_get<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.map_get(k).unwrap_or_else(|| panic!("key {k} missing in {v}"))
}
