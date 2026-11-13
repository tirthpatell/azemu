//! Helpers for messages inside the engine: ids, ttl, annotations, and how a stored message is
//! rendered for a receiver or a peek.

use azemu_amqp::message::{Header, Message, Properties};
use azemu_amqp::types::Value;

use crate::core::*;
use crate::store::*;

// ---- helpers -------------------------------------------------------------------------------

pub fn message_id_string(m: &Message) -> Option<String> {
    let id = m.properties.as_ref()?.message_id.as_ref()?;
    Some(match id {
        Value::String(s) | Value::Symbol(s) => s.clone(),
        Value::Uuid(u) => uuid::Uuid::from_bytes(*u).to_string(),
        Value::Binary(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
        other => other.as_u64().map(|x| x.to_string())?,
    })
}

/// min(message ttl, entity default), None when neither is set.
pub(crate) fn effective_ttl(m: &Message, default_ttl: Option<i64>) -> Option<i64> {
    let msg_ttl = m.header.as_ref().and_then(|h| h.ttl).filter(|t| *t > 0).map(|t| t as i64);
    match (msg_ttl, default_ttl) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

pub(crate) fn strip_server_annotations(m: &mut Message) {
    if let Some(a) = &mut m.message_annotations {
        a.retain(|(k, _)| {
            !matches!(
                k.as_str(),
                Some(ANN_ENQUEUED_TIME)
                    | Some(ANN_SEQUENCE_NUMBER)
                    | Some(ANN_LOCKED_UNTIL)
                    | Some(ANN_LOCK_TOKEN)
                    | Some(ANN_ENQUEUE_SEQUENCE_NUMBER)
                    | Some(ANN_MESSAGE_STATE)
                    | Some(ANN_DEAD_LETTER_SOURCE)
            )
        });
    }
}

pub fn set_prop(props: &mut Vec<(Value, Value)>, key: &str, value: Value) {
    match props.iter_mut().find(|(k, _)| k.as_str() == Some(key)) {
        Some(slot) => slot.1 = value,
        None => props.push((Value::String(key.to_string()), value)),
    }
}

/// `properties-to-modify` of an abandon / defer / dead-letter: merged into the application properties.
pub(crate) fn apply_modifications(m: &mut Message, modify: Option<Vec<(Value, Value)>>) {
    let Some(modify) = modify else { return };
    if modify.is_empty() {
        return;
    }
    let props = m.application_properties.get_or_insert_with(Vec::new);
    for (k, v) in modify {
        if let Some(key) = k.as_str() {
            set_prop(props, key, v);
        }
    }
}

/// `x-opt-message-state` of a message: 0 active, 1 deferred, 2 scheduled.
pub fn state_code(m: &Msg) -> i32 {
    match m.state {
        MsgState::Deferred => 1,
        MsgState::Scheduled => 2,
        _ => 0,
    }
}

/// The message as it is handed to a receiver or a peek: the stored message plus the
/// annotations and header Service Bus sets. `state_annotation` adds `x-opt-message-state`.
pub fn render(m: &Msg, state_annotation: Option<i32>) -> Message {
    let mut out = (*m.message).clone();
    let header = out.header.get_or_insert_with(Header::default);
    header.delivery_count = Some(m.delivery_count);
    if header.ttl.is_none() {
        if let Some(t) = m.ttl_ms {
            header.ttl = u32::try_from(t).ok();
        }
    }
    let props = out.properties.get_or_insert_with(Properties::default);
    if props.creation_time.is_none() {
        props.creation_time = Some(m.enqueued_ms);
    }
    if props.absolute_expiry_time.is_none() {
        props.absolute_expiry_time =
            Some(m.expires_ms.or_else(|| m.ttl_ms.map(|t| m.enqueued_ms.saturating_add(t))).unwrap_or(MAX_EXPIRY_MS));
    }
    out.set_annotation(ANN_ENQUEUED_TIME, Value::Timestamp(m.enqueued_ms));
    out.set_annotation(ANN_SEQUENCE_NUMBER, Value::Long(m.seq as i64));
    // 0 for a message sent straight to this entity, the first sequence number for a forwarded one
    out.set_annotation(ANN_ENQUEUE_SEQUENCE_NUMBER, Value::Long(m.enq_seq as i64));
    if let Some(l) = &m.lock {
        if !l.via_session {
            out.set_annotation(ANN_LOCKED_UNTIL, Value::Timestamp(l.until_ms));
        }
    }
    if let Some(src) = &m.dead_letter_source {
        out.set_annotation(ANN_DEAD_LETTER_SOURCE, Value::String(src.clone()));
    }
    if let Some(code) = state_annotation {
        out.set_annotation(ANN_MESSAGE_STATE, Value::Int(code));
    }
    out
}
