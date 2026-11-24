//! Strict authorisation of the Kafka endpoint (`docs/strict-auth.md`): the login (SASL PLAIN with a connection string, SASL OAUTHBEARER with an Entra
//! token) and the rights of what logged in. Each test says which row of what Azure answered in November 2026 it states.

use std::sync::Arc;
use std::time::Duration;

use azemu_core::auth::sas::{Rights, SasRule};
use azemu_core::auth::{Assignment, Auth, Origin, TestAuthority};
use azemu_core::testing::TestServer;
use azemu_eventhubs::kafka::endpoint_with;
use azemu_eventhubs::state::{now_ms, Shared};
use azemu_kafka::api::{self, err};
use azemu_kafka::header::{read_response, write_request};
use azemu_kafka::records::{encode_batch, Batch, Record};
use azemu_kafka::{RequestHeader, Struct, Value};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SENDER: &str = "2b629674-e913-4c01-ae53-ef4638d8f975";
const RECEIVER: &str = "a638d3c7-ab3a-418d-83e6-5f17a39d4fde";
const NS_ID: &str = "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.EventHub/namespaces/default";
const AUD: &str = "https://default.servicebus.windows.net";

fn assign(auth: &Auth, n: u8, role: &str, principal: &str) {
    auth.registry()
        .put_assignment(Assignment {
            id: format!("33333333-3333-3333-3333-3333333333{n:02}"),
            scope: NS_ID.into(),
            role_id: role.into(),
            principal_id: principal.into(),
            principal_type: "ServicePrincipal".into(),
            condition: None,
            condition_version: None,
            description: None,
            created_on: String::new(),
            updated_on: String::new(),
            origin: Origin::Api,
        })
        .unwrap();
}

struct Env {
    srv: TestServer,
}

async fn env() -> Env {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.EventHub/namespaces", "default", NS_ID);
    assign(&auth, 1, SENDER, "sender");
    assign(&auth, 2, RECEIVER, "reader");
    let shared = Shared::new(false, None);
    shared.set_auth(auth);
    let rights = |r: &[&str]| Rights::parse(r.iter().copied()).unwrap();
    shared.sas.declare("default", None, SasRule::declared("default", None, "sendonly", rights(&["Send"]), Some("send-key".into()), None));
    shared.sas.declare(
        "default",
        None,
        SasRule::declared("default", None, "listenonly", rights(&["Listen"]), Some("listen-key".into()), None),
    );
    shared.sas.declare(
        "default",
        Some("t"),
        SasRule::declared("default", Some("t"), "tsend", rights(&["Send", "Listen"]), Some("t-key".into()), None),
    );
    shared.state.lock().create_hub("default", "t", 2, 3_600_000, now_ms()).unwrap();
    shared.state.lock().create_hub("default", "u", 1, 3_600_000, now_ms()).unwrap();
    let (svc, amqp) = azemu_eventhubs::pair_with(shared.clone());
    let kafka = endpoint_with(shared.clone(), Default::default());
    let srv = TestServer::start_with(vec![svc, kafka.clone()], vec![amqp, kafka]).await;
    Env { srv }
}

struct K {
    s: TcpStream,
    corr: i32,
}

impl K {
    async fn connect(env: &Env) -> K {
        K { s: TcpStream::connect(("127.0.0.1", env.srv.port())).await.unwrap(), corr: 0 }
    }

    /// `None`: the connection was closed instead of answering.
    async fn try_call(&mut self, key: i16, version: i16, body: Struct) -> Option<Struct> {
        self.corr += 1;
        let h = RequestHeader { api_key: key, version, correlation_id: self.corr, client_id: Some("rust-test".into()) };
        self.s.write_all(&write_request(&h, &body)).await.ok()?;
        let mut len = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(10), self.s.read_exact(&mut len)).await.ok()?.ok()?;
        let mut frame = vec![0u8; u32::from_be_bytes(len) as usize];
        self.s.read_exact(&mut frame).await.ok()?;
        let (_, resp) = read_response(&h, &frame).ok()?;
        Some(resp)
    }

    async fn call(&mut self, key: i16, version: i16, body: Struct) -> Struct {
        self.try_call(key, version, body).await.expect("an answer")
    }

    async fn is_closed(&mut self) -> bool {
        let mut b = [0u8; 1];
        matches!(tokio::time::timeout(Duration::from_secs(5), self.s.read(&mut b)).await, Ok(Ok(0)) | Ok(Err(_)))
    }

    /// The login. `Ok`, or the message of the refusal.
    async fn login(&mut self, mechanism: &str, auth_bytes: Vec<u8>) -> Result<(), String> {
        let r = self.call(api::SASL_HANDSHAKE, 1, Struct::new().with("Mechanism", mechanism.to_string())).await;
        assert_eq!(r.i16("ErrorCode"), 0);
        let r = self.call(api::SASL_AUTHENTICATE, 1, Struct::new().with("AuthBytes", Bytes::from(auth_bytes))).await;
        match r.i16("ErrorCode") {
            0 => Ok(()),
            c => {
                assert_eq!(c, err::SASL_AUTHENTICATION_FAILED);
                Err(r.str("ErrorMessage").unwrap_or("").to_string())
            }
        }
    }

    async fn plain(&mut self, user: &str, password: &str) -> Result<(), String> {
        self.login("PLAIN", format!("\0{user}\0{password}").into_bytes()).await
    }

    async fn bearer(&mut self, token: &str) -> Result<(), String> {
        self.login("OAUTHBEARER", format!("n,,\u{1}auth=Bearer {token}\u{1}\u{1}").into_bytes()).await
    }
}

fn conn_string(rule: &str, key: &str) -> String {
    format!("Endpoint=sb://default.servicebus.windows.net/;SharedAccessKeyName={rule};SharedAccessKey={key}")
}

fn batch(values: &[&str]) -> Batch {
    Batch {
        base_offset: 0,
        partition_leader_epoch: -1,
        attributes: 0,
        last_offset_delta: values.len() as i32 - 1,
        first_timestamp: now_ms(),
        max_timestamp: now_ms(),
        producer_id: -1,
        producer_epoch: -1,
        base_sequence: -1,
        records: values
            .iter()
            .enumerate()
            .map(|(i, v)| Record {
                offset_delta: i as i32,
                timestamp_delta: 0,
                key: None,
                value: Some(Bytes::from(v.to_string())),
                headers: vec![],
            })
            .collect(),
        wire_len: 0,
    }
}

async fn produce(k: &mut K, topic: &str) -> Option<i16> {
    let body = Struct::new().with("TransactionalId", Value::Null).with("Acks", -1).with("TimeoutMs", 5000).with(
        "TopicData",
        vec![Struct::new()
            .with("Name", topic.to_string())
            .with("PartitionData", vec![Struct::new().with("Index", 0).with("Records", encode_batch(&batch(&["x"]), false))])],
    );
    let r = k.try_call(api::PRODUCE, 7, body).await?;
    let code = r.structs("Responses").next()?.structs("PartitionResponses").next()?.i16("ErrorCode");
    Some(code)
}

async fn fetch(k: &mut K, topic: &str) -> i16 {
    let body = Struct::new()
        .with("MaxWaitMs", 100)
        .with("MinBytes", 0)
        .with("MaxBytes", 1_000_000)
        .with("IsolationLevel", 0)
        .with("SessionId", 0)
        .with("SessionEpoch", -1)
        .with(
            "Topics",
            vec![Struct::new().with("Topic", topic.to_string()).with(
                "Partitions",
                vec![Struct::new()
                    .with("Partition", 0)
                    .with("FetchOffset", 0i64)
                    .with("LogStartOffset", -1i64)
                    .with("PartitionMaxBytes", 1_000_000)],
            )],
        );
    let r = k.call(api::FETCH, 11, body).await;
    let code = r.structs("Responses").next().unwrap().structs("Partitions").next().unwrap().i16("ErrorCode");
    code
}

async fn offset_fetch(k: &mut K, topic: &str) -> i16 {
    let body = Struct::new().with("GroupId", "$Default").with(
        "Topics",
        Value::Array(vec![Value::Struct(Struct::new().with("Name", topic.to_string()).with("PartitionIndexes", vec![0i32]))]),
    );
    let r = k.call(api::OFFSET_FETCH, 6, body).await;
    let code = r.structs("Topics").next().unwrap().structs("Partitions").next().unwrap().i16("ErrorCode");
    code
}

async fn find_coordinator(k: &mut K) -> Option<i16> {
    k.try_call(api::FIND_COORDINATOR, 3, Struct::new().with("Key", "$Default").with("KeyType", 0)).await.map(|r| r.i16("ErrorCode"))
}

#[tokio::test]
async fn the_login_checks_the_user_the_string_the_policy_and_the_key() {
    let env = env().await;
    // wrong key and unknown policy: the same text
    let mut k = K::connect(&env).await;
    assert_eq!(
        k.plain("$ConnectionString", &conn_string("RootManageSharedAccessKey", "wrong")).await,
        Err("Invalid SASL PLAIN password.".into())
    );
    assert!(k.is_closed().await, "the connection is closed after a failed login");
    let mut k = K::connect(&env).await;
    assert_eq!(k.plain("$ConnectionString", &conn_string("nobody", "SAS_KEY_VALUE")).await, Err("Invalid SASL PLAIN password.".into()));
    let mut k = K::connect(&env).await;
    assert_eq!(
        k.plain("someone", &conn_string("RootManageSharedAccessKey", "SAS_KEY_VALUE")).await,
        Err("Invalid SASL PLAIN user name.".into())
    );
    let mut k = K::connect(&env).await;
    let e = k.plain("$ConnectionString", "not a connection string").await.unwrap_err();
    assert!(e.starts_with("The Service Bus connection string is not of the expected format."), "{e}");
    // the root rule with the development key, and the rule of one hub
    let mut k = K::connect(&env).await;
    assert_eq!(k.plain("$ConnectionString", &conn_string("RootManageSharedAccessKey", "SAS_KEY_VALUE")).await, Ok(()));
    let mut k = K::connect(&env).await;
    assert_eq!(k.plain("$ConnectionString", &format!("{};EntityPath=t", conn_string("tsend", "t-key"))).await, Ok(()));
    // the rule of a hub is no rule without the hub in the string
    let mut k = K::connect(&env).await;
    assert!(k.plain("$ConnectionString", &conn_string("tsend", "t-key")).await.is_err());
}

#[tokio::test]
async fn a_request_before_the_login_is_not_answered() {
    let env = env().await;
    let mut k = K::connect(&env).await;
    assert!(find_coordinator(&mut k).await.is_none());
    // ApiVersions is what a client sends first
    let mut k = K::connect(&env).await;
    assert!(k.try_call(api::API_VERSIONS, 0, Struct::new()).await.is_some());
}

#[tokio::test]
async fn produce_needs_send_and_consume_needs_listen_with_a_connection_string() {
    let env = env().await;
    // root: both
    let mut k = K::connect(&env).await;
    k.plain("$ConnectionString", &conn_string("RootManageSharedAccessKey", "SAS_KEY_VALUE")).await.unwrap();
    assert_eq!(produce(&mut k, "t").await, Some(0));
    assert_eq!(fetch(&mut k, "t").await, 0);
    // a listen-only string: produce is TOPIC_AUTHORIZATION_FAILED (29) and the connection stays; consume works
    let mut k = K::connect(&env).await;
    k.plain("$ConnectionString", &conn_string("listenonly", "listen-key")).await.unwrap();
    assert_eq!(produce(&mut k, "t").await, Some(err::TOPIC_AUTHORIZATION_FAILED));
    assert_eq!(err::TOPIC_AUTHORIZATION_FAILED, 29);
    assert_eq!(find_coordinator(&mut k).await, Some(0));
    assert_eq!(offset_fetch(&mut k, "t").await, 0);
    assert_eq!(fetch(&mut k, "t").await, 0);
    // a send-only string: the coordinator lookup works, the offset fetch is refused per partition, a fetch too
    let mut k = K::connect(&env).await;
    k.plain("$ConnectionString", &conn_string("sendonly", "send-key")).await.unwrap();
    assert_eq!(produce(&mut k, "t").await, Some(0));
    assert_eq!(find_coordinator(&mut k).await, Some(0));
    assert_eq!(offset_fetch(&mut k, "t").await, err::TOPIC_AUTHORIZATION_FAILED);
    assert_eq!(fetch(&mut k, "t").await, err::TOPIC_AUTHORIZATION_FAILED);
    // the rule of one hub, used for another hub in the same connection
    let mut k = K::connect(&env).await;
    k.plain("$ConnectionString", &format!("{};EntityPath=t", conn_string("tsend", "t-key"))).await.unwrap();
    assert_eq!(produce(&mut k, "t").await, Some(0));
    assert_eq!(produce(&mut k, "u").await, Some(err::TOPIC_AUTHORIZATION_FAILED));
}

#[tokio::test]
async fn an_entra_principal_needs_a_role() {
    let env = env().await;
    let token = |who: &str| TestAuthority::token(who, AUD);
    // a sender produces; its consume fails at the offset fetch
    let mut k = K::connect(&env).await;
    k.bearer(&token("sender")).await.unwrap();
    assert_eq!(produce(&mut k, "t").await, Some(0));
    // a reader consumes and cannot produce (the connection stays: it has a right)
    let mut k = K::connect(&env).await;
    k.bearer(&token("reader")).await.unwrap();
    assert_eq!(produce(&mut k, "t").await, Some(err::TOPIC_AUTHORIZATION_FAILED));
    assert_eq!(find_coordinator(&mut k).await, Some(0));
    assert_eq!(fetch(&mut k, "t").await, 0);
    // no role: the produce is refused and the connection is closed after the answer; a consumer is closed at the coordinator lookup
    let mut k = K::connect(&env).await;
    k.bearer(&token("norole")).await.unwrap();
    assert_eq!(produce(&mut k, "t").await, Some(err::TOPIC_AUTHORIZATION_FAILED));
    assert!(k.is_closed().await);
    let mut k = K::connect(&env).await;
    k.bearer(&token("norole")).await.unwrap();
    assert_eq!(find_coordinator(&mut k).await, None);
}

#[tokio::test]
async fn a_bearer_token_that_is_wrong_is_refused_with_azures_text() {
    let env = env().await;
    let good = TestAuthority::token("sender", AUD);
    let mut k = K::connect(&env).await;
    assert_eq!(k.bearer("a-made-up-token").await, Err("Invalid SASL OAUTHBEARER message.".into()));
    let mut k = K::connect(&env).await;
    let e = k.bearer(&format!("{good}x")).await.unwrap_err();
    assert!(e.starts_with("InvalidSignature: Invalid authorization token signature. TrackingId:"), "{e}");
    let mut k = K::connect(&env).await;
    assert_eq!(
        k.bearer(&TestAuthority::token("sender", "https://vault.azure.net")).await,
        Err("Invalid URI: The format of the URI could not be determined.".into())
    );
}

#[tokio::test]
async fn with_the_mode_off_the_login_is_not_checked() {
    let shared = Shared::new(true, None);
    let (svc, amqp) = azemu_eventhubs::pair_with(shared.clone());
    let kafka = endpoint_with(shared, Default::default());
    let srv = TestServer::start_with(vec![svc, kafka.clone()], vec![amqp, kafka]).await;
    let env = Env { srv };
    let mut k = K::connect(&env).await;
    assert_eq!(k.plain("anyone", "anything").await, Ok(()));
    assert!(produce(&mut k, "made").await.is_some());
    let mut k = K::connect(&env).await;
    assert_eq!(k.bearer("made-up").await, Ok(()));
}
