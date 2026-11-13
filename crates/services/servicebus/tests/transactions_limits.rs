//! Bytes held by open transactions are limited per transaction, per connection and for the service, and everything a transaction holds is let go on every way out of it.
//! The limits are set small here, so the tests hold only a few hundred KB.

mod common;
use std::time::Duration;

use azemu_amqp::client::TestClient;
use azemu_amqp::terminus::DeliveryState;
use azemu_servicebus::txn::TxnLimits;
use common::*;

fn t(txn: &[u8], outcome: DeliveryState) -> DeliveryState {
    DeliveryState::Transactional { txn_id: txn.to_vec(), outcome: Some(Box::new(outcome)) }
}

async fn controller(c: &mut TestClient) -> (u32, Vec<u8>) {
    let coord = c.attach_coordinator().await.expect("coordinator attach");
    let txn = c.declare(coord).await.expect("declare");
    (coord, txn)
}

fn sized(id: &str, bytes: usize) -> azemu_amqp::message::Message {
    let mut m = msg_id("b", id);
    m.body = azemu_amqp::message::Body::Data(vec![vec![b'x'; bytes]]);
    m
}

fn held(e: &Env) -> usize {
    e.shared.txn_budget.used()
}

fn limits(e: &Env, per_txn: usize, per_connection: usize, total: usize) {
    e.shared.txn_budget.set_limits(TxnLimits { per_txn, per_connection, total });
}

fn is_limit_error(out: &Option<DeliveryState>) -> bool {
    match out {
        Some(DeliveryState::Transactional { outcome: Some(o), .. }) => {
            matches!(&**o, DeliveryState::Rejected { error: Some(e) } if e.condition == "amqp:resource-limit-exceeded")
        }
        _ => false,
    }
}

async fn eventually(what: &str, f: impl Fn() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what}");
}

#[tokio::test]
async fn one_transaction_cannot_hold_more_than_its_limit() {
    let e = env().await;
    e.queue("q", |_| {});
    limits(&e, 100_000, 10_000_000, 10_000_000);
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    let mut accepted = 0;
    for i in 0..10 {
        let out = c.send_message_in_txn(s, &sized(&format!("m{i}"), 30_000), &txn).await;
        if is_limit_error(&out) {
            break;
        }
        assert_eq!(out, Some(t(&txn, DeliveryState::Accepted)));
        accepted += 1;
    }
    assert_eq!(accepted, 3, "3 x about 30.5 KB fit into 100,000 bytes, the fourth does not");
    assert!(held(&e) <= 100_000, "{}", held(&e));
    // the transaction is still good: what was accepted commits, and the bytes are let go
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("q", |c| c.main.len()), 3);
    assert_eq!(held(&e), 0);
}

#[tokio::test]
async fn all_transactions_of_a_connection_share_a_limit() {
    let e = env().await;
    e.queue("q", |_| {});
    limits(&e, 1_000_000, 150_000, 10_000_000);
    let mut c = e.client().await;
    let (coord, a) = controller(&mut c).await;
    let b = c.declare(coord).await.unwrap();
    let s = sender(&mut c, "q").await;
    assert_eq!(c.send_message_in_txn(s, &sized("a1", 60_000), &a).await, Some(t(&a, DeliveryState::Accepted)));
    assert_eq!(c.send_message_in_txn(s, &sized("b1", 60_000), &b).await, Some(t(&b, DeliveryState::Accepted)));
    // 2 x 60.5 KB are held; a third of that size in either does not fit
    assert!(is_limit_error(&c.send_message_in_txn(s, &sized("a2", 60_000), &a).await));
    assert!(is_limit_error(&c.send_message_in_txn(s, &sized("b2", 60_000), &b).await));
    // another connection has its own share
    let mut c2 = e.client().await;
    let (_coord2, x) = controller(&mut c2).await;
    let s2 = sender(&mut c2, "q").await;
    assert_eq!(c2.send_message_in_txn(s2, &sized("x1", 60_000), &x).await, Some(t(&x, DeliveryState::Accepted)));
    // finishing one transaction makes room for the other
    c.discharge(coord, &a, true).await.unwrap();
    assert_eq!(c.send_message_in_txn(s, &sized("b2", 60_000), &b).await, Some(t(&b, DeliveryState::Accepted)));
}

#[tokio::test]
async fn the_service_has_a_limit_over_all_connections() {
    let e = env().await;
    e.queue("q", |_| {});
    limits(&e, 1_000_000, 1_000_000, 150_000);
    let mut c1 = e.client().await;
    let mut c2 = e.client().await;
    let (_k1, a) = controller(&mut c1).await;
    let (_k2, b) = controller(&mut c2).await;
    let s1 = sender(&mut c1, "q").await;
    let s2 = sender(&mut c2, "q").await;
    assert_eq!(c1.send_message_in_txn(s1, &sized("a1", 60_000), &a).await, Some(t(&a, DeliveryState::Accepted)));
    assert_eq!(c2.send_message_in_txn(s2, &sized("b1", 60_000), &b).await, Some(t(&b, DeliveryState::Accepted)));
    assert!(is_limit_error(&c1.send_message_in_txn(s1, &sized("a2", 60_000), &a).await));
    assert!(is_limit_error(&c2.send_message_in_txn(s2, &sized("b2", 60_000), &b).await));
    assert!(held(&e) <= 150_000);
}

#[tokio::test]
async fn settlements_and_schedules_count_too() {
    let e = env().await;
    e.queue("q", |_| {});
    for i in 0..40 {
        e.inject("q", msg_id("w", &format!("w{i}")));
    }
    // 512 bytes of fixed cost each: 40 settlements are 20 KB, so 10,000 bytes (minus 1,024 for the transaction itself) hold 17 of them
    limits(&e, 10_000, 10_000_000, 10_000_000);
    let mut c = e.client().await;
    let r = receiver(&mut c, "q", 40).await;
    let mut ds = Vec::new();
    for _ in 0..40 {
        ds.push(c.recv(r).await);
    }
    let (coord, txn) = controller(&mut c).await;
    let mut refused = 0;
    for d in &ds {
        match settle(&mut c, d, t(&txn, DeliveryState::Accepted)).await {
            Some(DeliveryState::Transactional { outcome: Some(o), .. }) if matches!(&*o, DeliveryState::Rejected { .. }) => refused += 1,
            Some(_) => {}
            None => panic!("no answer"),
        }
    }
    assert_eq!(refused, 40 - 17);
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("q", |c| c.main.len()), 23);
    assert_eq!(held(&e), 0);
}

#[tokio::test]
async fn everything_is_let_go_on_every_way_out() {
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    // commit
    c.send_message_in_txn(s, &sized("c1", 20_000), &txn).await;
    assert!(held(&e) > 20_000);
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(held(&e), 0, "commit");
    // a second discharge of the same transaction is unknown and releases nothing twice
    let other = c.declare(coord).await.unwrap();
    c.send_message_in_txn(s, &sized("o1", 20_000), &other).await;
    let before = held(&e);
    assert!(c.discharge(coord, &txn, false).await.is_err());
    assert!(c.discharge(coord, &txn, true).await.is_err());
    assert_eq!(held(&e), before, "second discharge");
    // rollback
    c.discharge(coord, &other, true).await.unwrap();
    assert_eq!(held(&e), 0, "rollback");
    // the coordinator link ends
    let coord2 = c.attach_coordinator().await.unwrap();
    let t2 = c.declare(coord2).await.unwrap();
    c.send_message_in_txn(s, &sized("d1", 20_000), &t2).await;
    assert!(held(&e) > 0);
    c.detach(coord2).await;
    assert_eq!(held(&e), 0, "coordinator detach");
    // the session ends
    let mut c2 = e.client().await;
    let (_k, t3) = controller(&mut c2).await;
    let s3 = sender(&mut c2, "q").await;
    c2.send_message_in_txn(s3, &sized("e1", 20_000), &t3).await;
    assert!(held(&e) > 0);
    c2.end_session().await;
    eventually("session end", || held(&e) == 0).await;
    // the connection is dropped
    let mut c3 = e.client().await;
    let (_k, t4) = controller(&mut c3).await;
    let s4 = sender(&mut c3, "q").await;
    c3.send_message_in_txn(s4, &sized("f1", 20_000), &t4).await;
    assert!(held(&e) > 0);
    drop(c3);
    eventually("connection drop", || held(&e) == 0).await;
}

#[tokio::test]
async fn a_transaction_that_times_out_lets_go_without_anyone_touching_it() {
    let shared = azemu_servicebus::state::Shared::new(true, None);
    shared.set_txn_timeout(Duration::from_millis(300));
    let e = env_with(shared).await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (_coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    c.send_message_in_txn(s, &sized("t1", 20_000), &txn).await;
    assert!(held(&e) > 20_000);
    // no request on this connection from here on: the connection's own sweep must free it
    eventually("time out", || held(&e) == 0).await;
    // and the transaction is told it timed out, once
    let coord = 0;
    let err = c.discharge(coord, &txn, false).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:transaction:timeout");
}

#[tokio::test]
async fn real_use_fits_the_default_limits() {
    // 100 operations of 100 KB each are 10 MB: far below 128 MB per transaction
    let e = env().await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let (coord, txn) = controller(&mut c).await;
    let s = sender(&mut c, "q").await;
    for i in 0..100 {
        let out = c.send_message_in_txn(s, &sized(&format!("r{i}"), 100_000), &txn).await;
        assert_eq!(out, Some(t(&txn, DeliveryState::Accepted)), "send {i}");
    }
    c.discharge(coord, &txn, false).await.unwrap();
    assert_eq!(e.core("q", |c| c.main.len()), 100);
    assert_eq!(held(&e), 0);
}
