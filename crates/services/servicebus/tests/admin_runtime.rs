mod admin_common;

use admin_common::*;
use azemu_amqp::message::Message;
use azemu_servicebus::core::Incoming;
use azemu_servicebus::model::{Address, SubQueue};
use azemu_servicebus::state::now_ms;
use azemu_servicebus::store::{Msg, MsgState};

fn incoming(n: usize) -> Incoming {
    Incoming::new(Message::data(vec![b'x'; n]), n + 20)
}

fn counts(xml: &str) -> (String, String, String, String, String) {
    let g = |n: &str| tag(xml, n).unwrap_or_else(|| panic!("{n} missing in {xml}"));
    (
        g("d2p1:ActiveMessageCount"),
        g("d2p1:DeadLetterMessageCount"),
        g("d2p1:ScheduledMessageCount"),
        g("d2p1:TransferMessageCount"),
        g("d2p1:TransferDeadLetterMessageCount"),
    )
}

fn dead_letter(core: &mut azemu_servicebus::core::Core) {
    let store = core.store_mut(SubQueue::DeadLetter);
    let seq = store.alloc_seq();
    store.insert(Msg {
        seq,
        enq_seq: seq,
        state: MsgState::Ready,
        message: Message::data(b"dead".to_vec()).into(),
        enqueued_ms: now_ms(),
        scheduled_ms: None,
        ttl_ms: None,
        expires_ms: None,
        delivery_count: 0,
        lock: None,
        session_id: None,
        size: 40,
        dead_letter_source: None,
        deferred: false,
    });
}

#[tokio::test]
async fn queue_runtime_properties_follow_the_messages() {
    let a = admin().await;
    a.queue("q", "").await;
    let z = a.get("/q?enrich=false").await.text();
    assert_eq!(counts(&z), ("0".into(), "0".into(), "0".into(), "0".into(), "0".into()));
    assert_eq!(tag(&z, "SizeInBytes").unwrap(), "0");
    assert_eq!(tag(&z, "MessageCount").unwrap(), "0");
    let (size, expect) = {
        let mut st = a.shared.state.lock();
        let q = st.ns("default").queue_mut("q").unwrap();
        for _ in 0..3 {
            q.core.enqueue(incoming(100), now_ms()).unwrap();
        }
        let mut sch = incoming(10);
        sch.scheduled_ms = Some(now_ms() + 3_600_000);
        q.core.enqueue(sch, now_ms()).unwrap();
        dead_letter(&mut q.core);
        let c = q.core.counts();
        ((q.core.size_bytes()), (c.active, c.dead_letter, c.scheduled))
    };
    assert_eq!(expect, (3, 1, 1));
    let b = a.get("/q?enrich=false").await.text();
    assert_eq!(counts(&b), ("3".into(), "1".into(), "1".into(), "0".into(), "0".into()));
    assert_eq!(tag(&b, "MessageCount").unwrap(), "5");
    assert_eq!(tag(&b, "SizeInBytes").unwrap(), size.to_string());
    assert!(size > 0);
    // deleting the entity drops them
    a.delete("/q").await;
    a.queue("q", "").await;
    assert_eq!(tag(&a.get("/q").await.text(), "MessageCount").unwrap(), "0");
}

#[tokio::test]
async fn list_shows_runtime_data_per_queue() {
    let a = admin().await;
    a.queue("a", "").await;
    a.queue("b", "").await;
    {
        let mut st = a.shared.state.lock();
        st.ns("default").queue_mut("a").unwrap().core.enqueue(incoming(5), now_ms()).unwrap();
    }
    let b = a.get("/$Resources/queues?$skip=0&$top=100&api-version=2021-05").await.text();
    assert_eq!(tags(&b, "d2p1:ActiveMessageCount"), vec!["1", "0"]);
    assert_eq!(tags(&b, "AccessedAt").len(), 2);
}

#[tokio::test]
async fn topic_and_subscription_runtime_properties() {
    let a = admin().await;
    a.topic("t", "").await;
    for s in ["s1", "s2"] {
        a.sub("t", s, "").await;
    }
    a.rule("t", "s1", "corr", "<Filter xsi:type=\"CorrelationFilter\"><CorrelationId>x</CorrelationId></Filter>").await;
    a.delete("/t/subscriptions/s2/rules/$Default").await;
    {
        let mut st = a.shared.state.lock();
        let now = now_ms();
        st.send("default", &Address::Topic { name: "t".into() }, incoming(50), now).unwrap();
        let mut sch = incoming(5);
        sch.scheduled_ms = Some(now + 3_600_000);
        st.send("default", &Address::Topic { name: "t".into() }, sch, now).unwrap();
    }
    // s1 has $Default (true) and a correlation rule: the message is accepted; s2 has no rule: it gets nothing
    let t = a.get("/t?enrich=false&api-version=2021-05").await.text();
    assert_eq!(tag(&t, "SubscriptionCount").unwrap(), "2");
    assert_eq!(counts(&t), ("0".into(), "0".into(), "1".into(), "0".into(), "0".into()), "{t}");
    assert!(!t.contains("SqlFilterCount"), "2021-05 does not have the filter counts");
    assert_order(&t, &["AccessedAt", "SupportOrdering", "CountDetails", "SubscriptionCount", "AutoDeleteOnIdle"]);
    let t = a.get("/t?enrich=false&api-version=2024-05").await.text();
    assert_eq!(tag(&t, "SqlFilterCount").unwrap(), "1");
    assert_eq!(tag(&t, "CorrelationFilterCount").unwrap(), "1");
    assert_order(&t, &["SubscriptionCount", "SqlFilterCount", "CorrelationFilterCount", "AutoDeleteOnIdle"]);
    let size_topic = {
        let mut st = a.shared.state.lock();
        st.ns("default").topic("t").unwrap().size_bytes()
    };
    assert_eq!(tag(&t, "SizeInBytes").unwrap(), size_topic.to_string());
    assert!(size_topic > 0);

    let s1 = a.get("/t/subscriptions/s1?enrich=false").await.text();
    assert_eq!(tags(&s1, "d3p1:ActiveMessageCount"), vec!["1"]);
    assert_eq!(tag(&s1, "MessageCount").unwrap(), "1");
    let s2 = a.get("/t/subscriptions/s2?enrich=false").await.text();
    assert_eq!(tags(&s2, "d3p1:ActiveMessageCount"), vec!["0"]);
    // a dead-lettered message in s1
    {
        let mut st = a.shared.state.lock();
        dead_letter(&mut st.ns("default").topic_mut("t").unwrap().subs.get_mut("s1").unwrap().core);
    }
    let s1 = a.get("/t/subscriptions/s1?enrich=false").await.text();
    assert_eq!(tags(&s1, "d3p1:DeadLetterMessageCount"), vec!["1"]);
    assert_eq!(tag(&s1, "MessageCount").unwrap(), "2");
    // list of topics and subscriptions
    let lt = a.get("/$Resources/topics?$skip=0&$top=100").await.text();
    assert_eq!(tags(&lt, "SubscriptionCount"), vec!["2"]);
    let ls = a.get("/t/subscriptions?$skip=0&$top=100").await.text();
    assert_eq!(tags(&ls, "d3p1:ActiveMessageCount"), vec!["1", "0"]);
}
