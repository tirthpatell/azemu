//! A wait that a fault rule asks for (`latency`, `timeout`) on a Kafka connection ends when the client goes away: a client that gives up and
//! tries again must not keep a connection slot for the whole wait (`docs/faults.md`).

use std::time::Duration;

use azemu_core::testing::TestServer;
use azemu_eventhubs::kafka::{endpoint_with, KafkaConfig};
use azemu_eventhubs::state::Shared;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `ApiVersions` v0 with no client id.
const API_VERSIONS: [u8; 14] = [0, 0, 0, 10, 0, 18, 0, 0, 0, 0, 0, 1, 0xff, 0xff];

async fn rule(srv: &TestServer, json: serde_json::Value) {
    let r =
        srv.send("POST", &format!("{}/_azemu/faults", srv.gateway_url()), &[("content-type", "application/json")], json.to_string()).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

async fn slots_free(kafka: &azemu_eventhubs::kafka::KafkaEndpoint, why: &str) {
    for _ in 0..300 {
        if kafka.open_connections() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{why}: the connection is still held ({} open)", kafka.open_connections());
}

async fn start() -> (TestServer, std::sync::Arc<azemu_eventhubs::kafka::KafkaEndpoint>) {
    let shared = Shared::new(true, None);
    let (svc, amqp) = azemu_eventhubs::pair_with(shared.clone());
    let kafka = endpoint_with(shared, KafkaConfig::default());
    let srv = TestServer::start_with(vec![svc, kafka.clone()], vec![amqp, kafka.clone()]).await;
    (srv, kafka)
}

#[tokio::test]
async fn a_latency_on_a_request_ends_when_the_client_goes_away() {
    let (srv, kafka) = start().await;
    rule(&srv, serde_json::json!({"service":"kafka","operation":"management","fault":{"kind":"latency","ms":3_600_000}})).await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.write_all(&API_VERSIONS).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(kafka.open_connections(), 1, "the request is waiting");
    drop(s);
    slots_free(&kafka, "latency").await;
}

#[tokio::test]
async fn a_timeout_on_a_request_ends_when_the_client_goes_away() {
    let (srv, kafka) = start().await;
    rule(&srv, serde_json::json!({"service":"kafka","operation":"management","fault":{"kind":"timeout"}})).await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.write_all(&API_VERSIONS).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(kafka.open_connections(), 1);
    drop(s);
    slots_free(&kafka, "timeout").await;
}

#[tokio::test]
async fn a_latency_on_the_connection_itself_ends_when_the_client_goes_away() {
    let (srv, kafka) = start().await;
    rule(&srv, serde_json::json!({"service":"kafka","operation":"connect","fault":{"kind":"latency","ms":3_600_000}})).await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    s.write_all(&API_VERSIONS).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(s);
    slots_free(&kafka, "connect latency").await;
}

#[tokio::test]
async fn a_request_sent_during_the_wait_is_not_lost() {
    let (srv, _kafka) = start().await;
    rule(&srv, serde_json::json!({"service":"kafka","operation":"management","times":1,"fault":{"kind":"latency","ms":600}})).await;
    let mut s = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    // two requests back to back: the first waits, the second arrives while it does; both are answered, in order
    let mut two = API_VERSIONS.to_vec();
    let mut second = API_VERSIONS;
    second[11] = 2; // correlation id
    two.extend_from_slice(&second);
    s.write_all(&two).await.unwrap();
    for want in [1u8, 2u8] {
        let mut len = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut len)).await.expect("answered").unwrap();
        let mut frame = vec![0u8; u32::from_be_bytes(len) as usize];
        s.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame[3], want, "the correlation id of the answer");
    }
}
