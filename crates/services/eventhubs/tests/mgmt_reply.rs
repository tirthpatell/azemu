//! A `$management` response goes to the response link that belongs to the request, not to every
//! response link of the node. The Event Hubs SDKs open one request/response pair per session;
//! Java and Go name the reply address of their response link in `reply-to`, Python sends none.

mod common;
use azemu_amqp::client::TestClient;
use azemu_amqp::message::{Body, Message, Properties};
use azemu_amqp::performatives::{Attach, RcvSettleMode, Role, SndSettleMode};
use azemu_amqp::terminus::{Source, Target, TargetKind};
use azemu_amqp::types::Value;
use common::*;

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
    let mut m = TestClient::value_message(
        Value::Null,
        vec![("operation", Value::str("READ")), ("type", Value::str("com.microsoft:eventhub")), ("name", Value::str("h"))],
    );
    m.properties = Some(Properties { message_id: Some(Value::str(id)), reply_to: reply_to.map(str::to_string), ..Default::default() });
    m.body = Body::Empty;
    send_one(c, request_link, &m).await;
}

fn correlation(m: &Message) -> Option<Value> {
    m.properties.as_ref().and_then(|p| p.correlation_id.clone())
}

#[tokio::test]
async fn one_pair_per_session_each_gets_its_own_answers() {
    let env = env().await;
    env.hub("h", 2);
    let mut c = env.client().await;
    let resp1 = response_link(&mut c, "$management", "$management").await;
    let req1 = attach_sender(&mut c, "$management").await.unwrap();
    c.begin_another_session().await;
    let resp2 = response_link(&mut c, "$management", "$management").await;
    let req2 = attach_sender(&mut c, "$management").await.unwrap();

    request(&mut c, req2, "from-pair-2", None).await;
    assert_eq!(correlation(&c.recv(resp2).await.message()), Some(Value::str("from-pair-2")));
    expect_quiet(&mut c, resp1).await;

    c.channel = 0;
    request(&mut c, req1, "from-pair-1", None).await;
    assert_eq!(correlation(&c.recv(resp1).await.message()), Some(Value::str("from-pair-1")));
    expect_quiet(&mut c, resp2).await;
}

#[tokio::test]
async fn reply_to_names_the_response_link_when_pairs_share_a_session() {
    let env = env().await;
    env.hub("h", 2);
    let mut c = env.client().await;
    let resp1 = response_link(&mut c, "$management", "client-reply-1").await;
    let resp2 = response_link(&mut c, "$management", "client-reply-2").await;
    let req = attach_sender(&mut c, "$management").await.unwrap();

    request(&mut c, req, "a", Some("client-reply-2")).await;
    assert_eq!(correlation(&c.recv(resp2).await.message()), Some(Value::str("a")));
    expect_quiet(&mut c, resp1).await;

    request(&mut c, req, "b", Some("client-reply-1")).await;
    assert_eq!(correlation(&c.recv(resp1).await.message()), Some(Value::str("b")));
    expect_quiet(&mut c, resp2).await;
}

#[tokio::test]
async fn the_pair_created_second_is_not_mixed_up_with_the_first() {
    // Before, request and response links were paired in the order they attached. Here the two
    // response links attach first, then the request links in the opposite order.
    let env = env().await;
    env.hub("h", 2);
    let mut c = env.client().await;
    let resp1 = response_link(&mut c, "$management", "$management").await;
    c.begin_another_session().await;
    let resp2 = response_link(&mut c, "$management", "$management").await;
    let req2 = attach_sender(&mut c, "$management").await.unwrap();
    c.channel = 0;
    let req1 = attach_sender(&mut c, "$management").await.unwrap();
    request(&mut c, req1, "one", None).await;
    assert_eq!(correlation(&c.recv(resp1).await.message()), Some(Value::str("one")));
    expect_quiet(&mut c, resp2).await;
    c.channel = 1;
    request(&mut c, req2, "two", None).await;
    assert_eq!(correlation(&c.recv(resp2).await.message()), Some(Value::str("two")));
    expect_quiet(&mut c, resp1).await;
}
