//! One entity per transaction (finding SB2). Azure refuses a transaction that works on two entities unless the client is
//! set up for cross-entity transactions. What shows that on the wire: the .NET SDK with `EnableCrossEntityTransactions`
//! puts all its links, and the coordinator, on ONE AMQP session; without it every link has its own session. So a
//! second entity is accepted only from a link on the session of the link that first used the transaction (or from a
//! send-via link, which names its destination). Work on the same entity is accepted from any session.

mod common;
use azemu_amqp::client::TestClient;
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::types::Value;
use common::*;

fn t(txn: &[u8], outcome: DeliveryState) -> DeliveryState {
    DeliveryState::Transactional { txn_id: txn.to_vec(), outcome: Some(Box::new(outcome)) }
}

fn refused_with(out: &Option<DeliveryState>, condition: &str) -> bool {
    match out {
        Some(DeliveryState::Transactional { outcome: Some(o), .. }) => {
            matches!(&**o, DeliveryState::Rejected { error: Some(e) } if e.condition == condition)
        }
        _ => false,
    }
}

/// A client with the coordinator on channel 0 and a second session (channel 1) for the links of "another client".
async fn two_sessions(e: &Env) -> (TestClient, u32, Vec<u8>) {
    let mut c = e.client().await;
    let coord = c.attach_coordinator().await.unwrap();
    let txn = c.declare(coord).await.unwrap();
    (c, coord, txn)
}

#[tokio::test]
async fn a_second_entity_from_another_session_is_refused_and_the_transaction_goes_on() {
    let e = env().await;
    e.queue("a", |_| {});
    e.queue("b", |_| {});
    let (mut c, coord, txn) = two_sessions(&e).await;
    let sa = sender(&mut c, "a").await;
    c.begin_another_session().await;
    let sb = sender(&mut c, "b").await;
    c.channel = 0;
    assert_eq!(c.send_message_in_txn(sa, &msg("to a"), &txn).await, Some(t(&txn, DeliveryState::Accepted)));
    c.channel = 1;
    let out = c.send_message_in_txn(sb, &msg("to b"), &txn).await;
    assert!(refused_with(&out, "amqp:not-allowed"), "{out:?}");
    c.channel = 0;
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("a", |c| c.main.len()), 1);
    assert_eq!(e.core("b", |c| c.main.len()), 0);
}

#[tokio::test]
async fn a_second_entity_on_the_same_session_is_accepted() {
    // a client set up for cross-entity transactions: everything on one session
    let e = env().await;
    e.queue("a", |_| {});
    e.queue("b", |_| {});
    let (mut c, coord, txn) = two_sessions(&e).await;
    let sa = sender(&mut c, "a").await;
    let sb = sender(&mut c, "b").await;
    c.send_message_in_txn(sa, &msg("to a"), &txn).await;
    assert_eq!(c.send_message_in_txn(sb, &msg("to b"), &txn).await, Some(t(&txn, DeliveryState::Accepted)));
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!((e.core("a", |c| c.main.len()), e.core("b", |c| c.main.len())), (1, 1));
}

#[tokio::test]
async fn the_same_entity_from_any_session_is_accepted() {
    let e = env().await;
    e.queue("a", |_| {});
    e.inject("a", msg_id("work", "w1"));
    let (mut c, coord, txn) = two_sessions(&e).await;
    let sa = sender(&mut c, "a").await;
    c.begin_another_session().await;
    let r = receiver(&mut c, "a", 1).await;
    let d = c.recv(r).await;
    c.channel = 0;
    assert_eq!(c.send_message_in_txn(sa, &msg("again"), &txn).await, Some(t(&txn, DeliveryState::Accepted)));
    c.channel = 1;
    assert!(settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await.is_some_and(|s| s == t(&txn, DeliveryState::Accepted)));
    c.channel = 0;
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("a", |c| c.main.len()), 1);
}

#[tokio::test]
async fn a_settlement_in_one_entity_and_a_send_to_another_from_two_sessions_is_refused() {
    // the .NET case without EnableCrossEntityTransactions: complete in queue A (receiver on one session), send to B (another)
    let e = env().await;
    e.queue("a", |_| {});
    e.queue("b", |_| {});
    e.inject("a", msg_id("work", "w1"));
    let (mut c, coord, txn) = two_sessions(&e).await;
    c.begin_another_session().await;
    let r = receiver(&mut c, "a", 1).await;
    let d = c.recv(r).await;
    c.begin_another_session().await;
    let sb = sender(&mut c, "b").await;
    c.channel = 1;
    assert_eq!(settle(&mut c, &d, t(&txn, DeliveryState::Accepted)).await, Some(t(&txn, DeliveryState::Accepted)));
    c.channel = 2;
    let out = c.send_message_in_txn(sb, &msg("out"), &txn).await;
    assert!(refused_with(&out, "amqp:not-allowed"), "{out:?}");
    c.channel = 0;
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("a", |c| c.main.len()), 0, "the completion is applied, nothing was sent to b");
    assert_eq!(e.core("b", |c| c.main.len()), 0);
}

#[tokio::test]
async fn a_send_via_link_may_work_for_another_entity_from_its_own_session() {
    use azemu_amqp::performatives::{Attach, Role};
    use azemu_amqp::terminus::{Source, Target, TargetKind};
    let e = env().await;
    e.queue("a", |_| {});
    e.queue("b", |_| {});
    let (mut c, coord, txn) = two_sessions(&e).await;
    let sa = sender(&mut c, "a").await;
    c.begin_another_session().await;
    let mut a = Attach::new("via", 0, Role::Sender);
    a.source = Some(Source::with_address("sender"));
    a.target = Some(TargetKind::Target(Target::with_address("a")));
    a.properties = Some(Value::Map(vec![(Value::sym("com.microsoft:transfer-destination-address"), Value::str("b"))]));
    let via = c.attach(a).await.unwrap();
    c.channel = 0;
    c.send_message_in_txn(sa, &msg("to a"), &txn).await;
    c.channel = 1;
    assert_eq!(c.send_message_in_txn(via, &msg("to b"), &txn).await, Some(t(&txn, DeliveryState::Accepted)));
    c.channel = 0;
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!((e.core("a", |c| c.main.len()), e.core("b", |c| c.main.len())), (1, 1));
}

#[tokio::test]
async fn transactions_are_bound_one_by_one() {
    // another transaction of the same connection may start on the other entity
    let e = env().await;
    e.queue("a", |_| {});
    e.queue("b", |_| {});
    let (mut c, coord, t1) = two_sessions(&e).await;
    let t2 = c.declare(coord).await.unwrap();
    let sa = sender(&mut c, "a").await;
    c.begin_another_session().await;
    let sb = sender(&mut c, "b").await;
    c.channel = 0;
    c.send_message_in_txn(sa, &msg("1"), &t1).await;
    c.channel = 1;
    assert_eq!(c.send_message_in_txn(sb, &msg("2"), &t2).await, Some(t(&t2, DeliveryState::Accepted)));
    c.channel = 0;
    c.discharge(coord, &t1, false).await.unwrap();
    c.discharge(coord, &t2, false).await.unwrap();
    assert_eq!((e.core("a", |c| c.main.len()), e.core("b", |c| c.main.len())), (1, 1));
}
