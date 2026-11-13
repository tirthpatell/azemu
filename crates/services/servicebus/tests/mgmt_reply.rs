//! A `$management` response goes to the response link that belongs to the request, not to every
//! response link of the node. Python opens one request/response pair per sender and receiver, all
//! to `<entity>/$management` (no `reply-to`, one session each); Java and Go name the reply
//! address of their response link in `reply-to`.

mod common;
use azemu_amqp::client::TestClient;
use azemu_amqp::message::{Body, Message, Properties};
use azemu_amqp::performatives::{Attach, RcvSettleMode, Role, SndSettleMode};
use azemu_amqp::terminus::{Source, Target, TargetKind};
use azemu_amqp::types::Value;
use common::*;

/// The client's receiving end of a pair: source is the node, target is where replies arrive.
async fn response_link(c: &mut TestClient, node: &str, reply_address: &str) -> u32 {
    let mut a = Attach::new(format!("resp-{reply_address}"), 0, Role::Receiver);
    a.source = Some(Source::with_address(node));
    a.target = Some(TargetKind::Target(Target::with_address(reply_address)));
    a.snd_settle_mode = SndSettleMode::Unsettled;
    a.rcv_settle_mode = RcvSettleMode::First;
    let h = c.attach(a).await.expect("response link");
    c.flow(h, 100, false).await;
    h
}

async fn request(c: &mut TestClient, request_link: u32, id: &str, reply_to: Option<&str>) {
    let m = Message {
        properties: Some(Properties { message_id: Some(Value::str(id)), reply_to: reply_to.map(str::to_string), ..Default::default() }),
        application_properties: Some(vec![
            (Value::str("operation"), Value::str("com.microsoft:peek-message")),
            (Value::str("com.microsoft:server-timeout"), Value::Uint(10000)),
        ]),
        body: Body::Value(Value::Map(vec![
            (Value::str("from-sequence-number"), Value::Long(1)),
            (Value::str("message-count"), Value::Int(1)),
        ])),
        ..Default::default()
    };
    send(c, request_link, &m).await;
}

fn correlation(m: &Message) -> Option<Value> {
    m.properties.as_ref().and_then(|p| p.correlation_id.clone())
}

#[tokio::test]
async fn one_pair_per_session_to_the_same_node_each_gets_its_own_answers() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    // pair 1 on session 0, pair 2 on session 1, as the Python SDK does for two receivers
    let resp1 = response_link(&mut c, "q/$management", "q/$management").await;
    let req1 = sender(&mut c, "q/$management").await;
    c.begin_another_session().await;
    let resp2 = response_link(&mut c, "q/$management", "q/$management").await;
    let req2 = sender(&mut c, "q/$management").await;

    request(&mut c, req2, "from-pair-2", None).await;
    let got = c.recv(resp2).await.message();
    assert_eq!(correlation(&got), Some(Value::str("from-pair-2")));
    no_delivery(&mut c, resp1, 300).await;

    c.channel = 0;
    request(&mut c, req1, "from-pair-1", None).await;
    let got = c.recv(resp1).await.message();
    assert_eq!(correlation(&got), Some(Value::str("from-pair-1")));
    no_delivery(&mut c, resp2, 300).await;
}

#[tokio::test]
async fn reply_to_names_the_response_link_when_pairs_share_a_session() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let resp1 = response_link(&mut c, "q/$management", "client-reply-1").await;
    let resp2 = response_link(&mut c, "q/$management", "client-reply-2").await;
    let req = sender(&mut c, "q/$management").await;

    request(&mut c, req, "a", Some("client-reply-2")).await;
    assert_eq!(correlation(&c.recv(resp2).await.message()), Some(Value::str("a")));
    no_delivery(&mut c, resp1, 300).await;

    request(&mut c, req, "b", Some("client-reply-1")).await;
    assert_eq!(correlation(&c.recv(resp1).await.message()), Some(Value::str("b")));
    no_delivery(&mut c, resp2, 300).await;
}

#[tokio::test]
async fn cbs_pairs_on_two_sessions_do_not_hear_each_others_answers() {
    let e = env().await;
    let mut c = e.client().await;
    let resp1 = response_link(&mut c, "$cbs", "$cbs").await;
    let _req1 = sender(&mut c, "$cbs").await;
    c.begin_another_session().await;
    let resp2 = response_link(&mut c, "$cbs", "$cbs").await;
    let req2 = sender(&mut c, "$cbs").await;
    let m = Message {
        properties: Some(Properties { message_id: Some(Value::str("tok")), ..Default::default() }),
        application_properties: Some(vec![
            (Value::str("operation"), Value::str("put-token")),
            (Value::str("type"), Value::str("servicebus.windows.net:sastoken")),
            (Value::str("name"), Value::str("sb://default/q")),
        ]),
        body: Body::Value(Value::str("SharedAccessSignature sr=x")),
        ..Default::default()
    };
    send(&mut c, req2, &m).await;
    let got = c.recv(resp2).await.message();
    assert_eq!(got.app_property("status-code").and_then(|v| v.as_i64()), Some(202));
    no_delivery(&mut c, resp1, 300).await;
}
