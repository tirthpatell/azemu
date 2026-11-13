//! A receiver that asks for "the next available session" waits for one. These tests check what
//! happens when it gives up (detach, end of session, connection dropped) and what it gets when
//! the wait succeeds.

mod common;
use std::time::Duration;

use azemu_amqp::client::{LinkState, TestClient};
use azemu_amqp::performatives::{Attach, Detach, Performative, Role};
use azemu_amqp::terminus::{Source, TargetKind};
use azemu_amqp::types::Value;
use common::*;

/// Send an attach for "the next available session" and do not wait for the answer.
async fn start_next_session_attach(c: &mut TestClient, address: &str, handle: u32, timeout_ms: u32) {
    let mut s = Source::with_address(address);
    s.filter = Some(Value::Map(vec![(Value::sym("com.microsoft:session-filter"), Value::Null)]));
    let mut a = Attach::new(format!("wait-{handle}"), handle, Role::Receiver);
    a.source = Some(s);
    a.target = Some(TargetKind::Target(Default::default()));
    a.properties = Some(Value::Map(vec![(Value::sym("com.microsoft:timeout"), Value::Uint(timeout_ms))]));
    let mut l = LinkState::default();
    l.name = a.name.clone();
    l.role = Some(Role::Receiver);
    c.links.insert(handle, l);
    c.send(0, Performative::Attach(a)).await;
}

fn session_locked(e: &Env, queue: &str, sid: &str) -> bool {
    let now = azemu_servicebus::state::now_ms();
    e.core(queue, |c| c.sessions.get(sid).and_then(|s| s.lock.as_ref()).map(|l| l.until_ms > now).unwrap_or(false))
}

fn waiting_receivers(e: &Env, queue: &str) -> bool {
    e.core(queue, |c| c.has_waiters())
}

#[tokio::test]
async fn a_detached_next_session_wait_never_locks_a_session() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    start_next_session_attach(&mut c, "sq", 3, 60_000).await;
    c.pump_for(Duration::from_millis(200)).await;
    // the client gives up
    c.send(0, Performative::Detach(Detach { handle: 3, closed: true, error: None })).await;
    c.wait_until("detach echo", |c| c.links[&3].detached.is_some()).await;
    // a session appears later: nobody is waiting, so nobody may lock it
    e.inject("sq", msg_session("m", "A"));
    c.pump_for(Duration::from_millis(400)).await;
    assert!(!session_locked(&e, "sq", "A"), "a gone receiver locked the session");
    // and a real receiver gets it at once
    let h = session_receiver_next(&mut c).await;
    assert_eq!(body(&c.recv(h).await.message()), "m");
}

async fn session_receiver_next(c: &mut TestClient) -> u32 {
    use azemu_amqp::performatives::{RcvSettleMode, SndSettleMode};
    let mut s = Source::with_address("sq");
    s.filter = Some(Value::Map(vec![(Value::sym("com.microsoft:session-filter"), Value::Null)]));
    c.attach_receiver_source(s, SndSettleMode::Unsettled, RcvSettleMode::Second, 10).await.expect("the session is free")
}

#[tokio::test]
async fn an_ended_session_next_session_wait_never_locks_a_session() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    start_next_session_attach(&mut c, "sq", 3, 60_000).await;
    c.pump_for(Duration::from_millis(200)).await;
    assert!(c.end_session().await.is_none());
    e.inject("sq", msg_session("m", "A"));
    c.pump_for(Duration::from_millis(400)).await;
    assert!(!session_locked(&e, "sq", "A"), "a gone receiver locked the session");
    assert!(!waiting_receivers(&e, "sq"));
}

#[tokio::test]
async fn a_dropped_connection_next_session_wait_never_locks_a_session() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    start_next_session_attach(&mut c, "sq", 3, 60_000).await;
    c.pump_for(Duration::from_millis(200)).await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(100)).await;
    e.inject("sq", msg_session("m", "A"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!session_locked(&e, "sq", "A"), "a gone receiver locked the session");
}

#[tokio::test]
async fn a_closed_connection_next_session_wait_never_locks_a_session() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    start_next_session_attach(&mut c, "sq", 3, 60_000).await;
    c.pump_for(Duration::from_millis(200)).await;
    assert!(c.close().await.is_none());
    e.inject("sq", msg_session("m", "A"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!session_locked(&e, "sq", "A"), "a gone receiver locked the session");
}

#[tokio::test]
async fn a_next_session_wait_that_timed_out_is_over() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    start_next_session_attach(&mut c, "sq", 3, 300).await;
    c.wait_until("timeout detach", |c| c.links[&3].detached.is_some()).await;
    assert_eq!(c.links[&3].detached.clone().flatten().map(|e| e.condition), Some("com.microsoft:timeout".to_string()));
    e.inject("sq", msg_session("m", "A"));
    c.pump_for(Duration::from_millis(300)).await;
    assert!(!session_locked(&e, "sq", "A"));
}

#[tokio::test]
async fn a_receiver_that_waited_for_a_session_gets_only_that_session() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    start_next_session_attach(&mut c, "sq", 3, 60_000).await;
    c.pump_for(Duration::from_millis(100)).await;
    c.send(
        0,
        Performative::Flow(azemu_amqp::performatives::Flow {
            next_incoming_id: Some(0),
            incoming_window: 100,
            next_outgoing_id: 0,
            outgoing_window: 100,
            handle: Some(3),
            delivery_count: Some(0),
            link_credit: Some(10),
            ..Default::default()
        }),
    )
    .await;
    e.inject("sq", msg_session("a1", "A"));
    e.inject("sq", msg_session("b1", "B"));
    e.inject("sq", msg_session("a2", "A"));
    c.wait_until("attach", |c| c.links[&3].attach.is_some()).await;
    let granted = c.links[&3]
        .attach
        .as_ref()
        .unwrap()
        .source
        .as_ref()
        .unwrap()
        .filter_get("com.microsoft:session-filter")
        .and_then(|v| v.undescribed().as_str().map(str::to_string));
    assert_eq!(granted.as_deref(), Some("A"));
    assert_eq!(body(&c.recv(3).await.message()), "a1");
    assert_eq!(body(&c.recv(3).await.message()), "a2");
    no_delivery(&mut c, 3, 300).await;
}

#[tokio::test]
async fn closing_a_receiver_that_waited_for_a_session_frees_the_session() {
    let e = env().await;
    e.queue("sq", |s| s.requires_session = true);
    let mut c = e.client().await;
    start_next_session_attach(&mut c, "sq", 3, 60_000).await;
    c.pump_for(Duration::from_millis(100)).await;
    e.inject("sq", msg_session("a1", "A"));
    c.wait_until("attach", |c| c.links[&3].attach.is_some()).await;
    assert!(session_locked(&e, "sq", "A"));
    c.send(0, Performative::Detach(Detach { handle: 3, closed: true, error: None })).await;
    c.wait_until("detach echo", |c| c.links[&3].detached.is_some()).await;
    assert!(!session_locked(&e, "sq", "A"), "the session stayed locked after its receiver closed");
}
