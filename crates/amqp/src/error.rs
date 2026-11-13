//! The AMQP `error` composite type and the standard error conditions (part 2, section 2.8.15
//! and the connection/session/link error tables) plus transaction errors.

use crate::codec::DecodeError;
use crate::fields::{opt, Fields};
use crate::types::Value;

pub const DESC_ERROR: u64 = 0x1d;

/// Standard error condition symbols.
pub mod condition {
    pub const INTERNAL_ERROR: &str = "amqp:internal-error";
    pub const NOT_FOUND: &str = "amqp:not-found";
    pub const UNAUTHORIZED_ACCESS: &str = "amqp:unauthorized-access";
    pub const DECODE_ERROR: &str = "amqp:decode-error";
    pub const RESOURCE_LIMIT_EXCEEDED: &str = "amqp:resource-limit-exceeded";
    pub const NOT_ALLOWED: &str = "amqp:not-allowed";
    pub const INVALID_FIELD: &str = "amqp:invalid-field";
    pub const NOT_IMPLEMENTED: &str = "amqp:not-implemented";
    pub const RESOURCE_LOCKED: &str = "amqp:resource-locked";
    pub const PRECONDITION_FAILED: &str = "amqp:precondition-failed";
    pub const RESOURCE_DELETED: &str = "amqp:resource-deleted";
    pub const ILLEGAL_STATE: &str = "amqp:illegal-state";
    pub const FRAME_SIZE_TOO_SMALL: &str = "amqp:frame-size-too-small";

    pub const CONNECTION_FORCED: &str = "amqp:connection:forced";
    pub const CONNECTION_FRAMING_ERROR: &str = "amqp:connection:framing-error";
    pub const CONNECTION_REDIRECT: &str = "amqp:connection:redirect";

    pub const SESSION_WINDOW_VIOLATION: &str = "amqp:session:window-violation";
    pub const SESSION_ERRANT_LINK: &str = "amqp:session:errant-link";
    pub const SESSION_HANDLE_IN_USE: &str = "amqp:session:handle-in-use";
    pub const SESSION_UNATTACHED_HANDLE: &str = "amqp:session:unattached-handle";

    pub const LINK_DETACH_FORCED: &str = "amqp:link:detach-forced";
    pub const LINK_TRANSFER_LIMIT_EXCEEDED: &str = "amqp:link:transfer-limit-exceeded";
    pub const LINK_MESSAGE_SIZE_EXCEEDED: &str = "amqp:link:message-size-exceeded";
    pub const LINK_REDIRECT: &str = "amqp:link:redirect";
    pub const LINK_STOLEN: &str = "amqp:link:stolen";

    pub const TXN_UNKNOWN_ID: &str = "amqp:transaction:unknown-id";
    pub const TXN_ROLLBACK: &str = "amqp:transaction:rollback";
    pub const TXN_TIMEOUT: &str = "amqp:transaction:timeout";
}

/// An AMQP error: condition symbol, description, and an info map.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Error {
    pub condition: String,
    pub description: Option<String>,
    pub info: Option<Vec<(Value, Value)>>,
}

impl Error {
    pub fn new(condition: impl Into<String>, description: impl Into<String>) -> Error {
        Error { condition: condition.into(), description: Some(description.into()), info: None }
    }
    pub fn condition_only(condition: impl Into<String>) -> Error {
        Error { condition: condition.into(), description: None, info: None }
    }
    pub fn with_info(mut self, key: &str, value: Value) -> Error {
        self.info.get_or_insert_with(Vec::new).push((Value::sym(key), value));
        self
    }
    pub fn to_value(&self) -> Value {
        // All three fields are always written (explicit nulls): the Python SDK's pyamqp reads
        // `error[2]` of a rejected outcome without checking the list length.
        Value::described_code(
            DESC_ERROR,
            Value::List(vec![
                Value::Symbol(self.condition.clone()),
                opt(self.description.clone()),
                self.info.clone().map(Value::Map).unwrap_or(Value::Null),
            ]),
        )
    }
    pub fn from_value(v: &Value) -> Result<Error, DecodeError> {
        let f = Fields::of(v, DESC_ERROR, "amqp:error:list")?;
        Ok(Error { condition: f.req_string(0)?, description: f.opt_string(1)?, info: f.get(2).as_map().map(|m| m.to_vec()) })
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.description {
            Some(d) => write!(f, "{}: {}", self.condition, d),
            None => write!(f, "{}", self.condition),
        }
    }
}
impl std::error::Error for Error {}
