//! Two AMQP services on one port.
//!
//! Core hands every connection that starts with `AMQP` to one raw protocol. Service Bus and
//! Event Hubs both speak AMQP 1.0 and, on real Azure, both live under `*.servicebus.windows.net`:
//! a namespace is either a Service Bus namespace or an Event Hubs namespace. [`AmqpMux`] is the
//! one raw protocol that serves both. It owns the connection through the AMQP handshake; the
//! `open` frame names the host the client dialled, and the choice is made then:
//!
//! 1. an event hub exists in the namespace (created through the admin API, a config file, or by
//!    an earlier client), or the config file declares the namespace: Event Hubs;
//! 2. the host name has the label `eventhub` or `eventhubs` (`ns.eventhub.localhost`,
//!    `eventhub.localhost`, `ns.eventhubs.core.windows.net`): Event Hubs;
//! 3. otherwise: the other service (Service Bus).
//!
//! A connection that the rule sent to the other service is moved link by link when a client
//! attaches to an address only Event Hubs has (`{hub}/Partitions/{n}`,
//! `{hub}/ConsumerGroups/{cg}/Partitions/{n}`, the bare `$management` node): that link and its
//! requests go to Event Hubs, and the event hub it creates makes the namespace an Event Hubs
//! namespace for every later connection. So a client that dials `sb://myns.servicebus.localhost`
//! and sends or reads events works with no setup; only a plain hub address (`{hub}`, ambiguous
//! by itself) and `$cbs` stay with the other service on such a connection, which is harmless
//! because both accept every token.
//!
//! The mux does not know Service Bus: it takes a [`HandlerFactory`] for "the other" service.

use std::collections::HashMap;
use std::sync::Arc;

use azemu_amqp::api::*;
use azemu_amqp::performatives::{Open, Role};
use azemu_amqp::terminus::DeliveryState;
use azemu_amqp::{condition, Error as AmqpError};
use azemu_core::{async_trait, RawConn, RawProtocol};

use crate::address::Address;
use crate::amqp_conn::{hostname_says_eventhubs, namespace_from_hostname};
use crate::state::Shared;

/// The raw protocol to register instead of the Service Bus and Event Hubs ones.
pub struct AmqpMux {
    shared: Arc<Shared>,
    eventhubs: Arc<dyn HandlerFactory>,
    other: Arc<dyn HandlerFactory>,
    cfg: Arc<ServerConfig>,
}

impl AmqpMux {
    pub fn new(shared: Arc<Shared>, eventhubs: Arc<dyn HandlerFactory>, other: Arc<dyn HandlerFactory>, cfg: Arc<ServerConfig>) -> AmqpMux {
        AmqpMux { shared, eventhubs, other, cfg }
    }
}

#[async_trait]
impl RawProtocol for AmqpMux {
    fn sniff(&self, first_bytes: &[u8]) -> bool {
        first_bytes.starts_with(b"AMQP")
    }

    async fn serve(&self, conn: RawConn) {
        let info = ConnectionInfo { peer: conn.peer, local: conn.local, tls: conn.tls, server_name: conn.server_name.clone() };
        let factory: SharedFactory =
            Arc::new(MuxFactory { shared: self.shared.clone(), eventhubs: self.eventhubs.clone(), other: self.other.clone() });
        azemu_amqp::serve_connection(conn.io, info, factory, self.cfg.clone()).await;
    }
}

struct MuxFactory {
    shared: Arc<Shared>,
    eventhubs: Arc<dyn HandlerFactory>,
    other: Arc<dyn HandlerFactory>,
}

impl HandlerFactory for MuxFactory {
    fn new_connection(&self, info: &ConnectionInfo, conn: ConnHandle) -> Box<dyn ConnectionHandler> {
        Box::new(MuxHandler {
            info: info.clone(),
            conn,
            shared: self.shared.clone(),
            eventhubs_factory: self.eventhubs.clone(),
            other_factory: self.other.clone(),
            open: None,
            primary: Side::Other,
            eventhubs: None,
            other: None,
            route: HashMap::new(),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Side {
    EventHubs,
    Other,
}

struct MuxHandler {
    info: ConnectionInfo,
    conn: ConnHandle,
    shared: Arc<Shared>,
    eventhubs_factory: Arc<dyn HandlerFactory>,
    other_factory: Arc<dyn HandlerFactory>,
    open: Option<Open>,
    primary: Side,
    eventhubs: Option<Box<dyn ConnectionHandler>>,
    other: Option<Box<dyn ConnectionHandler>>,
    route: HashMap<LinkId, Side>,
}

/// The decision the `open` frame allows (rules 1 and 2 of the module docs).
pub fn side_for_host(shared: &Shared, host: Option<&str>) -> bool {
    hostname_says_eventhubs(host) || shared.knows_namespace(&namespace_from_hostname(host))
}

impl MuxHandler {
    /// The handler of a side, created on first use and told about the `open` frame.
    fn handler(&mut self, side: Side) -> Result<&mut Box<dyn ConnectionHandler>, AmqpError> {
        let (slot, factory) = match side {
            Side::EventHubs => (&mut self.eventhubs, &self.eventhubs_factory),
            Side::Other => (&mut self.other, &self.other_factory),
        };
        if slot.is_none() {
            let mut h = factory.new_connection(&self.info, self.conn.clone());
            if let Some(open) = &self.open {
                h.on_open(open)?;
            }
            *slot = Some(h);
        }
        slot.as_mut().ok_or_else(|| AmqpError::new(condition::INTERNAL_ERROR, "no handler"))
    }
}

impl ConnectionHandler for MuxHandler {
    fn on_open(&mut self, open: &Open) -> Result<(), AmqpError> {
        self.primary = if side_for_host(&self.shared, open.hostname.as_deref()) { Side::EventHubs } else { Side::Other };
        self.open = Some(open.clone());
        let p = self.primary;
        self.handler(p)?;
        Ok(())
    }

    fn on_attach(&mut self, req: AttachRequest) -> AttachDecision {
        let raw = match req.local_role() {
            Role::Receiver => req.attach.target.as_ref().and_then(|t| t.address()).unwrap_or_default().to_string(),
            Role::Sender => req.attach.source.as_ref().and_then(|s| s.address.clone()).unwrap_or_default(),
        };
        let only_eventhubs = Address::parse(&raw).map(|a| a.is_unmistakably_eventhubs()).unwrap_or(false);
        let side = if only_eventhubs { Side::EventHubs } else { self.primary };
        let link = req.link;
        self.route.insert(link, side);
        let decision = match self.handler(side) {
            Ok(h) => h.on_attach(req),
            Err(e) => AttachDecision::Refuse(e),
        };
        if matches!(decision, AttachDecision::Refuse(_)) {
            // the library never reports a refused link as detached
            self.route.remove(&link);
        }
        decision
    }

    fn on_transfer(&mut self, link: LinkId, delivery: IncomingDelivery) -> TransferAction {
        match self.route.get(&link).copied() {
            Some(side) => match self.handler(side) {
                Ok(h) => h.on_transfer(link, delivery),
                Err(e) => TransferAction::Settle(DeliveryState::rejected(e)),
            },
            None => TransferAction::Settle(DeliveryState::rejected(AmqpError::new(condition::ILLEGAL_STATE, "unexpected transfer"))),
        }
    }

    fn on_disposition(&mut self, link: LinkId, tag: &[u8], state: Option<DeliveryState>, settled: bool) -> Option<DeliveryState> {
        let side = self.route.get(&link).copied()?;
        self.handler(side).ok()?.on_disposition(link, tag, state, settled)
    }

    fn on_flow(&mut self, link: LinkId, credit: u32, drain: bool) {
        if let Some(side) = self.route.get(&link).copied() {
            if let Ok(h) = self.handler(side) {
                h.on_flow(link, credit, drain);
            }
        }
    }

    fn pull(&mut self, link: LinkId) -> Option<Outgoing> {
        let side = self.route.get(&link).copied()?;
        self.handler(side).ok()?.pull(link)
    }

    fn on_detach(&mut self, link: LinkId, reason: DetachReason) {
        if let Some(side) = self.route.remove(&link) {
            if let Ok(h) = self.handler(side) {
                h.on_detach(link, reason);
            }
        }
    }

    fn on_close(&mut self) {
        if let Some(h) = self.eventhubs.as_mut() {
            h.on_close();
        }
        if let Some(h) = self.other.as_mut() {
            h.on_close();
        }
    }
}
