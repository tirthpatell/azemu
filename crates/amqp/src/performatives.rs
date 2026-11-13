//! The nine AMQP performatives (part 2, section 2.7).

use crate::codec::DecodeError;
use crate::error::Error;
use crate::fields::{described, opt, symbols, Fields};
use crate::terminus::{DeliveryState, Source, TargetKind};
use crate::types::Value;

pub const DESC_OPEN: u64 = 0x10;
pub const DESC_BEGIN: u64 = 0x11;
pub const DESC_ATTACH: u64 = 0x12;
pub const DESC_FLOW: u64 = 0x13;
pub const DESC_TRANSFER: u64 = 0x14;
pub const DESC_DISPOSITION: u64 = 0x15;
pub const DESC_DETACH: u64 = 0x16;
pub const DESC_END: u64 = 0x17;
pub const DESC_CLOSE: u64 = 0x18;

/// Link role: the sender of messages or the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Sender,
    Receiver,
}
impl Role {
    pub fn is_receiver(self) -> bool {
        self == Role::Receiver
    }
    pub fn to_bool(self) -> bool {
        self == Role::Receiver
    }
    pub fn from_bool(b: bool) -> Role {
        if b {
            Role::Receiver
        } else {
            Role::Sender
        }
    }
    pub fn flip(self) -> Role {
        match self {
            Role::Sender => Role::Receiver,
            Role::Receiver => Role::Sender,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SndSettleMode {
    Unsettled = 0,
    Settled = 1,
    Mixed = 2,
}
impl SndSettleMode {
    pub fn from_u8(v: u8) -> SndSettleMode {
        match v {
            0 => SndSettleMode::Unsettled,
            1 => SndSettleMode::Settled,
            _ => SndSettleMode::Mixed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcvSettleMode {
    First = 0,
    Second = 1,
}
impl RcvSettleMode {
    pub fn from_u8(v: u8) -> RcvSettleMode {
        if v == 1 {
            RcvSettleMode::Second
        } else {
            RcvSettleMode::First
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Open {
    pub container_id: String,
    pub hostname: Option<String>,
    pub max_frame_size: u32,
    pub channel_max: u16,
    pub idle_timeout: Option<u32>,
    pub outgoing_locales: Vec<String>,
    pub incoming_locales: Vec<String>,
    pub offered_capabilities: Vec<String>,
    pub desired_capabilities: Vec<String>,
    pub properties: Option<Value>,
}

impl Default for Open {
    fn default() -> Self {
        Open {
            container_id: String::new(),
            hostname: None,
            max_frame_size: u32::MAX,
            channel_max: u16::MAX,
            idle_timeout: None,
            outgoing_locales: vec![],
            incoming_locales: vec![],
            offered_capabilities: vec![],
            desired_capabilities: vec![],
            properties: None,
        }
    }
}

impl Open {
    pub fn to_value(&self) -> Value {
        described(
            DESC_OPEN,
            vec![
                Value::String(self.container_id.clone()),
                opt(self.hostname.clone()),
                if self.max_frame_size == u32::MAX { Value::Null } else { Value::Uint(self.max_frame_size) },
                if self.channel_max == u16::MAX { Value::Null } else { Value::Ushort(self.channel_max) },
                opt(self.idle_timeout),
                symbols(&self.outgoing_locales),
                symbols(&self.incoming_locales),
                symbols(&self.offered_capabilities),
                symbols(&self.desired_capabilities),
                self.properties.clone().unwrap_or(Value::Null),
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Open, DecodeError> {
        let f = Fields::of(v, DESC_OPEN, "amqp:open:list")?;
        Ok(Open {
            container_id: f.req_string(0)?,
            hostname: f.opt_string(1)?,
            max_frame_size: f.u32_or(2, u32::MAX)?,
            channel_max: f.opt_u32(3)?.map(|x| x.min(u16::MAX as u32) as u16).unwrap_or(u16::MAX),
            idle_timeout: f.opt_u32(4)?,
            outgoing_locales: f.symbols(5),
            incoming_locales: f.symbols(6),
            offered_capabilities: f.symbols(7),
            desired_capabilities: f.symbols(8),
            properties: f.opt_value(9),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Begin {
    pub remote_channel: Option<u16>,
    pub next_outgoing_id: u32,
    pub incoming_window: u32,
    pub outgoing_window: u32,
    pub handle_max: u32,
    pub offered_capabilities: Vec<String>,
    pub desired_capabilities: Vec<String>,
    pub properties: Option<Value>,
}

impl Default for Begin {
    fn default() -> Self {
        Begin {
            remote_channel: None,
            next_outgoing_id: 0,
            incoming_window: 2048,
            outgoing_window: 2048,
            handle_max: u32::MAX,
            offered_capabilities: vec![],
            desired_capabilities: vec![],
            properties: None,
        }
    }
}

impl Begin {
    pub fn to_value(&self) -> Value {
        described(
            DESC_BEGIN,
            vec![
                self.remote_channel.map(Value::Ushort).unwrap_or(Value::Null),
                Value::Uint(self.next_outgoing_id),
                Value::Uint(self.incoming_window),
                Value::Uint(self.outgoing_window),
                if self.handle_max == u32::MAX { Value::Null } else { Value::Uint(self.handle_max) },
                symbols(&self.offered_capabilities),
                symbols(&self.desired_capabilities),
                self.properties.clone().unwrap_or(Value::Null),
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Begin, DecodeError> {
        let f = Fields::of(v, DESC_BEGIN, "amqp:begin:list")?;
        Ok(Begin {
            remote_channel: f.opt_u32(0)?.map(|x| x.min(u16::MAX as u32) as u16),
            next_outgoing_id: f.req_u32(1)?,
            incoming_window: f.req_u32(2)?,
            outgoing_window: f.req_u32(3)?,
            handle_max: f.u32_or(4, u32::MAX)?,
            offered_capabilities: f.symbols(5),
            desired_capabilities: f.symbols(6),
            properties: f.opt_value(7),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attach {
    pub name: String,
    pub handle: u32,
    pub role: Role,
    pub snd_settle_mode: SndSettleMode,
    pub rcv_settle_mode: RcvSettleMode,
    pub source: Option<Source>,
    pub target: Option<TargetKind>,
    pub unsettled: Option<Value>,
    pub incomplete_unsettled: bool,
    pub initial_delivery_count: Option<u32>,
    pub max_message_size: Option<u64>,
    pub offered_capabilities: Vec<String>,
    pub desired_capabilities: Vec<String>,
    pub properties: Option<Value>,
}

impl Attach {
    pub fn new(name: impl Into<String>, handle: u32, role: Role) -> Attach {
        Attach {
            name: name.into(),
            handle,
            role,
            snd_settle_mode: SndSettleMode::Mixed,
            rcv_settle_mode: RcvSettleMode::First,
            source: None,
            target: None,
            unsettled: None,
            incomplete_unsettled: false,
            initial_delivery_count: if role == Role::Sender { Some(0) } else { None },
            max_message_size: None,
            offered_capabilities: vec![],
            desired_capabilities: vec![],
            properties: None,
        }
    }
    pub fn to_value(&self) -> Value {
        described(
            DESC_ATTACH,
            vec![
                Value::String(self.name.clone()),
                Value::Uint(self.handle),
                Value::Bool(self.role.to_bool()),
                if self.snd_settle_mode == SndSettleMode::Mixed { Value::Null } else { Value::Ubyte(self.snd_settle_mode as u8) },
                if self.rcv_settle_mode == RcvSettleMode::First { Value::Null } else { Value::Ubyte(self.rcv_settle_mode as u8) },
                self.source.as_ref().map(|s| s.to_value()).unwrap_or(Value::Null),
                self.target.as_ref().map(|t| t.to_value()).unwrap_or(Value::Null),
                self.unsettled.clone().unwrap_or(Value::Null),
                if self.incomplete_unsettled { Value::Bool(true) } else { Value::Null },
                opt(self.initial_delivery_count),
                opt(self.max_message_size),
                symbols(&self.offered_capabilities),
                symbols(&self.desired_capabilities),
                self.properties.clone().unwrap_or(Value::Null),
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Attach, DecodeError> {
        let f = Fields::of(v, DESC_ATTACH, "amqp:attach:list")?;
        Ok(Attach {
            name: f.req_string(0)?,
            handle: f.req_u32(1)?,
            role: Role::from_bool(f.opt_bool(2)?.ok_or(DecodeError::Invalid("attach without role"))?),
            snd_settle_mode: SndSettleMode::from_u8(f.opt_u8(3)?.unwrap_or(2)),
            rcv_settle_mode: RcvSettleMode::from_u8(f.opt_u8(4)?.unwrap_or(0)),
            source: match f.get(5) {
                Value::Null => None,
                s => Some(Source::from_value(s)?),
            },
            target: match f.get(6) {
                Value::Null => None,
                t => Some(TargetKind::from_value(t)?),
            },
            unsettled: f.opt_value(7),
            incomplete_unsettled: f.bool_or(8, false)?,
            initial_delivery_count: f.opt_u32(9)?,
            max_message_size: f.opt_u64(10)?,
            offered_capabilities: f.symbols(11),
            desired_capabilities: f.symbols(12),
            properties: f.opt_value(13),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Flow {
    pub next_incoming_id: Option<u32>,
    pub incoming_window: u32,
    pub next_outgoing_id: u32,
    pub outgoing_window: u32,
    pub handle: Option<u32>,
    pub delivery_count: Option<u32>,
    pub link_credit: Option<u32>,
    pub available: Option<u32>,
    pub drain: bool,
    pub echo: bool,
    pub properties: Option<Value>,
}

impl Flow {
    pub fn to_value(&self) -> Value {
        described(
            DESC_FLOW,
            vec![
                opt(self.next_incoming_id),
                Value::Uint(self.incoming_window),
                Value::Uint(self.next_outgoing_id),
                Value::Uint(self.outgoing_window),
                opt(self.handle),
                opt(self.delivery_count),
                opt(self.link_credit),
                opt(self.available),
                if self.drain { Value::Bool(true) } else { Value::Null },
                if self.echo { Value::Bool(true) } else { Value::Null },
                self.properties.clone().unwrap_or(Value::Null),
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Flow, DecodeError> {
        let f = Fields::of(v, DESC_FLOW, "amqp:flow:list")?;
        Ok(Flow {
            next_incoming_id: f.opt_u32(0)?,
            incoming_window: f.req_u32(1)?,
            next_outgoing_id: f.req_u32(2)?,
            outgoing_window: f.req_u32(3)?,
            handle: f.opt_u32(4)?,
            delivery_count: f.opt_u32(5)?,
            link_credit: f.opt_u32(6)?,
            available: f.opt_u32(7)?,
            drain: f.bool_or(8, false)?,
            echo: f.bool_or(9, false)?,
            properties: f.opt_value(10),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Transfer {
    pub handle: u32,
    pub delivery_id: Option<u32>,
    pub delivery_tag: Option<Vec<u8>>,
    pub message_format: Option<u32>,
    pub settled: Option<bool>,
    pub more: bool,
    pub rcv_settle_mode: Option<u8>,
    pub state: Option<DeliveryState>,
    pub resume: bool,
    pub aborted: bool,
    pub batchable: bool,
}

impl Transfer {
    pub fn to_value(&self) -> Value {
        described(
            DESC_TRANSFER,
            vec![
                Value::Uint(self.handle),
                opt(self.delivery_id),
                self.delivery_tag.clone().map(Value::Binary).unwrap_or(Value::Null),
                opt(self.message_format),
                opt(self.settled),
                if self.more { Value::Bool(true) } else { Value::Null },
                self.rcv_settle_mode.map(Value::Ubyte).unwrap_or(Value::Null),
                self.state.as_ref().map(|s| s.to_value()).unwrap_or(Value::Null),
                if self.resume { Value::Bool(true) } else { Value::Null },
                if self.aborted { Value::Bool(true) } else { Value::Null },
                if self.batchable { Value::Bool(true) } else { Value::Null },
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Transfer, DecodeError> {
        let f = Fields::of(v, DESC_TRANSFER, "amqp:transfer:list")?;
        Ok(Transfer {
            handle: f.req_u32(0)?,
            delivery_id: f.opt_u32(1)?,
            delivery_tag: f.opt_binary(2)?,
            message_format: f.opt_u32(3)?,
            settled: f.opt_bool(4)?,
            more: f.bool_or(5, false)?,
            rcv_settle_mode: f.opt_u8(6)?,
            state: match f.get(7) {
                Value::Null => None,
                s => Some(DeliveryState::from_value(s)?),
            },
            resume: f.bool_or(8, false)?,
            aborted: f.bool_or(9, false)?,
            batchable: f.bool_or(10, false)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Disposition {
    pub role: Role,
    pub first: u32,
    pub last: Option<u32>,
    pub settled: bool,
    pub state: Option<DeliveryState>,
    pub batchable: bool,
}

impl Disposition {
    pub fn to_value(&self) -> Value {
        described(
            DESC_DISPOSITION,
            vec![
                Value::Bool(self.role.to_bool()),
                Value::Uint(self.first),
                opt(self.last),
                if self.settled { Value::Bool(true) } else { Value::Null },
                self.state.as_ref().map(|s| s.to_value()).unwrap_or(Value::Null),
                if self.batchable { Value::Bool(true) } else { Value::Null },
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Disposition, DecodeError> {
        let f = Fields::of(v, DESC_DISPOSITION, "amqp:disposition:list")?;
        Ok(Disposition {
            role: Role::from_bool(f.opt_bool(0)?.ok_or(DecodeError::Invalid("disposition without role"))?),
            first: f.req_u32(1)?,
            last: f.opt_u32(2)?,
            settled: f.bool_or(3, false)?,
            state: match f.get(4) {
                Value::Null => None,
                s => Some(DeliveryState::from_value(s)?),
            },
            batchable: f.bool_or(5, false)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Detach {
    pub handle: u32,
    pub closed: bool,
    pub error: Option<Error>,
}

impl Detach {
    pub fn to_value(&self) -> Value {
        described(
            DESC_DETACH,
            vec![
                Value::Uint(self.handle),
                if self.closed { Value::Bool(true) } else { Value::Null },
                self.error.as_ref().map(|e| e.to_value()).unwrap_or(Value::Null),
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Detach, DecodeError> {
        let f = Fields::of(v, DESC_DETACH, "amqp:detach:list")?;
        Ok(Detach {
            handle: f.req_u32(0)?,
            closed: f.bool_or(1, false)?,
            error: match f.get(2) {
                Value::Null => None,
                e => Some(Error::from_value(e)?),
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct End {
    pub error: Option<Error>,
}

impl End {
    pub fn to_value(&self) -> Value {
        described(DESC_END, vec![self.error.as_ref().map(|e| e.to_value()).unwrap_or(Value::Null)])
    }
    pub fn from_value(v: &Value) -> Result<End, DecodeError> {
        let f = Fields::of(v, DESC_END, "amqp:end:list")?;
        Ok(End {
            error: match f.get(0) {
                Value::Null => None,
                e => Some(Error::from_value(e)?),
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Close {
    pub error: Option<Error>,
}

impl Close {
    pub fn to_value(&self) -> Value {
        described(DESC_CLOSE, vec![self.error.as_ref().map(|e| e.to_value()).unwrap_or(Value::Null)])
    }
    pub fn from_value(v: &Value) -> Result<Close, DecodeError> {
        let f = Fields::of(v, DESC_CLOSE, "amqp:close:list")?;
        Ok(Close {
            error: match f.get(0) {
                Value::Null => None,
                e => Some(Error::from_value(e)?),
            },
        })
    }
}

/// Any performative.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Performative {
    Open(Open),
    Begin(Begin),
    Attach(Attach),
    Flow(Flow),
    Transfer(Transfer),
    Disposition(Disposition),
    Detach(Detach),
    End(End),
    Close(Close),
}

/// Number of fields of each performative in the specification.
fn field_count(code: u64) -> usize {
    match code {
        DESC_OPEN => 10,
        DESC_BEGIN => 8,
        DESC_ATTACH => 14,
        DESC_FLOW => 11,
        DESC_TRANSFER => 11,
        DESC_DISPOSITION => 6,
        DESC_DETACH => 3,
        _ => 1,
    }
}

impl Performative {
    /// The value as it goes on the wire: every field is present, trailing nulls included. The
    /// Event Hubs Python SDK (pyamqp 5.15) reads fields such as `frame[9]` of `open`,
    /// `frame[13]` of `attach`, `frame[2]` of `detach` and `frame[11]` (the payload) of
    /// `transfer` by position and does not pad short lists; Azure never sends short ones. (The
    /// `to_value` of each struct drops trailing nulls, as the specification allows.)
    pub fn to_value(&self) -> Value {
        let mut v = self.to_value_minimal();
        if let Value::Described(d) = &mut v {
            if let (Some(code), Value::List(items)) = (d.descriptor.as_u64(), &mut d.value) {
                while items.len() < field_count(code) {
                    items.push(Value::Null);
                }
            }
        }
        v
    }

    fn to_value_minimal(&self) -> Value {
        match self {
            Performative::Open(p) => p.to_value(),
            Performative::Begin(p) => p.to_value(),
            Performative::Attach(p) => p.to_value(),
            Performative::Flow(p) => p.to_value(),
            Performative::Transfer(p) => p.to_value(),
            Performative::Disposition(p) => p.to_value(),
            Performative::Detach(p) => p.to_value(),
            Performative::End(p) => p.to_value(),
            Performative::Close(p) => p.to_value(),
        }
    }
    pub fn from_value(v: &Value) -> Result<Performative, DecodeError> {
        let code = v.as_described().and_then(|d| d.descriptor.as_u64()).or_else(|| {
            let name = v.as_described()?.descriptor.as_str()?;
            Some(match name {
                "amqp:open:list" => DESC_OPEN,
                "amqp:begin:list" => DESC_BEGIN,
                "amqp:attach:list" => DESC_ATTACH,
                "amqp:flow:list" => DESC_FLOW,
                "amqp:transfer:list" => DESC_TRANSFER,
                "amqp:disposition:list" => DESC_DISPOSITION,
                "amqp:detach:list" => DESC_DETACH,
                "amqp:end:list" => DESC_END,
                "amqp:close:list" => DESC_CLOSE,
                _ => return None,
            })
        });
        Ok(match code {
            Some(DESC_OPEN) => Performative::Open(Open::from_value(v)?),
            Some(DESC_BEGIN) => Performative::Begin(Begin::from_value(v)?),
            Some(DESC_ATTACH) => Performative::Attach(Attach::from_value(v)?),
            Some(DESC_FLOW) => Performative::Flow(Flow::from_value(v)?),
            Some(DESC_TRANSFER) => Performative::Transfer(Transfer::from_value(v)?),
            Some(DESC_DISPOSITION) => Performative::Disposition(Disposition::from_value(v)?),
            Some(DESC_DETACH) => Performative::Detach(Detach::from_value(v)?),
            Some(DESC_END) => Performative::End(End::from_value(v)?),
            Some(DESC_CLOSE) => Performative::Close(Close::from_value(v)?),
            _ => return Err(DecodeError::Invalid("unknown performative")),
        })
    }
}
