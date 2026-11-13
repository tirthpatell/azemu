//! Source, target and coordinator termini, delivery states and outcomes, and the transaction
//! messages (part 3 messaging, part 4 transactions).

use crate::codec::DecodeError;
use crate::error::Error;
use crate::fields::{described, opt, opt_sym, symbols, Fields};
use crate::types::Value;

pub const DESC_RECEIVED: u64 = 0x23;
pub const DESC_ACCEPTED: u64 = 0x24;
pub const DESC_REJECTED: u64 = 0x25;
pub const DESC_RELEASED: u64 = 0x26;
pub const DESC_MODIFIED: u64 = 0x27;
pub const DESC_SOURCE: u64 = 0x28;
pub const DESC_TARGET: u64 = 0x29;
pub const DESC_COORDINATOR: u64 = 0x30;
pub const DESC_DECLARE: u64 = 0x31;
pub const DESC_DISCHARGE: u64 = 0x32;
pub const DESC_DECLARED: u64 = 0x33;
pub const DESC_TRANSACTIONAL_STATE: u64 = 0x34;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Source {
    pub address: Option<String>,
    pub durable: u32,
    pub expiry_policy: Option<String>,
    pub timeout: u32,
    pub dynamic: bool,
    pub dynamic_node_properties: Option<Value>,
    pub distribution_mode: Option<String>,
    /// The filter map (symbol -> described value), as received.
    pub filter: Option<Value>,
    pub default_outcome: Option<Value>,
    pub outcomes: Vec<String>,
    pub capabilities: Vec<String>,
}

impl Source {
    pub fn with_address(address: impl Into<String>) -> Source {
        Source { address: Some(address.into()), ..Default::default() }
    }
    pub fn to_value(&self) -> Value {
        described(
            DESC_SOURCE,
            vec![
                opt(self.address.clone()),
                if self.durable == 0 { Value::Null } else { Value::Uint(self.durable) },
                opt_sym(&self.expiry_policy),
                if self.timeout == 0 { Value::Null } else { Value::Uint(self.timeout) },
                if self.dynamic { Value::Bool(true) } else { Value::Null },
                self.dynamic_node_properties.clone().unwrap_or(Value::Null),
                opt_sym(&self.distribution_mode),
                self.filter.clone().unwrap_or(Value::Null),
                self.default_outcome.clone().unwrap_or(Value::Null),
                symbols(&self.outcomes),
                symbols(&self.capabilities),
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Source, DecodeError> {
        let f = Fields::of(v, DESC_SOURCE, "amqp:source:list")?;
        Ok(Source {
            address: f.opt_string(0)?,
            durable: f.u32_or(1, 0)?,
            expiry_policy: f.opt_string(2)?,
            timeout: f.u32_or(3, 0)?,
            dynamic: f.bool_or(4, false)?,
            dynamic_node_properties: f.opt_value(5),
            distribution_mode: f.opt_string(6)?,
            filter: f.opt_value(7),
            default_outcome: f.opt_value(8),
            outcomes: f.symbols(9),
            capabilities: f.symbols(10),
        })
    }
    /// Look up a filter by its symbol key. Returns the (possibly described) value.
    pub fn filter_get(&self, key: &str) -> Option<&Value> {
        self.filter.as_ref()?.map_get(key)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Target {
    pub address: Option<String>,
    pub durable: u32,
    pub expiry_policy: Option<String>,
    pub timeout: u32,
    pub dynamic: bool,
    pub dynamic_node_properties: Option<Value>,
    pub capabilities: Vec<String>,
}

impl Target {
    pub fn with_address(address: impl Into<String>) -> Target {
        Target { address: Some(address.into()), ..Default::default() }
    }
    pub fn to_value(&self) -> Value {
        described(
            DESC_TARGET,
            vec![
                opt(self.address.clone()),
                if self.durable == 0 { Value::Null } else { Value::Uint(self.durable) },
                opt_sym(&self.expiry_policy),
                if self.timeout == 0 { Value::Null } else { Value::Uint(self.timeout) },
                if self.dynamic { Value::Bool(true) } else { Value::Null },
                self.dynamic_node_properties.clone().unwrap_or(Value::Null),
                symbols(&self.capabilities),
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Target, DecodeError> {
        let f = Fields::of(v, DESC_TARGET, "amqp:target:list")?;
        Ok(Target {
            address: f.opt_string(0)?,
            durable: f.u32_or(1, 0)?,
            expiry_policy: f.opt_string(2)?,
            timeout: f.u32_or(3, 0)?,
            dynamic: f.bool_or(4, false)?,
            dynamic_node_properties: f.opt_value(5),
            capabilities: f.symbols(6),
        })
    }
}

/// The transaction coordinator target.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Coordinator {
    pub capabilities: Vec<String>,
}

impl Coordinator {
    pub fn to_value(&self) -> Value {
        described(DESC_COORDINATOR, vec![symbols(&self.capabilities)])
    }
    pub fn from_value(v: &Value) -> Result<Coordinator, DecodeError> {
        let f = Fields::of(v, DESC_COORDINATOR, "amqp:coordinator:list")?;
        Ok(Coordinator { capabilities: f.symbols(0) })
    }
}

/// What an attach frame carries in its `target` field.
#[derive(Debug, Clone, PartialEq)]
pub enum TargetKind {
    Target(Target),
    Coordinator(Coordinator),
}

impl TargetKind {
    pub fn to_value(&self) -> Value {
        match self {
            TargetKind::Target(t) => t.to_value(),
            TargetKind::Coordinator(c) => c.to_value(),
        }
    }
    pub fn from_value(v: &Value) -> Result<TargetKind, DecodeError> {
        match v.descriptor_code() {
            Some(DESC_COORDINATOR) => Ok(TargetKind::Coordinator(Coordinator::from_value(v)?)),
            _ => Ok(TargetKind::Target(Target::from_value(v)?)),
        }
    }
    pub fn address(&self) -> Option<&str> {
        match self {
            TargetKind::Target(t) => t.address.as_deref(),
            TargetKind::Coordinator(_) => None,
        }
    }
}

/// A delivery state or terminal outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum DeliveryState {
    Received {
        section_number: u32,
        section_offset: u64,
    },
    Accepted,
    Rejected {
        error: Option<Error>,
    },
    Released,
    Modified {
        delivery_failed: bool,
        undeliverable_here: bool,
        message_annotations: Option<Value>,
    },
    /// Result of a `declare`: the transaction id.
    Declared {
        txn_id: Vec<u8>,
    },
    Transactional {
        txn_id: Vec<u8>,
        outcome: Option<Box<DeliveryState>>,
    },
}

impl DeliveryState {
    pub fn modified(delivery_failed: bool, undeliverable_here: bool) -> DeliveryState {
        DeliveryState::Modified { delivery_failed, undeliverable_here, message_annotations: None }
    }
    pub fn rejected(error: Error) -> DeliveryState {
        DeliveryState::Rejected { error: Some(error) }
    }

    pub fn to_value(&self) -> Value {
        match self {
            DeliveryState::Received { section_number, section_offset } => {
                described(DESC_RECEIVED, vec![Value::Uint(*section_number), Value::Ulong(*section_offset)])
            }
            DeliveryState::Accepted => described(DESC_ACCEPTED, vec![]),
            DeliveryState::Rejected { error } => {
                described(DESC_REJECTED, vec![error.as_ref().map(|e| e.to_value()).unwrap_or(Value::Null)])
            }
            DeliveryState::Released => described(DESC_RELEASED, vec![]),
            DeliveryState::Modified { delivery_failed, undeliverable_here, message_annotations } => described(
                DESC_MODIFIED,
                vec![
                    if *delivery_failed { Value::Bool(true) } else { Value::Null },
                    if *undeliverable_here { Value::Bool(true) } else { Value::Null },
                    message_annotations.clone().unwrap_or(Value::Null),
                ],
            ),
            DeliveryState::Declared { txn_id } => described(DESC_DECLARED, vec![Value::Binary(txn_id.clone())]),
            DeliveryState::Transactional { txn_id, outcome } => described(
                DESC_TRANSACTIONAL_STATE,
                vec![Value::Binary(txn_id.clone()), outcome.as_ref().map(|o| o.to_value()).unwrap_or(Value::Null)],
            ),
        }
    }

    pub fn from_value(v: &Value) -> Result<DeliveryState, DecodeError> {
        let d = v.as_described().ok_or(DecodeError::Invalid("delivery state is not described"))?;
        let code = d.descriptor.as_u64().or_else(|| match d.descriptor.as_str() {
            Some("amqp:received:list") => Some(DESC_RECEIVED),
            Some("amqp:accepted:list") => Some(DESC_ACCEPTED),
            Some("amqp:rejected:list") => Some(DESC_REJECTED),
            Some("amqp:released:list") => Some(DESC_RELEASED),
            Some("amqp:modified:list") => Some(DESC_MODIFIED),
            Some("amqp:declared:list") => Some(DESC_DECLARED),
            Some("amqp:transactional-state:list") => Some(DESC_TRANSACTIONAL_STATE),
            _ => None,
        });
        let fields = |_: ()| match &d.value {
            Value::List(items) => Fields(items),
            _ => Fields(&[]),
        };
        let f = fields(());
        match code {
            Some(DESC_RECEIVED) => {
                Ok(DeliveryState::Received { section_number: f.u32_or(0, 0)?, section_offset: f.opt_u64(1)?.unwrap_or(0) })
            }
            Some(DESC_ACCEPTED) => Ok(DeliveryState::Accepted),
            Some(DESC_REJECTED) => Ok(DeliveryState::Rejected {
                error: match f.get(0) {
                    Value::Null => None,
                    e => Some(Error::from_value(e)?),
                },
            }),
            Some(DESC_RELEASED) => Ok(DeliveryState::Released),
            Some(DESC_MODIFIED) => Ok(DeliveryState::Modified {
                delivery_failed: f.bool_or(0, false)?,
                undeliverable_here: f.bool_or(1, false)?,
                message_annotations: f.opt_value(2),
            }),
            Some(DESC_DECLARED) => Ok(DeliveryState::Declared { txn_id: f.opt_binary(0)?.unwrap_or_default() }),
            Some(DESC_TRANSACTIONAL_STATE) => Ok(DeliveryState::Transactional {
                txn_id: f.opt_binary(0)?.unwrap_or_default(),
                outcome: match f.get(1) {
                    Value::Null => None,
                    o => Some(Box::new(DeliveryState::from_value(o)?)),
                },
            }),
            _ => Err(DecodeError::Invalid("unknown delivery state")),
        }
    }

    /// True for terminal outcomes (not `Received`).
    pub fn is_terminal(&self) -> bool {
        !matches!(self, DeliveryState::Received { .. })
    }
}

/// Body of a message sent to a coordinator link: `declare` or `discharge`.
#[derive(Debug, Clone, PartialEq)]
pub enum TxnControl {
    Declare { global_id: Option<Value> },
    Discharge { txn_id: Vec<u8>, fail: bool },
}

impl TxnControl {
    pub fn to_value(&self) -> Value {
        match self {
            TxnControl::Declare { global_id } => described(DESC_DECLARE, vec![global_id.clone().unwrap_or(Value::Null)]),
            TxnControl::Discharge { txn_id, fail } => {
                described(DESC_DISCHARGE, vec![Value::Binary(txn_id.clone()), if *fail { Value::Bool(true) } else { Value::Null }])
            }
        }
    }
    pub fn from_value(v: &Value) -> Result<TxnControl, DecodeError> {
        match v.descriptor_code() {
            Some(DESC_DECLARE) => {
                let f = Fields::of(v, DESC_DECLARE, "amqp:declare:list")?;
                Ok(TxnControl::Declare { global_id: f.opt_value(0) })
            }
            Some(DESC_DISCHARGE) => {
                let f = Fields::of(v, DESC_DISCHARGE, "amqp:discharge:list")?;
                Ok(TxnControl::Discharge { txn_id: f.opt_binary(0)?.unwrap_or_default(), fail: f.bool_or(1, false)? })
            }
            _ => Err(DecodeError::Invalid("not a declare or discharge")),
        }
    }
}
