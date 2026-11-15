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

    /// A queue, topic or subscription that does not exist. A subscription is named the way the service names it
    /// (`<namespace>:Topic:<topic>|<subscription>`), a queue by its address.
    pub fn entity_not_found(path: &str, ns: &str) -> SbError {
        let name = match subscription_parts(path) {
            Some((topic, sub)) => format!("{ns}:Topic:{topic}|{sub}"),
            None => entity_uri(ns, path),
        };
        SbError::new(
            ENTITY_NOT_FOUND,
            404,
            format!(
                "The messaging entity '{name}' could not be found. To know more visit https://aka.ms/sbResourceMgrExceptions.  TrackingId:{}, SystemTracker:{}:{}, Timestamp:{}",
                tracking_id(),
                ns,
                path,
                timestamp_now()
            ),
        )
    }

    /// The same, always named by its address: what the `$cbs` `put-token` for the address of a missing entity answers.
    pub fn entity_address_not_found(path: &str, ns: &str) -> SbError {
        SbError::new(
            ENTITY_NOT_FOUND,
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

    /// An entity that Status `Disabled`, `SendDisabled` or `ReceiveDisabled` refuses an operation on.
    pub fn entity_disabled(ns: &str, path: &str) -> SbError {
        let ns = if ns.is_empty() { "default" } else { ns };
        let name = match subscription_parts(path) {
            Some((topic, sub)) => format!("{ns}:Topic:{topic}|{sub}"),
            None => entity_uri(ns, path),
        };
        SbError::new(
            ENTITY_DISABLED,
            400,
            format!(
                "Messaging entity '{name}' is currently disabled. For more information please see https://aka.ms/ServiceBusExceptions . TrackingId:{}, SystemTracker:{ns}.servicebus.windows.net:{path}, Timestamp:{}",
                tracking_id(),
                timestamp_now()
            ),
        )
    }

    pub fn message_lock_lost() -> SbError {
        SbError::new(
            MESSAGE_LOCK_LOST,
            410,
            format!(
                "The lock supplied is invalid. Either the lock expired, or the message has already been removed from the queue. For more information please see https://aka.ms/ServiceBusExceptions . Reference:{}, TrackingId:{}, SystemTracker:NoSystemTracker, Timestamp:{}",
                tracking_id(),
                tracking_id(),
                timestamp_now()
            ),
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

    /// `receive-by-sequence-number` for a sequence number no message ever had.
    pub fn messages_not_found() -> SbError {
        SbError::new(
            MESSAGE_NOT_FOUND,
            500,
            format!(
                "Messages not found. One or more sequence numbers missing. TrackingId:{}, SystemTracker:NoSystemTracker, Timestamp:{}",
                tracking_id(),
                timestamp_now()
            ),
        )
    }

    /// `receive-by-sequence-number` for a deferred message that another receiver holds.
    pub fn deferred_message_locked() -> SbError {
        SbError::new(
            MESSAGE_NOT_FOUND,
            500,
            format!(
                "Failed to lock one or more specified messages. Either the message is already locked or does not exist. TrackingId:{}, SystemTracker:NoSystemTracker, Timestamp:{}",
                tracking_id(),
                timestamp_now()
            ),
        )
    }

    /// `receive-by-sequence-number` for a message that exists (or did) and is not deferred.
    pub fn message_not_deferred() -> SbError {
        SbError::new(
            MESSAGE_NOT_FOUND,
            500,
            format!(
                "Failed to lock one or more specified messages as they were not deferred messages. TrackingId:{}, SystemTracker:NoSystemTracker, Timestamp:{}",
                tracking_id(),
                timestamp_now()
            ),
        )
    }

    /// `cancel-scheduled-message` for a sequence number that was never a scheduled message (the text is the service's).
    pub fn scheduled_message_unknown(seq: i64) -> SbError {
        SbError::new(
            condition::NOT_ALLOWED,
            400,
            format!(
                "The scheduled message with SequenceNumber = {seq} is already being cancelled. TrackingId:{}, SystemTracker:NoSystemTracker, Timestamp:{}",
                tracking_id(),
                timestamp_now()
            ),
        )
    }

    /// A message sent with a time to live of zero.
    pub fn zero_time_to_live() -> SbError {
        SbError::new(
            ARGUMENT_OUT_OF_RANGE,
            400,
            "Argument TimeToLIve must be a positive timeout value. The provided value was 00:00:00.\r\nParameter name: TimeToLIve\r\nActual value was 00:00:00.",
        )
    }

    /// A receiver on an entity that forwards its messages.
    pub fn forwarding_receiver(ns: &str, path: &str) -> SbError {
        let ns = if ns.is_empty() { "default" } else { ns };
        SbError::not_allowed(format!(
            "Cannot create a message receiver on an entity with auto-forwarding enabled. TrackingId:{}, SystemTracker:{ns}.servicebus.windows.net:{path}, Timestamp:{}",
            tracking_id(),
            timestamp_now()
        ))
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

/// `topic/Subscriptions/sub` (with or without a dead-letter sub-queue after it) as `(topic, sub)`.
fn subscription_parts(path: &str) -> Option<(&str, &str)> {
    let mut it = path.split('/');
    let (topic, word, sub) = (it.next()?, it.next()?, it.next()?);
    word.eq_ignore_ascii_case("Subscriptions").then_some((topic, sub))
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
