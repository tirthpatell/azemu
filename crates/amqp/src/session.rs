//! Session-level frames: begin, end, flow, transfer (receiving), disposition.

use std::collections::HashMap;

use bytes::Bytes;
use tracing::debug;

use crate::api::*;
use crate::connection::Conn;
use crate::error::{condition, Error};
use crate::performatives::*;
use crate::state::*;
use crate::terminus::DeliveryState;

/// A half-received message this big makes the connection add up all of them.
const LARGE_PARTIAL: usize = 1024 * 1024;

/// Transfers we may still send, from the peer's `flow`: its `incoming-window`, less the transfers we
/// sent that it had not counted yet when it wrote the flow (`next_outgoing_id - next-incoming-id`,
/// a wrapping subtraction: both are serial numbers). The window itself is a plain `uint`
/// (0 to 2^32-1; a client that wants no limit sends 2^32-1), so it is never read as a signed number.
pub(crate) fn remote_window(peer_next_incoming_id: u32, peer_incoming_window: u32, next_outgoing_id: u32) -> u32 {
    let in_flight = next_outgoing_id.wrapping_sub(peer_next_incoming_id);
    // A peer that claims to have seen more than we sent is wrong; do not let that open the window.
    if in_flight > u32::MAX / 2 {
        return peer_incoming_window;
    }
    peer_incoming_window.saturating_sub(in_flight)
}

/// Link credit we may still use, from the peer's `flow` on a link we send on: its `link-credit`, less
/// the deliveries we sent that its `delivery-count` did not include yet. Serial arithmetic on the
/// counts; the credit is a plain `uint`.
pub(crate) fn remote_credit(peer_delivery_count: u32, peer_link_credit: u32, delivery_count: u32) -> u32 {
    let ahead = delivery_count.wrapping_sub(peer_delivery_count);
    if ahead > u32::MAX / 2 {
        return peer_link_credit;
    }
    peer_link_credit.saturating_sub(ahead)
}

impl Conn {
    pub(crate) fn on_begin(&mut self, channel: u16, b: Begin) -> Result<(), Error> {
        if b.remote_channel.is_some() {
            return Err(Error::new(condition::NOT_ALLOWED, "begin replies to a session this server never started"));
        }
        if channel > self.cfg.channel_max {
            return Err(Error::new(condition::NOT_ALLOWED, format!("channel {channel} exceeds channel-max")));
        }
        if self.sessions.contains_key(&channel) {
            return Err(Error::new(condition::NOT_ALLOWED, format!("channel {channel} already has a session")));
        }
        if self.sessions.len() >= self.cfg.max_sessions {
            return Err(Error::new(
                condition::RESOURCE_LIMIT_EXCEEDED,
                format!("The limit of {} sessions per connection has been reached.", self.cfg.max_sessions),
            ));
        }
        let w = self.cfg.session_window;
        self.sessions.insert(
            channel,
            Session {
                next_incoming_id: b.next_outgoing_id,
                incoming_window: w,
                next_outgoing_id: self.cfg.initial_outgoing_id,
                outgoing_window: w,
                remote_incoming_window: b.incoming_window,
                next_delivery_id: 0,
                links: HashMap::new(),
                end_sent: false,
            },
        );
        self.send(
            channel,
            Performative::Begin(Begin {
                remote_channel: Some(channel),
                next_outgoing_id: self.cfg.initial_outgoing_id,
                incoming_window: w,
                outgoing_window: w,
                handle_max: HANDLE_MAX,
                ..Default::default()
            }),
        );
        Ok(())
    }

    pub(crate) fn on_end(&mut self, channel: u16, e: End) -> Result<(), Error> {
        let Some(sess) = self.sessions.get(&channel) else {
            return Err(Error::new(condition::CONNECTION_FRAMING_ERROR, format!("end on unmapped channel {channel}")));
        };
        if let Some(err) = &e.error {
            debug!("peer ended session {channel} with error: {err}");
        }
        let was_sent = sess.end_sent;
        let handles: Vec<u32> = sess.links.keys().copied().collect();
        for h in handles {
            self.notify_detach(channel, h, DetachReason::SessionEnded);
        }
        if let Some(sess) = self.sessions.remove(&channel) {
            for l in sess.links.values() {
                self.link_index.remove(&l.id);
            }
        }
        if !was_sent {
            self.send(channel, Performative::End(End { error: None }));
        }
        Ok(())
    }

    /// End a session because of a protocol violation (a session error, not a connection error).
    pub(crate) fn end_session_with_error(&mut self, channel: u16, error: Error) {
        let Some(sess) = self.sessions.get(&channel) else { return };
        if sess.end_sent {
            return;
        }
        debug!("ending session {channel}: {error}");
        let handles: Vec<u32> = sess.links.keys().copied().collect();
        for h in handles {
            self.notify_detach(channel, h, DetachReason::SessionEnded);
        }
        if let Some(sess) = self.sessions.get_mut(&channel) {
            sess.end_sent = true;
            // links are gone from the service's point of view; keep none
            for l in sess.links.values() {
                self.link_index.remove(&l.id);
            }
            sess.links.clear();
        }
        self.send(channel, Performative::End(End { error: Some(error) }));
    }

    pub(crate) fn session_flow(sess: &Session) -> Flow {
        Flow {
            next_incoming_id: Some(sess.next_incoming_id),
            incoming_window: sess.incoming_window,
            next_outgoing_id: sess.next_outgoing_id,
            outgoing_window: sess.outgoing_window,
            ..Default::default()
        }
    }

    pub(crate) fn on_flow(&mut self, channel: u16, f: Flow) -> Result<(), Error> {
        let Some(sess) = self.sessions.get_mut(&channel) else {
            return Err(Error::new(condition::CONNECTION_FRAMING_ERROR, format!("flow on unmapped channel {channel}")));
        };
        if sess.end_sent {
            return Ok(());
        }
        let remote_next_in = f.next_incoming_id.unwrap_or(sess.next_outgoing_id);
        sess.remote_incoming_window = remote_window(remote_next_in, f.incoming_window, sess.next_outgoing_id);
        if sess.remote_incoming_window > 0 {
            for l in sess.links.values_mut() {
                if l.role == Role::Sender {
                    l.needs_pull = true;
                }
            }
            self.pump_needed = true;
        }
        let mut replies: Vec<Flow> = Vec::new();
        let mut link_flow_events: Option<(LinkId, u32, bool)> = None;
        match f.handle {
            Some(h) => {
                let base = Self::session_flow(sess);
                let Some(link) = sess.links.get_mut(&h) else {
                    self.end_session_with_error(
                        channel,
                        Error::new(condition::SESSION_UNATTACHED_HANDLE, format!("flow for unknown handle {h}")),
                    );
                    return Ok(());
                };
                match link.role {
                    Role::Sender => {
                        let dc = f.delivery_count.unwrap_or(link.delivery_count);
                        link.credit = remote_credit(dc, f.link_credit.unwrap_or(0), link.delivery_count);
                        link.drain = f.drain;
                        link.needs_pull = true;
                        self.pump_needed = true;
                        link_flow_events = Some((link.id, link.credit, link.drain));
                    }
                    Role::Receiver => {
                        // The peer is the sender; with drain it tells us its final delivery-count.
                        if f.drain {
                            if let Some(dc) = f.delivery_count {
                                link.delivery_count = dc;
                                link.credit = 0;
                            }
                        }
                    }
                }
                if f.echo {
                    let mut r = base;
                    r.handle = Some(h);
                    r.delivery_count = Some(link.delivery_count);
                    r.link_credit = Some(link.credit);
                    r.drain = link.drain;
                    replies.push(r);
                }
            }
            None => {
                if f.echo {
                    replies.push(Self::session_flow(sess));
                }
            }
        }
        for r in replies {
            self.send(channel, Performative::Flow(r));
        }
        if let Some((id, credit, drain)) = link_flow_events {
            self.handler.on_flow(id, credit, drain);
        }
        Ok(())
    }

    pub(crate) fn on_transfer(&mut self, channel: u16, t: Transfer, payload: Bytes) -> Result<(), Error> {
        let Some(sess) = self.sessions.get_mut(&channel) else {
            return Err(Error::new(condition::CONNECTION_FRAMING_ERROR, format!("transfer on unmapped channel {channel}")));
        };
        if sess.end_sent {
            return Ok(());
        }
        if sess.incoming_window == 0 {
            self.end_session_with_error(channel, Error::new(condition::SESSION_WINDOW_VIOLATION, "incoming window exhausted"));
            return Ok(());
        }
        sess.incoming_window -= 1;
        sess.next_incoming_id = sess.next_incoming_id.wrapping_add(1);

        let mut frames: Vec<Performative> = Vec::new();
        let mut detach_error: Option<Error> = None;
        let mut completed: Option<(LinkId, IncomingDelivery)> = None;
        let mut oversize: Option<(u32, Error)> = None;
        let mut large_partial = false;
        {
            let Some(link) = sess.links.get_mut(&t.handle) else {
                self.end_session_with_error(
                    channel,
                    Error::new(condition::SESSION_UNATTACHED_HANDLE, format!("transfer for unknown handle {}", t.handle)),
                );
                return Ok(());
            };
            if link.state != LinkState::Attached {
                return Ok(());
            }
            if link.role != Role::Receiver {
                detach_error = Some(Error::new(condition::ILLEGAL_STATE, "transfer on a link this server sends on"));
            } else if t.aborted {
                link.partial = None;
            } else {
                if link.partial.is_none() {
                    match t.delivery_id {
                        None => {
                            detach_error = Some(Error::new(condition::ILLEGAL_STATE, "first transfer of a delivery has no delivery-id"))
                        }
                        Some(_) if link.credit == 0 => {
                            detach_error = Some(Error::new(condition::LINK_TRANSFER_LIMIT_EXCEEDED, "transfer without link credit"))
                        }
                        Some(id) => {
                            link.partial = Some(Partial {
                                delivery_id: id,
                                tag: t.delivery_tag.clone().unwrap_or_default(),
                                message_format: t.message_format.unwrap_or(0),
                                settled: t.settled.unwrap_or(false),
                                state: t.state.clone(),
                                payload: bytes::BytesMut::new(),
                                overflow: false,
                                total: 0,
                            })
                        }
                    }
                }
                if detach_error.is_none() {
                    if let Some(p) = link.partial.as_mut() {
                        p.total += payload.len();
                        if !p.overflow {
                            if link.max_message_size > 0 && p.total as u64 > link.max_message_size {
                                p.overflow = true;
                                p.payload.clear();
                            } else {
                                p.payload.extend_from_slice(&payload);
                                large_partial = p.payload.len() > LARGE_PARTIAL;
                            }
                        }
                        if !t.more {
                            let p = link.partial.take().expect("partial present");
                            link.credit = link.credit.saturating_sub(1);
                            link.delivery_count = link.delivery_count.wrapping_add(1);
                            if p.overflow {
                                // Reject the delivery (or, when it was sent pre-settled and nobody
                                // can be told, detach): the sender learns the limit either way.
                                let err = Error::new(
                                    condition::LINK_MESSAGE_SIZE_EXCEEDED,
                                    format!(
                                        "The received message (delivery-id:{}, size:{} bytes) exceeds the limit ({} bytes) currently allowed on the link.",
                                        p.delivery_id, p.total, link.max_message_size
                                    ),
                                );
                                if p.settled {
                                    detach_error = Some(err);
                                } else {
                                    oversize = Some((p.delivery_id, err));
                                }
                            } else {
                                completed = Some((
                                    link.id,
                                    IncomingDelivery {
                                        delivery_id: p.delivery_id,
                                        tag: p.tag,
                                        message_format: p.message_format,
                                        settled: p.settled,
                                        state: p.state,
                                        payload: p.payload.freeze(),
                                    },
                                ));
                            }
                        }
                    }
                }
            }
        }

        if large_partial {
            // Half-received messages of all links together: only counted once one is big.
            let held: usize =
                self.sessions.values().flat_map(|s| s.links.values()).filter_map(|l| l.partial.as_ref()).map(|p| p.payload.len()).sum();
            if held > self.cfg.max_partial_bytes {
                return Err(Error::new(
                    condition::RESOURCE_LIMIT_EXCEEDED,
                    format!("More than {} bytes of incomplete messages are held on this connection.", self.cfg.max_partial_bytes),
                ));
            }
        }

        if let Some(err) = detach_error {
            self.detach_link(channel, t.handle, Some(err));
        }

        if let Some((delivery_id, err)) = oversize {
            self.settle_incoming(channel, t.handle, delivery_id, DeliveryState::rejected(err));
        }

        if let Some((id, delivery)) = completed {
            let (delivery_id, presettled) = (delivery.delivery_id, delivery.settled);
            let action = self.handler.on_transfer(id, delivery);
            if let TransferAction::Settle(state) = action {
                if !presettled {
                    self.settle_incoming(channel, t.handle, delivery_id, state);
                }
            }
        }

        // Top up link credit and the session window before they run dry.
        let credit_cfg = self.cfg.receiver_credit;
        let window_cfg = self.cfg.session_window;
        if let Some(sess) = self.sessions.get_mut(&channel) {
            if let Some(link) = sess.links.get_mut(&t.handle) {
                if link.role == Role::Receiver && link.state == LinkState::Attached && link.credit <= credit_cfg / 2 {
                    link.credit = credit_cfg;
                    frames.push(Performative::Flow(Flow {
                        handle: Some(link.handle),
                        delivery_count: Some(link.delivery_count),
                        link_credit: Some(link.credit),
                        ..Self::session_flow(sess)
                    }));
                }
            }
            if sess.incoming_window <= window_cfg / 2 {
                sess.incoming_window = window_cfg;
                frames.push(Performative::Flow(Self::session_flow(sess)));
            }
        }
        for f in frames {
            self.send(channel, f);
        }
        Ok(())
    }

    /// Send our disposition for an incoming delivery.
    pub(crate) fn settle_incoming(&mut self, channel: u16, handle: u32, delivery_id: u32, state: DeliveryState) {
        let Some(sess) = self.sessions.get_mut(&channel) else { return };
        let Some(link) = sess.links.get_mut(&handle) else { return };
        // Receiver settle mode `second` asks for a two-step settlement, but the clients that
        // matter (the Python SDK's pyamqp sender) never send the sender's settling
        // disposition and wait for ours to be settled. Like Service Bus, settle at once.
        let second = false;
        let _ = (&link.rcv_settle_mode, &link.unsettled_in);
        self.send(
            channel,
            Performative::Disposition(Disposition {
                role: Role::Receiver,
                first: delivery_id,
                last: None,
                settled: !second,
                state: Some(state),
                batchable: false,
            }),
        );
    }

    pub(crate) fn on_disposition(&mut self, channel: u16, d: Disposition) -> Result<(), Error> {
        let Some(sess) = self.sessions.get_mut(&channel) else {
            return Err(Error::new(condition::CONNECTION_FRAMING_ERROR, format!("disposition on unmapped channel {channel}")));
        };
        if sess.end_sent {
            return Ok(());
        }
        let first = d.first;
        let last = d.last.unwrap_or(d.first);
        let in_range = |id: u32| id.wrapping_sub(first) <= last.wrapping_sub(first);
        match d.role {
            // The peer is the receiver of deliveries we sent.
            Role::Receiver => {
                let mut events: Vec<(u32, LinkId, Vec<u8>)> = Vec::new();
                for link in sess.links.values_mut() {
                    if link.role != Role::Sender {
                        continue;
                    }
                    let mut ids: Vec<u32> = link.unsettled_out.keys().copied().filter(|id| in_range(*id)).collect();
                    ids.sort_unstable();
                    let wants_settle = !d.settled && d.state.as_ref().map(|s| s.is_terminal()).unwrap_or(false);
                    for id in ids {
                        let tag =
                            if d.settled || wants_settle { link.unsettled_out.remove(&id) } else { link.unsettled_out.get(&id).cloned() };
                        if let Some(tag) = tag {
                            events.push((id, link.id, tag));
                        }
                    }
                }
                let wants_settle = !d.settled && d.state.as_ref().map(|s| s.is_terminal()).unwrap_or(false);
                for (delivery_id, link_id, tag) in events {
                    let answer = self.handler.on_disposition(link_id, &tag, d.state.clone(), d.settled);
                    if wants_settle {
                        let state = answer.or_else(|| d.state.clone());
                        self.send(
                            channel,
                            Performative::Disposition(Disposition {
                                role: Role::Sender,
                                first: delivery_id,
                                last: None,
                                settled: true,
                                state,
                                batchable: false,
                            }),
                        );
                    }
                }
            }
            // The peer is the sender of deliveries we received and answered unsettled.
            Role::Sender => {
                let mut to_settle: Vec<u32> = Vec::new();
                if d.settled {
                    for link in sess.links.values_mut() {
                        let ids: Vec<u32> = link.unsettled_in.iter().copied().filter(|id| in_range(*id)).collect();
                        for id in ids {
                            link.unsettled_in.remove(&id);
                            to_settle.push(id);
                        }
                    }
                }
                for id in to_settle {
                    self.send(
                        channel,
                        Performative::Disposition(Disposition {
                            role: Role::Receiver,
                            first: id,
                            last: None,
                            settled: true,
                            state: None,
                            batchable: false,
                        }),
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{remote_credit, remote_window};

    #[test]
    fn window_is_unsigned() {
        assert_eq!(remote_window(0, u32::MAX, 0), u32::MAX);
        assert_eq!(remote_window(0, 1 << 31, 0), 1 << 31);
        assert_eq!(remote_window(5, 10, 5), 10);
        assert_eq!(remote_window(5, 10, 8), 7);
        assert_eq!(remote_window(5, 3, 8), 0, "more in flight than the window: nothing left");
        assert_eq!(remote_window(0, u32::MAX, 3), u32::MAX - 3);
    }

    #[test]
    fn window_survives_the_wrap() {
        // we are at 2 (after the wrap); the peer has seen up to u32::MAX - 1, so 4 are in flight
        assert_eq!(remote_window(u32::MAX - 1, 10, 2), 6);
        assert_eq!(remote_window(u32::MAX - 1, u32::MAX, 2), u32::MAX - 4);
        // the peer is ahead of us (broken): the window is what it says
        assert_eq!(remote_window(7, 10, 5), 10);
    }

    #[test]
    fn credit_is_unsigned_and_survives_the_wrap() {
        assert_eq!(remote_credit(0, u32::MAX, 0), u32::MAX);
        assert_eq!(remote_credit(0, 1 << 31, 0), 1 << 31);
        assert_eq!(remote_credit(10, 5, 12), 3);
        assert_eq!(remote_credit(10, 1, 12), 0);
        // delivery count wrapped: we have sent 5 more since the peer's count of u32::MAX - 2
        assert_eq!(remote_credit(u32::MAX - 2, 100, 2), 95);
    }
}
