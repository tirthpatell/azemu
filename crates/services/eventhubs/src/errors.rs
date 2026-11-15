//! Event Hubs error conditions and the messages the SDKs expect.
//!
//! Condition symbols come from the Python SDK (`_constants.py`, `_pyamqp/error.py`); the
//! descriptions follow what the real service sends.

use azemu_amqp::{condition, Error as AmqpError};

pub const ARGUMENT_ERROR: &str = "com.microsoft:argument-error";
pub const ARGUMENT_OUT_OF_RANGE: &str = "com.microsoft:argument-out-of-range";
pub const ENTITY_ALREADY_EXISTS: &str = "com.microsoft:entity-already-exists";

/// An error ready for the wire: an AMQP condition and description, and for management
/// responses the HTTP-like status code Event Hubs uses.
#[derive(Debug, Clone, PartialEq)]
pub struct EhError {
    pub condition: &'static str,
    pub description: String,
    pub status: i32,
}

impl EhError {
    pub fn new(condition: &'static str, status: i32, description: impl Into<String>) -> EhError {
        EhError { condition, description: description.into(), status }
    }

    pub fn to_amqp(&self) -> AmqpError {
        AmqpError::new(self.condition, self.description.clone())
    }

    pub fn entity_not_found(ns: &str, path: &str) -> EhError {
        EhError::new(
            condition::NOT_FOUND,
            404,
            format!(
                "The messaging entity '{}' could not be found. To know more visit https://aka.ms/sbResourceMgrExceptions.  TrackingId:{}, SystemTracker:{}.servicebus.windows.net:{}, Timestamp:{}",
                entity_uri(ns, path),
                tracking_id(),
                ns,
                path,
                timestamp_now()
            ),
        )
    }

    /// A partition id the event hub does not have (checked in November 2026: the text is the same for `4`, `99`, `-1` and `abc`).
    /// A link (`link`: a sender or receiver attach) gets a second line naming the parameter; `$management` does not.
    pub fn bad_partition(partitions: usize, link: bool) -> EhError {
        let mut text = format!(
            "The specified partition is invalid for an EventHub partition sender or receiver. It should be between 0 and {}.",
            partitions.saturating_sub(1)
        );
        if link {
            text.push_str("\r\nParameter name: PartitionId");
        }
        EhError::new(ARGUMENT_OUT_OF_RANGE, 400, text)
    }

    /// A start position past the end: `what` is "sequence number" or "offset". `tracker` is `{namespace}:eventhub:{hub}~{partition}`.
    pub fn position_past_the_end(what: &str, value: i64, last: i64, tracker: &str) -> EhError {
        EhError::new(
            ARGUMENT_ERROR,
            400,
            format!(
                "The supplied {what} '{value}' is invalid. The last {what} in the system is '{last}' TrackingId:{}, SystemTracker:{tracker}, Timestamp:{}",
                tracking_id(),
                timestamp_now()
            ),
        )
    }

    pub fn argument(description: impl Into<String>) -> EhError {
        EhError::new(ARGUMENT_ERROR, 400, description)
    }

    pub fn out_of_range(description: impl Into<String>) -> EhError {
        EhError::new(ARGUMENT_OUT_OF_RANGE, 400, description)
    }

    pub fn not_allowed(description: impl Into<String>) -> EhError {
        EhError::new(condition::NOT_ALLOWED, 400, description)
    }

    pub fn already_exists(what: &str) -> EhError {
        EhError::new(ENTITY_ALREADY_EXISTS, 409, format!("{what} already exists."))
    }

    pub fn not_implemented(what: &str) -> EhError {
        EhError::new(condition::NOT_IMPLEMENTED, 501, format!("eventhubs: {what} is not implemented by azemu"))
    }

    /// The error for the receiver that loses against another one with an epoch.
    pub fn stolen(description: impl Into<String>) -> EhError {
        EhError::new(condition::LINK_STOLEN, 410, description)
    }
}

pub fn entity_uri(ns: &str, path: &str) -> String {
    format!("sb://{ns}.servicebus.windows.net/{path}")
}

pub fn tracking_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn timestamp_now() -> String {
    azemu_core::time::iso8601_secs(azemu_core::time::OffsetDateTime::now_utc())
}
