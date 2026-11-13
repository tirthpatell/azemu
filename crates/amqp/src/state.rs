//! Connection, session and link state owned by the connection task.

use std::collections::{HashMap, HashSet};

use bytes::BytesMut;

use crate::api::LinkId;
use crate::performatives::{Attach, RcvSettleMode, Role, SndSettleMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// Attach seen, the service has not answered yet.
    Pending,
    Attached,
    /// We sent a detach and wait for the peer's.
    DetachSent,
}

/// A multi-frame incoming delivery being reassembled.
pub(crate) struct Partial {
    pub delivery_id: u32,
    pub tag: Vec<u8>,
    pub message_format: u32,
    pub settled: bool,
    pub state: Option<crate::terminus::DeliveryState>,
    pub payload: BytesMut,
    /// The message outgrew the link's max-message-size: the rest is read and thrown away.
    pub overflow: bool,
    pub total: usize,
}

pub(crate) struct Link {
    pub id: LinkId,
    /// Same number on both sides of the wire.
    pub handle: u32,
    pub name: String,
    /// Our role.
    pub role: Role,
    pub state: LinkState,
    pub snd_settle_mode: SndSettleMode,
    pub rcv_settle_mode: RcvSettleMode,
    pub credit: u32,
    pub delivery_count: u32,
    pub drain: bool,
    pub needs_pull: bool,
    pub max_message_size: u64,
    pub partial: Option<Partial>,
    /// Deliveries we sent that are not settled: delivery-id to tag.
    pub unsettled_out: HashMap<u32, Vec<u8>>,
    /// Deliveries we received and answered with an unsettled state (rcv-settle-mode second).
    pub unsettled_in: HashSet<u32>,
    pub peer_attach: Option<Attach>,
    pub notified: bool,
}

pub(crate) struct Session {
    pub next_incoming_id: u32,
    pub incoming_window: u32,
    pub next_outgoing_id: u32,
    pub outgoing_window: u32,
    pub remote_incoming_window: u32,
    pub next_delivery_id: u32,
    pub links: HashMap<u32, Link>,
    /// We sent `end`; waiting for the peer's.
    pub end_sent: bool,
}

/// The `handle-max` we advertise in `begin`; a link handle above it is refused.
pub(crate) const HANDLE_MAX: u32 = 65535;
