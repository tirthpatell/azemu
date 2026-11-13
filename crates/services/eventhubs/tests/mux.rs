//! The AMQP multiplexer: one port, Event Hubs and "the other" AMQP service.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use azemu_amqp::api::*;
use azemu_amqp::performatives::Role;
use azemu_amqp::terminus::{DeliveryState, TargetKind};
use azemu_amqp::Error;
use azemu_core::testing::TestServer;
use azemu_eventhubs::state::Shared;
use common::*;

/// A stand-in for Service Bus: accepts `$cbs`, refuses everything else with a recognisable error.
struct StubFactory {
    connections: Arc<AtomicUsize>,
    opens: Arc<std::sync::Mutex<Vec<String>>>,
}

struct StubConn {
    opens: Arc<std::sync::Mutex<Vec<String>>>,
}

impl HandlerFactory for StubFactory {
    fn new_connection(&self, _info: &ConnectionInfo, _conn: ConnHandle) -> Box<dyn ConnectionHandler> {
        self.connections.fetch_add(1, Ordering::SeqCst);
        Box::new(StubConn { opens: self.opens.clone() })
    }
}

impl ConnectionHandler for StubConn {
    fn on_open(&mut self, open: &azemu_amqp::performatives::Open) -> Result<(), Error> {
        self.opens.lock().unwrap().push(open.hostname.clone().unwrap_or_default());
        Ok(())
    }
    fn on_attach(&mut self, req: AttachRequest) -> AttachDecision {
        let raw = match req.local_role() {
            Role::Receiver => req.attach.target.as_ref().and_then(TargetKind::address).unwrap_or_default().to_string(),
            Role::Sender => req.attach.source.as_ref().and_then(|s| s.address.clone()).unwrap_or_default(),
        };
        if raw == "$cbs" {
            AttachDecision::Accept(AttachAccept::default())
        } else {
            AttachDecision::Refuse(Error::new("amqp:not-found", format!("stub-other: {raw}")))
        }
    }
    fn on_transfer(&mut self, _link: LinkId, _d: IncomingDelivery) -> TransferAction {
        TransferAction::Settle(DeliveryState::Accepted)
    }
    fn pull(&mut self, _link: LinkId) -> Option<Outgoing> {
        None
    }
    fn on_detach(&mut self, _link: LinkId, _reason: DetachReason) {}
}

struct Setup {
    srv: TestServer,
    shared: Arc<Shared>,
    stub_connections: Arc<AtomicUsize>,
    stub_opens: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn setup() -> Setup {
    let shared = Shared::new(true, None);
    let (svc, amqp) = azemu_eventhubs::pair_with(shared.clone());
    let connections = Arc::new(AtomicUsize::new(0));
    let opens = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mux = amqp.mux(Arc::new(StubFactory { connections: connections.clone(), opens: opens.clone() }));
    let srv = TestServer::start_with(vec![svc], vec![mux]).await;
    Setup { srv, shared, stub_connections: connections, stub_opens: opens }
}

fn stub_error(e: Option<Error>) -> String {
    e.expect("error").description.unwrap_or_default()
}

#[tokio::test]
async fn an_eventhub_host_name_selects_event_hubs() {
    let s = setup().await;
    for host in ["myns.eventhub.localhost", "myns.eventhubs.core.windows.net", "eventhub.localhost"] {
        let mut c = connect(s.srv.port(), host).await;
        let l = attach_sender(&mut c, "h/Partitions/0").await.unwrap_or_else(|e| panic!("{host}: {e:?}"));
        send_one(&mut c, l, &event("x")).await;
        // and a plain hub address works too (ambiguous by itself, decided by the host)
        attach_sender(&mut c, "h").await.unwrap();
    }
    assert_eq!(s.stub_connections.load(Ordering::SeqCst), 0, "the other service never saw a connection");
}

#[tokio::test]
async fn any_other_host_name_goes_to_the_other_service() {
    let s = setup().await;
    for host in ["myns.servicebus.localhost", "localhost", "127.0.0.1", "myns.servicebus.windows.net"] {
        let mut c = connect(s.srv.port(), host).await;
        let e = attach_sender(&mut c, "queue1").await.unwrap_err();
        assert_eq!(stub_error(e), "stub-other: queue1", "{host}");
    }
    assert_eq!(s.stub_connections.load(Ordering::SeqCst), 4);
    assert!(s.stub_opens.lock().unwrap().contains(&"myns.servicebus.windows.net".to_string()), "the open frame reaches the other handler");
    assert!(s.shared.state.lock().namespaces.is_empty(), "no event hub was created");
}

#[tokio::test]
async fn a_namespace_that_has_an_event_hub_is_an_event_hubs_namespace() {
    let s = setup().await;
    // created through the admin API, the way a test suite would
    let url = format!("{}/_azemu/eventhubs/team/hubs/orders", s.srv.gateway_url());
    let r = s.srv.send("PUT", &url, &[], r#"{"partitionCount":2}"#).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let mut c = connect(s.srv.port(), "team.servicebus.localhost").await;
    let l = attach_sender(&mut c, "orders/Partitions/1").await.unwrap();
    send_one(&mut c, l, &event("x")).await;
    // a plain address is decided by the namespace as well
    attach_sender(&mut c, "orders").await.unwrap();
    // another namespace is still Service Bus's
    let mut d = connect(s.srv.port(), "elsewhere.servicebus.localhost").await;
    assert_eq!(stub_error(attach_sender(&mut d, "orders").await.unwrap_err()), "stub-other: orders");
}

#[tokio::test]
async fn an_attach_only_event_hubs_has_moves_that_link_to_event_hubs() {
    let s = setup().await;
    let mut c = connect(s.srv.port(), "myns.servicebus.localhost").await;
    // `$cbs` and an unknown entity belong to the other service ...
    assert!(attach_receiver(&mut c, "$cbs", 10).await.is_ok());
    assert_eq!(stub_error(attach_sender(&mut c, "plain").await.unwrap_err()), "stub-other: plain");
    // ... but these addresses exist only on Event Hubs
    let sender = attach_sender(&mut c, "amqps://myns.servicebus.localhost:1/h/Partitions/0").await.unwrap();
    send_one(&mut c, sender, &event("via fallback")).await;
    let r = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
    assert_eq!(take(&mut c, r, 1).await, vec!["via fallback"]);
    // the namespace is now known: new connections with the same host go to Event Hubs
    let mut d = connect(s.srv.port(), "myns.servicebus.localhost").await;
    attach_sender(&mut d, "h").await.unwrap();
    // the bare `$management` node is Event Hubs' too
    let links = mgmt_links(&mut c).await;
    let (status, _, body) = mgmt(&mut c, links, vec![("name", "h"), ("operation", "READ"), ("type", "com.microsoft:eventhub")]).await;
    assert_eq!(status, 200);
    assert!(body.map_get("partition_ids").is_some());
}

#[tokio::test]
async fn a_config_file_namespace_is_event_hubs_for_every_host_that_names_it() {
    let dir = std::env::temp_dir().join(format!("azemu-eh-mux-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("Config.json");
    std::fs::write(&path, r#"{"UserConfig":{"NamespaceConfig":[{"Name":"emulator","Entities":[{"Name":"eh1","PartitionCount":2}]}]}}"#)
        .unwrap();
    let shared = Shared::new(true, Some(path));
    shared.load_config();
    let (svc, amqp) = azemu_eventhubs::pair_with(shared);
    let mux = amqp.mux(Arc::new(StubFactory { connections: Arc::new(AtomicUsize::new(0)), opens: Default::default() }));
    let srv = TestServer::start_with(vec![svc], vec![mux]).await;
    for host in ["emulator.servicebus.localhost", "localhost"] {
        let mut c = connect(srv.port(), host).await;
        attach_sender(&mut c, "eh1/Partitions/1").await.unwrap_or_else(|e| panic!("{host}: {e:?}"));
    }
}

#[tokio::test]
async fn event_hubs_and_service_bus_share_one_port() {
    let shared = Shared::new(true, None);
    let (eh_svc, eh_amqp) = azemu_eventhubs::pair_with(shared);
    // the test is about sharing a port, not about creating queues: queues are made on first use here
    let (sb_svc, _sb_amqp) = azemu_servicebus::pair_with(azemu_servicebus::state::Shared::new(true, None));
    let sb_factory = Arc::new(azemu_servicebus::amqp_conn::SbFactory { shared: sb_svc.shared.clone() });
    let mux = eh_amqp.mux(sb_factory);
    let srv = TestServer::start_with(vec![eh_svc, sb_svc], vec![mux]).await;

    // Service Bus: a queue, send and receive
    let mut sb = connect(srv.port(), "default.servicebus.localhost").await;
    let q = attach_sender(&mut sb, "jobs").await.unwrap();
    send_one(&mut sb, q, &event("job-1")).await;
    let r = attach_receiver(&mut sb, "jobs", 5).await.unwrap();
    assert_eq!(body_of(&sb.recv(r).await), "job-1");

    // Event Hubs: a hub of the same name, on the same port
    let mut eh = connect(srv.port(), "default.eventhub.localhost").await;
    let l = attach_sender(&mut eh, "jobs/Partitions/0").await.unwrap();
    send_one(&mut eh, l, &event("event-1")).await;
    let r = consume(&mut eh, "jobs", "$Default", 0, &from_start()).await.unwrap();
    assert_eq!(take(&mut eh, r, 1).await, vec!["event-1"]);

    // one connection with a Service Bus host name uses both: the Event Hubs address moves over
    let mut both = connect(srv.port(), "mixed.servicebus.localhost").await;
    let q = attach_sender(&mut both, "queue-x").await.unwrap();
    send_one(&mut both, q, &event("to-queue")).await;
    let h = attach_sender(&mut both, "hub-x/Partitions/0").await.unwrap();
    send_one(&mut both, h, &event("to-hub")).await;
    let rq = attach_receiver(&mut both, "queue-x", 5).await.unwrap();
    assert_eq!(body_of(&both.recv(rq).await), "to-queue");
    let rh = consume(&mut both, "hub-x", "$Default", 0, &from_start()).await.unwrap();
    assert_eq!(take(&mut both, rh, 1).await, vec!["to-hub"]);

    // HTTP: both services answer on the gateway
    let r = srv.send("GET", &format!("{}/_azemu/eventhubs/namespaces", srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200);
    assert!(r.text().contains("mixed"), "{}", r.text());
}
