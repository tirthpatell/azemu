#![allow(dead_code)]
//! Shared helpers for the Service Bus integration tests: a server on a free port and an AMQP
//! client built on the test client of `azemu-amqp`.

use std::sync::Arc;
use std::time::Duration;

use azemu_amqp::client::{Received, TestClient};
use azemu_amqp::message::{Body, Message, Properties};
use azemu_amqp::performatives::{RcvSettleMode, SndSettleMode};
use azemu_amqp::terminus::{DeliveryState, Source};
use azemu_amqp::types::Value;
use azemu_core::testing::TestServer;
use azemu_servicebus::core::{Core, Incoming};
use azemu_servicebus::model::Settings;
use azemu_servicebus::rules::Rule;
use azemu_servicebus::state::{now_ms, Shared, State};

pub struct Env {
    pub srv: TestServer,
    pub shared: Arc<Shared>,
}

pub async fn env() -> Env {
    env_with(Shared::new(true, None)).await
}

pub async fn env_with(shared: Arc<Shared>) -> Env {
    let (svc, amqp) = azemu_servicebus::pair_with(shared.clone());
    let srv = TestServer::start_with(vec![svc], vec![amqp]).await;
    Env { srv, shared }
}

/// A server on a data directory (no timed saves: `srv.save_now()` / `srv.shutdown()` write, dropping does not).
pub async fn env_persistent(dir: &std::path::Path) -> Env {
    let shared = Shared::new(true, None);
    let (svc, amqp) = azemu_servicebus::pair_with(shared.clone());
    let data = dir.to_path_buf();
    let srv = TestServer::start_configured(vec![svc], vec![amqp], move |b| b.data_dir(data).save_interval(Duration::ZERO)).await;
    Env { srv, shared }
}

impl Env {
    pub async fn client(&self) -> TestClient {
        self.client_for("default.servicebus.localhost").await
    }

    pub async fn client_for(&self, host: &str) -> TestClient {
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], self.srv.port()).into();
        let mut c = TestClient::connect_sasl(addr, "ANONYMOUS", None).await;
        c.open_session(host).await;
        c
    }

    pub fn state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.shared.state.lock())
    }

    pub fn queue(&self, name: &str, f: impl FnOnce(&mut Settings)) {
        let mut s = Settings::queue_default(now_ms());
        f(&mut s);
        self.state(|st| st.ns("default").create_queue(name, s)).unwrap();
    }

    pub fn topic(&self, name: &str) {
        self.state(|st| st.ns("default").create_topic(name, Settings::topic_default(now_ms()))).unwrap();
    }

    pub fn sub(&self, topic: &str, name: &str, rules: Vec<Rule>, f: impl FnOnce(&mut Settings)) {
        let mut s = Settings::subscription_default(now_ms());
        f(&mut s);
        self.state(|st| st.ns("default").create_subscription(topic, name, s, rules)).unwrap();
    }

    /// Run something against the engine of a queue.
    pub fn core<R>(&self, queue: &str, f: impl FnOnce(&mut Core) -> R) -> R {
        self.state(|st| f(&mut st.ns("default").queue_mut(queue).expect("queue").core))
    }

    pub fn inject_ns(&self, ns: &str, queue: &str) {
        let m = msg("x");
        let size = m.encode().len();
        self.state(|st| st.ns(ns).queue_mut(queue).unwrap().core.enqueue(Incoming::new(m, size), now_ms()).unwrap());
    }

    pub fn inject(&self, queue: &str, m: Message) {
        let size = m.encode().len();
        self.core(queue, |c| c.enqueue(Incoming::new(m, size), now_ms()).unwrap());
    }
}

pub fn msg(body: &str) -> Message {
    Message::data(body.as_bytes().to_vec())
}

pub fn msg_id(body: &str, id: &str) -> Message {
    Message { properties: Some(Properties { message_id: Some(Value::str(id)), ..Default::default() }), ..msg(body) }
}

pub fn msg_session(body: &str, session: &str) -> Message {
    Message { properties: Some(Properties { group_id: Some(session.into()), ..Default::default() }), ..msg(body) }
}

pub fn body(m: &Message) -> String {
    match &m.body {
        Body::Data(parts) => String::from_utf8_lossy(&parts.concat()).to_string(),
        other => format!("{other:?}"),
    }
}

pub fn ann(m: &Message, key: &str) -> Option<Value> {
    m.annotation(key).cloned()
}

pub fn app(m: &Message, key: &str) -> Option<Value> {
    m.app_property(key).cloned()
}

/// Attach a peek-lock receiver on `address` and give it credit.
pub async fn receiver(c: &mut TestClient, address: &str, credit: u32) -> u32 {
    c.attach_receiver_source(Source::with_address(address), SndSettleMode::Unsettled, RcvSettleMode::Second, credit)
        .await
        .expect("attach receiver")
}

pub async fn receiver_rd(c: &mut TestClient, address: &str, credit: u32) -> u32 {
    c.attach_receiver_source(Source::with_address(address), SndSettleMode::Settled, RcvSettleMode::First, credit)
        .await
        .expect("attach receiver")
}

pub async fn sender(c: &mut TestClient, address: &str) -> u32 {
    c.attach_sender(address).await.expect("attach sender")
}

pub async fn send(c: &mut TestClient, h: u32, m: &Message) {
    let out = c.send_message_wait(h, m).await;
    assert_eq!(out, Some(DeliveryState::Accepted), "send outcome");
}

/// Complete a delivery the Python-SDK way: unsettled disposition, wait for the settled echo.
pub async fn settle(c: &mut TestClient, d: &Received, state: DeliveryState) -> Option<DeliveryState> {
    c.disposition_unsettled(d.delivery_id, state).await;
    c.wait_sender_disposition(d.delivery_id).await
}

pub async fn recv_body(c: &mut TestClient, h: u32) -> (Received, Message) {
    let d = c.recv(h).await;
    let m = d.message();
    (d, m)
}

pub async fn no_delivery(c: &mut TestClient, h: u32, ms: u64) {
    assert!(c.recv_for(h, Duration::from_millis(ms)).await.is_none(), "unexpected delivery");
}

/// A request/response pair to `<node>`: `call` sends a request and returns (status, description,
/// condition, body).
pub struct Mgmt {
    pub req: u32,
    pub resp: u32,
    next_id: u32,
}

pub struct Reply {
    pub status: i64,
    pub description: String,
    pub condition: Option<String>,
    pub body: Value,
    pub raw: Message,
}

impl Mgmt {
    pub async fn open(c: &mut TestClient, node: &str) -> Mgmt {
        let resp = receiver_rd(c, node, 100).await;
        let req = sender(c, node).await;
        Mgmt { req, resp, next_id: 0 }
    }

    pub async fn call(&mut self, c: &mut TestClient, op: &str, body: Vec<(&str, Value)>) -> Reply {
        self.call_in(c, op, body, None).await
    }

    /// A request sent as transactional work when `txn` holds a transaction id.
    pub async fn call_in(&mut self, c: &mut TestClient, op: &str, body: Vec<(&str, Value)>, txn: Option<&[u8]>) -> Reply {
        self.next_id += 1;
        let id = format!("req-{}", self.next_id);
        let m = Message {
            properties: Some(Properties { message_id: Some(Value::str(id.clone())), reply_to: Some("reply".into()), ..Default::default() }),
            application_properties: Some(vec![
                (Value::str("operation"), Value::str(op)),
                (Value::str("type"), Value::str("entity-mgmt")),
                (Value::str("com.microsoft:server-timeout"), Value::Uint(10000)),
            ]),
            body: Body::Value(Value::Map(body.into_iter().map(|(k, v)| (Value::str(k), v)).collect())),
            ..Default::default()
        };
        match txn {
            None => send(c, self.req, &m).await,
            Some(t) => {
                let out = c.send_message_in_txn(self.req, &m, t).await;
                assert_eq!(
                    out,
                    Some(DeliveryState::Transactional { txn_id: t.to_vec(), outcome: Some(Box::new(DeliveryState::Accepted)) }),
                    "request outcome"
                );
            }
        }
        let d = c.recv(self.resp).await;
        let raw = d.message();
        assert_eq!(raw.properties.as_ref().and_then(|p| p.correlation_id.clone()), Some(Value::str(id)), "correlation id");
        let key = |k: &str| raw.app_property(k).cloned();
        let body = match &raw.body {
            Body::Value(v) => v.clone(),
            _ => Value::Null,
        };
        Reply {
            status: key("statusCode").and_then(|v| v.as_i64()).expect("statusCode"),
            description: key("statusDescription").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
            condition: key("errorCondition").and_then(|v| v.as_str().map(str::to_string)),
            body,
            raw,
        }
    }
}

impl Mgmt {
    /// Send an already encoded request message (bytes captured from an SDK) and return the reply.
    pub async fn call_raw(&mut self, c: &mut TestClient, payload: &[u8]) -> Reply {
        let id = c.send_payload(self.req, payload, 0, false).await;
        c.wait_disposition(id).await;
        let raw = c.recv(self.resp).await.message();
        let key = |k: &str| raw.app_property(k).cloned();
        let body = match &raw.body {
            Body::Value(v) => v.clone(),
            _ => Value::Null,
        };
        Reply {
            status: key("statusCode").and_then(|v| v.as_i64()).expect("statusCode"),
            description: key("statusDescription").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
            condition: key("errorCondition").and_then(|v| v.as_str().map(str::to_string)),
            body,
            raw,
        }
    }
}

pub fn sym_map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.map_get(key)
}

pub fn uuid_value(token_tag: &[u8]) -> Value {
    // delivery tag (GUID little-endian) to the RFC order the SDK sends in requests
    let t = azemu_servicebus::store::tag_to_token(token_tag).expect("16 byte tag");
    Value::Uuid(t)
}
