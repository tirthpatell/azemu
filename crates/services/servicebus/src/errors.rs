//! Service Bus error conditions and the messages the SDKs expect.
//!
//! Condition symbols come from the Python SDK (`_common/constants.py`, `exceptions.py`,
//! `_transport/_pyamqp_transport.py`). Descriptions follow what the real service sends.

use azemu_amqp::{condition, Error as AmqpError};

pub const ENTITY_NOT_FOUND: &str = condition::NOT_FOUND; // "amqp:not-found"
pub const MESSAGE_LOCK_LOST: &str = "com.microsoft:message-lock-lost";
pub const SESSION_LOCK_LOST: &str = "com.microsoft:session-lock-lost";
pub const SESSION_CANNOT_BE_LOCKED: &str = "com.microsoft:session-cannot-be-locked";
pub const MESSAGE_NOT_FOUND: &str = "com.microsoft:message-not-found";
pub const ARGUMENT_ERROR: &str = "com.microsoft:argument-error";
pub const ARGUMENT_OUT_OF_RANGE: &str = "com.microsoft:argument-out-of-range";
pub const ENTITY_DISABLED: &str = "com.microsoft:entity-disabled";
pub const ENTITY_ALREADY_EXISTS: &str = "com.microsoft:entity-already-exists";
pub const TIMEOUT: &str = "com.microsoft:timeout";
pub const SERVER_BUSY: &str = "com.microsoft:server-busy";
pub const PRECONDITION_FAILED: &str = "com.microsoft:precondition-failed";
pub const DEAD_LETTER: &str = "com.microsoft:dead-letter";

/// An error ready to go on the wire: an AMQP condition and description, and for management
/// responses the HTTP-like status code Service Bus uses.
#[derive(Debug, Clone, PartialEq)]
pub struct SbError {
    pub condition: &'static str,
    pub description: String,
    pub status: i32,
}

impl SbError {
    pub fn new(condition: &'static str, status: i32, description: impl Into<String>) -> SbError {
        SbError { condition, description: description.into(), status }
    }

    pub fn to_amqp(&self) -> AmqpError {
        AmqpError::new(self.condition, self.description.clone())
    }

    pub fn entity_not_found(path: &str, ns: &str) -> SbError {
        SbError::new(
            ENTITY_NOT_FOUND,
            404,
            format!(
                "The messaging entity '{}' could not be found. To know more visit https://aka.ms/sbResourceMgrExceptions.  TrackingId:{}, SystemTracker:{}:{}, Timestamp:{}",
                entity_uri(ns, path),
                tracking_id(),
                ns,
                path,
                timestamp_now()
            ),
        )
    }

    pub fn message_lock_lost() -> SbError {
        SbError::new(
            MESSAGE_LOCK_LOST,
            410,
            "The lock supplied is invalid. Either the lock expired, or the message has already been removed from the queue. For more information please see https://aka.ms/ServiceBusMessageLockLost",
        )
    }

    /// For an operation on a session: a lost message lock there is a lost session lock (the session
    /// lock is what holds the messages of a session receiver). Every other error stays as it is.
    pub fn in_session(self) -> SbError {
        if self.condition == MESSAGE_LOCK_LOST {
            SbError::session_lock_lost()
        } else {
            self
        }
    }

    pub fn session_lock_lost() -> SbError {
        SbError::new(SESSION_LOCK_LOST, 410, "The session lock has expired on the MessageSession. Accept a new MessageSession.")
    }

    pub fn message_not_found(seq: i64) -> SbError {
        SbError::new(MESSAGE_NOT_FOUND, 404, format!("The message with sequence number {seq} was not found."))
    }

    pub fn argument(description: impl Into<String>) -> SbError {
        SbError::new(ARGUMENT_ERROR, 400, description)
    }

    pub fn not_allowed(description: impl Into<String>) -> SbError {
        SbError::new(condition::NOT_ALLOWED, 400, description)
    }

    pub fn not_implemented(what: &str) -> SbError {
        SbError::new(condition::NOT_IMPLEMENTED, 501, format!("servicebus: {what} is not implemented by azemu"))
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
