//! Link-level handling: attach, detach, commands from service handles, and the send pump.

use std::collections::{HashMap, HashSet};

use tracing::debug;

use crate::api::*;
use crate::connection::Conn;
use crate::error::{condition, Error};
use crate::frame::{split_transfer, Frame, FrameBody};
use crate::performatives::*;
use crate::state::*;

impl Conn {
    pub(crate) fn on_attach(&mut self, channel: u16, a: Attach) -> Result<(), Error> {
        let Some(sess) = self.sessions.get_mut(&channel) else {
            return Err(Error::new(condition::CONNECTION_FRAMING_ERROR, format!("attach on unmapped channel {channel}")));
        };
        if sess.end_sent {
            return Ok(());
        }
        if sess.links.contains_key(&a.handle) {
            self.end_session_with_error(channel, Error::new(condition::SESSION_HANDLE_IN_USE, format!("handle {} is in use", a.handle)));
            return Ok(());
        }
        // handle-max was advertised in our `begin`
        if a.handle > HANDLE_MAX {
            self.end_session_with_error(
                channel,
                Error::new(condition::NOT_ALLOWED, format!("handle {} is above the handle-max of {HANDLE_MAX}", a.handle)),
            );
            return Ok(());
        }
        let too_many_links = self.link_index.len() >= self.cfg.max_links;
        let id = self.next_link_id;
        self.next_link_id += 1;
        let our_role = a.role.flip();
        let link = Link {
            id,
            handle: a.handle,
            name: a.name.clone(),
            role: our_role,
            state: LinkState::Pending,
            snd_settle_mode: a.snd_settle_mode,
            rcv_settle_mode: a.rcv_settle_mode,
            credit: 0,
            delivery_count: if our_role == Role::Receiver { a.initial_delivery_count.unwrap_or(0) } else { 0 },
            drain: false,
            needs_pull: false,
            max_message_size: self.cfg.max_message_size,
            partial: None,
            unsettled_out: HashMap::new(),
            unsettled_in: HashSet::new(),
            peer_attach: Some(a.clone()),
            notified: false,
        };
        sess.links.insert(a.handle, link);
        self.link_index.insert(id, (channel, a.handle));
        if too_many_links {
            let max = self.cfg.max_links;
            self.refuse_attach(
                channel,
                a.handle,
                Error::new(condition::RESOURCE_LIMIT_EXCEEDED, format!("The limit of {max} links per connection has been reached.")),
            );
            return Ok(());
        }
        let decision = self.handler.on_attach(AttachRequest {
            link: id,
            session: channel,
            handle: LinkHandle { link: id, tx: self.tx.clone() },
            attach: a.clone(),
        });
        match decision {
            AttachDecision::Accept(acc) => self.accept_attach(channel, a.handle, acc),
            AttachDecision::Refuse(err) => self.refuse_attach(channel, a.handle, err),
            AttachDecision::Defer => {}
        }
        Ok(())
    }

    fn accept_attach(&mut self, channel: u16, handle: u32, acc: AttachAccept) {
        let default_max = self.cfg.max_message_size;
        let credit_cfg = self.cfg.receiver_credit;
        let Some(sess) = self.sessions.get_mut(&channel) else { return };
        let Some(link) = sess.links.get_mut(&handle) else { return };
        if link.state != LinkState::Pending {
            return;
        }
        let Some(peer) = link.peer_attach.take() else { return };
        link.state = LinkState::Attached;
        link.max_message_size = acc.max_message_size.unwrap_or(default_max);
        if let Some(m) = acc.snd_settle_mode {
            link.snd_settle_mode = m;
        }
        if let Some(m) = acc.rcv_settle_mode {
            link.rcv_settle_mode = m;
        }
        let mut resp = Attach::new(peer.name.clone(), handle, link.role);
        resp.snd_settle_mode = link.snd_settle_mode;
        resp.rcv_settle_mode = link.rcv_settle_mode;
        resp.source = acc.source.or(peer.source.clone());
        resp.target = acc.target.or(peer.target.clone());
        resp.initial_delivery_count = if link.role == Role::Sender { Some(link.delivery_count) } else { None };
        resp.max_message_size = Some(link.max_message_size);
        resp.offered_capabilities = acc.offered_capabilities;
        resp.properties = acc.properties;
        let mut flow = None;
        if link.role == Role::Receiver {
            link.credit = credit_cfg;
            flow = Some(Flow {
                handle: Some(handle),
                delivery_count: Some(link.delivery_count),
                link_credit: Some(link.credit),
                ..Self::session_flow(sess)
            });
        }
        self.send(channel, Performative::Attach(resp));
        if let Some(f) = flow {
            self.send(channel, Performative::Flow(f));
        }
    }

    /// Refuse an attach the way the spec describes: an attach with null termini, then a detach
    /// with the error. The link record stays until the peer's detach arrives.
    fn refuse_attach(&mut self, channel: u16, handle: u32, err: Error) {
        let Some(sess) = self.sessions.get_mut(&channel) else { return };
        let Some(link) = sess.links.get_mut(&handle) else { return };
        let Some(peer) = link.peer_attach.take() else { return };
        link.state = LinkState::DetachSent;
        link.notified = true; // the service never saw it attached
        let mut resp = Attach::new(peer.name, handle, link.role);
        resp.snd_settle_mode = peer.snd_settle_mode;
        resp.rcv_settle_mode = peer.rcv_settle_mode;
        resp.initial_delivery_count = if link.role == Role::Sender { Some(0) } else { None };
        self.send(channel, Performative::Attach(resp));
        self.send(channel, Performative::Detach(Detach { handle, closed: true, error: Some(err) }));
    }

    /// Notify the service that a link ended (once).
    pub(crate) fn notify_detach(&mut self, channel: u16, handle: u32, reason: DetachReason) {
        let Some(sess) = self.sessions.get_mut(&channel) else { return };
        let Some(link) = sess.links.get_mut(&handle) else { return };
        if link.notified {
            return;
        }
        link.notified = true;
        let id = link.id;
        self.handler.on_detach(id, reason);
    }

    /// We detach a link (service request or protocol error).
    pub(crate) fn detach_link(&mut self, channel: u16, handle: u32, error: Option<Error>) {
        let Some(sess) = self.sessions.get_mut(&channel) else { return };
        let Some(link) = sess.links.get_mut(&handle) else { return };
        if link.state == LinkState::DetachSent {
            return;
        }
        let was_pending = link.state == LinkState::Pending;
        link.state = LinkState::DetachSent;
        link.partial = None;
        link.credit = 0;
        self.notify_detach(channel, handle, DetachReason::Local);
        if was_pending {
            // the peer never got our attach: answer it first
            if let Some(sess) = self.sessions.get_mut(&channel) {
                if let Some(link) = sess.links.get_mut(&handle) {
                    if let Some(peer) = link.peer_attach.take() {
                        let mut resp = Attach::new(peer.name, handle, link.role);
                        resp.initial_delivery_count = if link.role == Role::Sender { Some(0) } else { None };
                        self.send(channel, Performative::Attach(resp));
                    }
                }
            }
        }
        self.send(channel, Performative::Detach(Detach { handle, closed: true, error }));
    }

    pub(crate) fn on_detach(&mut self, channel: u16, d: Detach) -> Result<(), Error> {
        let Some(sess) = self.sessions.get_mut(&channel) else {
            return Err(Error::new(condition::CONNECTION_FRAMING_ERROR, format!("detach on unmapped channel {channel}")));
        };
        if sess.end_sent {
            return Ok(());
        }
        let Some(link) = sess.links.get(&d.handle) else {
            self.end_session_with_error(
                channel,
                Error::new(condition::SESSION_UNATTACHED_HANDLE, format!("detach for unknown handle {}", d.handle)),
            );
            return Ok(());
        };
        let state = link.state;
        let id = link.id;
        if state != LinkState::DetachSent {
            if let Some(e) = &d.error {
                debug!("peer detached link {} with error: {e}", link.name);
            }
            self.notify_detach(channel, d.handle, DetachReason::Peer { closed: d.closed, error: d.error.clone() });
            self.send(channel, Performative::Detach(Detach { handle: d.handle, closed: d.closed, error: None }));
        }
        if let Some(sess) = self.sessions.get_mut(&channel) {
            sess.links.remove(&d.handle);
        }
        self.link_index.remove(&id);
        Ok(())
    }

    pub(crate) fn link_command(&mut self, cmd: Command) {
        match cmd {
            Command::Close(_) => {}
            Command::Wake(id) => {
                if let Some((ch, h)) = self.link_index.get(&id).copied() {
                    if let Some(link) = self.sessions.get_mut(&ch).and_then(|s| s.links.get_mut(&h)) {
                        if link.role == Role::Sender && link.state == LinkState::Attached {
                            link.needs_pull = true;
                            self.pump_needed = true;
                        }
                    }
                }
            }
            Command::Detach(id, err) => {
                if let Some((ch, h)) = self.link_index.get(&id).copied() {
                    self.detach_link(ch, h, err);
                }
            }
            Command::Settle { link, delivery_id, state } => {
                if let Some((ch, h)) = self.link_index.get(&link).copied() {
                    let attached =
                        self.sessions.get(&ch).and_then(|s| s.links.get(&h)).map(|l| l.state == LinkState::Attached).unwrap_or(false);
                    if attached {
                        self.settle_incoming(ch, h, delivery_id, state);
                    }
                }
            }
            Command::CompleteAttach(id, result) => {
                if let Some((ch, h)) = self.link_index.get(&id).copied() {
                    match result {
                        Ok(acc) => {
                            self.accept_attach(ch, h, acc);
                            self.pump_needed = true;
                        }
                        Err(e) => {
                            // The service saw this attach (it deferred it), so it hears about the
                            // end of the link too.
                            self.notify_detach(ch, h, DetachReason::Local);
                            self.refuse_attach(ch, h, e);
                        }
                    }
                }
            }
        }
    }

    /// Hand every sender link that has credit and window as many deliveries as the service has.
    pub(crate) fn pump_links(&mut self) {
        let channels: Vec<u16> = self.sessions.keys().copied().collect();
        for ch in channels {
            let handles: Vec<u32> = match self.sessions.get(&ch) {
                Some(s) if !s.end_sent => s
                    .links
                    .values()
                    .filter(|l| l.role == Role::Sender && l.state == LinkState::Attached && (l.needs_pull || l.drain))
                    .map(|l| l.handle)
                    .collect(),
                _ => continue,
            };
            for h in handles {
                self.pump_link(ch, h);
            }
        }
    }

    fn pump_link(&mut self, ch: u16, h: u32) {
        loop {
            if self.output_backed_up() {
                // try again when the socket has drained
                self.pump_needed = true;
                return;
            }
            let peer_max = self.peer_max_frame;
            let Some(sess) = self.sessions.get_mut(&ch) else { return };
            let Some(link) = sess.links.get_mut(&h) else { return };
            if link.credit == 0 {
                link.needs_pull = false;
                break;
            }
            if sess.remote_incoming_window == 0 {
                // wait for the peer's flow
                return;
            }
            let Some(out) = self.handler.pull(link.id) else {
                link.needs_pull = false;
                break;
            };
            let delivery_id = sess.next_delivery_id;
            sess.next_delivery_id = sess.next_delivery_id.wrapping_add(1);
            let first = Transfer {
                handle: h,
                delivery_id: Some(delivery_id),
                delivery_tag: Some(out.tag.clone()),
                message_format: Some(out.message_format),
                settled: Some(out.settled),
                ..Default::default()
            };
            let frames = split_transfer(ch, first, &out.payload, peer_max);
            let n = frames.len() as u32;
            sess.next_outgoing_id = sess.next_outgoing_id.wrapping_add(n);
            sess.remote_incoming_window = sess.remote_incoming_window.saturating_sub(n);
            link.credit -= 1;
            link.delivery_count = link.delivery_count.wrapping_add(1);
            if !out.settled {
                link.unsettled_out.insert(delivery_id, out.tag);
            }
            for f in frames {
                self.send_frame_owned(f);
            }
        }
        // drain: the credit was used up or there is nothing left; tell the peer
        let Some(sess) = self.sessions.get_mut(&ch) else { return };
        let Some(link) = sess.links.get_mut(&h) else { return };
        if link.drain && link.state == LinkState::Attached {
            link.delivery_count = link.delivery_count.wrapping_add(link.credit);
            link.credit = 0;
            link.drain = false;
            let flow = Flow {
                handle: Some(h),
                delivery_count: Some(link.delivery_count),
                link_credit: Some(0),
                drain: true,
                ..Self::session_flow(sess)
            };
            self.send(ch, Performative::Flow(flow));
        }
    }

    fn send_frame_owned(&mut self, f: Frame) {
        debug_assert!(matches!(f.body, FrameBody::Amqp(..)));
        self.send_frame(&f);
    }
}
