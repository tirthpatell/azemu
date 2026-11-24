//! Strict authorisation of Service Bus and Event Hubs with namespaces made through Resource Manager (`docs/strict-auth.md`): the keys `listKeys` hands out
//! are the keys the data plane checks, a regenerated key takes effect at once, and a namespace Resource Manager made has no development key.

use azemu_amqp::client::TestClient;
use azemu_amqp::message::Properties;
use azemu_amqp::types::Value as Amqp;
use azemu_core::auth::sas::{make_token, DEV_KEY};
use azemu_core::auth::Auth;
use azemu_core::serde_json::{json, Value};
use azemu_core::testing::{TestResponse, TestServer};

const GROUP: &str = "/subscriptions/11111111-2222-3333-4444-555555555555/resourceGroups/rg";

struct Env {
    srv: TestServer,
}

impl Env {
    async fn arm(&self, method: &str, path: &str, body: Value) -> TestResponse {
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = format!("http://management.localhost:{}{path}{sep}api-version=2022-10-01-preview", self.srv.port());
        self.srv.send(method, &url, &[("authorization", "Bearer arm"), ("content-type", "application/json")], body.to_string()).await
    }

    async fn put(&self, path: &str, body: Value) {
        let r = self.arm("PUT", path, body).await;
        assert!(r.status < 300, "PUT {path}: {} {}", r.status, r.text());
    }

    /// `(primary, secondary)` of a rule.
    async fn keys(&self, rule: &str) -> (String, String) {
        let r = self.arm("POST", &format!("{rule}/listKeys"), json!({})).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let j = r.json();
        (j["primaryKey"].as_str().unwrap().into(), j["secondaryKey"].as_str().unwrap().into())
    }

    async fn client(&self, host: &str) -> TestClient {
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], self.srv.port()).into();
        let mut c = TestClient::connect_sasl(addr, "ANONYMOUS", None).await;
        c.open_session(host).await;
        c
    }
}

fn unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// `put-token`: the status and the description.
async fn put_token(c: &mut TestClient, links: (u32, u32), host: &str, entity: &str, rule: &str, key: &str) -> (i64, String) {
    let sr = format!("sb://{host}/{entity}");
    let mut m = TestClient::value_message(
        Amqp::str(make_token(&sr, unix() + 600, rule, key)),
        vec![
            ("operation", Amqp::str("put-token")),
            ("type", Amqp::str("servicebus.windows.net:sastoken")),
            ("name", Amqp::str(sr.clone())),
        ],
    );
    m.properties = Some(Properties { message_id: Some(Amqp::Uuid([5; 16])), ..Default::default() });
    assert_eq!(c.send_message_wait(links.1, &m).await, Some(azemu_amqp::terminus::DeliveryState::Accepted));
    let r = c.recv(links.0).await.message();
    (
        r.app_property("status-code").and_then(|v| v.as_i64()).unwrap(),
        r.app_property("status-description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    )
}

async fn cbs(c: &mut TestClient) -> (u32, u32) {
    let resp = c.attach_receiver("$cbs", 20).await.unwrap();
    let req = c.attach_sender("$cbs").await.unwrap();
    (resp, req)
}

#[tokio::test]
async fn the_keys_resource_manager_hands_out_are_the_keys_service_bus_checks() {
    let stack = azemu::full_stack_with(Auth::strict());
    let env = Env { srv: TestServer::start_with(stack.services, stack.raw).await };
    env.put(GROUP, json!({"location": "eastus"})).await;
    let ns = format!("{GROUP}/providers/Microsoft.ServiceBus/namespaces/armsbns1");
    env.put(&ns, json!({"location": "eastus", "sku": {"name": "Standard"}, "properties": {}})).await;
    env.put(&format!("{ns}/queues/q"), json!({"properties": {}})).await;
    env.put(&format!("{ns}/authorizationRules/sender"), json!({"properties": {"rights": ["Send"]}})).await;
    let host = "armsbns1.servicebus.localhost";
    let (primary, secondary) = env.keys(&format!("{ns}/authorizationRules/sender")).await;
    let (root_key, _) = env.keys(&format!("{ns}/authorizationRules/RootManageSharedAccessKey")).await;

    let mut c = env.client(host).await;
    let l = cbs(&mut c).await;
    // both keys of the rule, and the root rule's key
    // (a token put again for the same audience replaces the one before it: the root rule goes first)
    assert_eq!(put_token(&mut c, l, host, "q", "RootManageSharedAccessKey", &root_key).await.0, 202);
    assert_eq!(put_token(&mut c, l, host, "q", "sender", &secondary).await.0, 202);
    assert_eq!(put_token(&mut c, l, host, "q", "sender", &primary).await.0, 202);
    // the rule's rights: it sends and does not receive
    c.attach_sender("q").await.expect("a rule with Send sends");
    assert!(c.attach_receiver("q", 1).await.is_err(), "a rule with Send does not receive");
    // a namespace Resource Manager made has no development key
    let (status, text) = put_token(&mut c, l, host, "q", "RootManageSharedAccessKey", DEV_KEY).await;
    assert_eq!(status, 401, "{text}");
    assert!(text.starts_with("InvalidSignature: The token has an invalid signature."), "{text}");

    // a regenerated key takes effect at once: the old one is refused, the new one works, the other key of the rule is unchanged
    let r = env.arm("POST", &format!("{ns}/authorizationRules/sender/regenerateKeys"), json!({"keyType": "PrimaryKey"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let fresh = r.json()["primaryKey"].as_str().unwrap().to_string();
    assert_ne!(fresh, primary);
    assert_eq!(put_token(&mut c, l, host, "q", "sender", &primary).await.0, 401);
    assert_eq!(put_token(&mut c, l, host, "q", "sender", &fresh).await.0, 202);
    assert_eq!(put_token(&mut c, l, host, "q", "sender", &secondary).await.0, 202);

    // a rule that is deleted is an unknown key name
    let r = env.arm("DELETE", &format!("{ns}/authorizationRules/sender"), json!({})).await;
    assert!(r.status < 300, "{}", r.text());
    let (status, text) = put_token(&mut c, l, host, "q", "sender", &fresh).await;
    assert_eq!(status, 401);
    assert!(text.starts_with("InvalidSignature: The token has an invalid signature. TrackingId:"), "{text}");

    // a rule of one queue, made on the queue: its keys come from the queue's own listKeys
    env.put(&format!("{ns}/queues/q/authorizationRules/qlisten"), json!({"properties": {"rights": ["Listen"]}})).await;
    let (qkey, _) = env.keys(&format!("{ns}/queues/q/authorizationRules/qlisten")).await;
    assert_eq!(put_token(&mut c, l, host, "q", "qlisten", &qkey).await.0, 202);
    env.put(&format!("{ns}/queues/other"), json!({"properties": {}})).await;
    assert_eq!(put_token(&mut c, l, host, "other", "qlisten", &qkey).await.0, 401);
}

#[tokio::test]
async fn the_keys_resource_manager_hands_out_are_the_keys_event_hubs_checks_and_kafka_logs_in_with_them() {
    use azemu_kafka::{api, Struct};

    let stack = azemu::full_stack_with(Auth::strict());
    let env = Env { srv: TestServer::start_with(stack.services, stack.raw).await };
    env.put(GROUP, json!({"location": "eastus"})).await;
    let ns = format!("{GROUP}/providers/Microsoft.EventHub/namespaces/armehns1");
    env.put(&ns, json!({"location": "eastus", "sku": {"name": "Standard"}, "properties": {}})).await;
    env.put(&format!("{ns}/eventhubs/hub1"), json!({"properties": {"partitionCount": 2}})).await;
    env.put(&format!("{ns}/authorizationRules/listener"), json!({"properties": {"rights": ["Listen"]}})).await;
    let (key, _) = env.keys(&format!("{ns}/authorizationRules/listener")).await;
    let host = "armehns1.eventhub.localhost";

    // AMQP: a rule that listens attaches a receiver and not a sender
    let mut c = env.client(host).await;
    let l = cbs(&mut c).await;
    assert_eq!(put_token(&mut c, l, host, "hub1", "listener", &key).await.0, 202);
    assert!(c.attach_sender("hub1").await.is_err());
    assert_eq!(put_token(&mut c, l, host, "hub1", "listener", "SAS_KEY_VALUE").await.0, 401);

    // Kafka: the connection string from `listKeys` is the password
    let conn = format!("Endpoint=sb://{host}/;SharedAccessKeyName=listener;SharedAccessKey={key}");
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", env.srv.port())).await.unwrap();
    let r = kafka_call(&mut s, 1, api::SASL_HANDSHAKE, Struct::new().with("Mechanism", "PLAIN".to_string())).await;
    assert_eq!(r.i16("ErrorCode"), 0);
    let auth = Struct::new().with("AuthBytes", bytes::Bytes::from(format!("\0$ConnectionString\0{conn}").into_bytes()));
    let r = kafka_call(&mut s, 2, api::SASL_AUTHENTICATE, auth).await;
    assert_eq!(r.i16("ErrorCode"), 0, "{:?}", r.str("ErrorMessage"));
    let wrong = format!("Endpoint=sb://{host}/;SharedAccessKeyName=listener;SharedAccessKey=SAS_KEY_VALUE");
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", env.srv.port())).await.unwrap();
    kafka_call(&mut s, 1, api::SASL_HANDSHAKE, Struct::new().with("Mechanism", "PLAIN".to_string())).await;
    let auth = Struct::new().with("AuthBytes", bytes::Bytes::from(format!("\0$ConnectionString\0{wrong}").into_bytes()));
    let r = kafka_call(&mut s, 2, api::SASL_AUTHENTICATE, auth).await;
    assert_eq!(r.str("ErrorMessage"), Some("Invalid SASL PLAIN password."));
}

async fn kafka_call(s: &mut tokio::net::TcpStream, corr: i32, key: i16, body: azemu_kafka::Struct) -> azemu_kafka::Struct {
    use azemu_kafka::header::{read_response, write_request};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let h = azemu_kafka::RequestHeader { api_key: key, version: 1, correlation_id: corr, client_id: Some("t".into()) };
    s.write_all(&write_request(&h, &body)).await.unwrap();
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await.unwrap();
    let mut frame = vec![0u8; u32::from_be_bytes(len) as usize];
    s.read_exact(&mut frame).await.unwrap();
    read_response(&h, &frame).unwrap().1
}
