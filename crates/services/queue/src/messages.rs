//! Message operations: put, get, peek, update, delete and clear.

use azemu_core::resp::Resp;
use azemu_core::time::rfc1123;
use azemu_core::{Request, Response};

use crate::errors;
use crate::state::{Message, MessageError, Queue, State, DEFAULT_TTL_SECS, MAX_MESSAGE_BYTES, MAX_VISIBILITY_SECS};
use crate::xml;

/// The largest request body that can still hold a 64 KiB message once escaped
/// (`&amp;` is 5 bytes for 1). Bigger bodies are refused before they are parsed.
const MAX_BODY_BYTES: usize = MAX_MESSAGE_BYTES * 6 + 1024;

fn queue_mut<'a>(state: &'a mut State, req: &Request, name: &str) -> Result<&'a mut Queue, Response> {
    state.accounts.entry(req.account.clone()).or_default().queues.get_mut(name).ok_or_else(|| errors::queue_not_found(&req.ctx))
}

/// An integer query parameter with limits. `Ok(None)` when absent.
fn int_param(req: &Request, name: &str, min: i64, max: i64) -> Result<Option<i64>, Response> {
    let Some(raw) = req.query.get(name) else {
        return Ok(None);
    };
    match raw.trim().parse::<i64>() {
        Ok(v) if (min..=max).contains(&v) => Ok(Some(v)),
        Ok(_) => Err(errors::out_of_range_query(&req.ctx, name, raw, min, max)),
        Err(_) => Err(errors::invalid_query(&req.ctx, name, raw, "")),
    }
}

/// The text of a `<QueueMessage>` body.
fn read_message_text(req: &Request) -> Result<String, Response> {
    if req.body.len() > MAX_BODY_BYTES {
        return Err(errors::body_too_large(&req.ctx, MAX_MESSAGE_BYTES));
    }
    let Ok(body) = std::str::from_utf8(&req.body) else {
        return Err(errors::invalid_xml_no_pos(&req.ctx, "Error parsing Xml content"));
    };
    let root = xml::parse(body).map_err(|e| errors::invalid_xml(&req.ctx, e.pos, &e.reason))?;
    if root.name != "QueueMessage" {
        return Err(errors::invalid_xml(&req.ctx, root.end, "Error parsing Xml content"));
    }
    let Some(text) = root.child_text("MessageText") else {
        return Err(errors::invalid_xml(&req.ctx, root.end, "Error parsing Xml content"));
    };
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(errors::body_too_large(&req.ctx, MAX_MESSAGE_BYTES));
    }
    Ok(text.to_string())
}

fn message_xml(m: &Message, kind: Kind) -> String {
    let mut s = String::from("<QueueMessage>");
    s.push_str(&xml::element("MessageId", &m.id));
    s.push_str(&xml::element("InsertionTime", &rfc1123(m.inserted)));
    s.push_str(&xml::element("ExpirationTime", &rfc1123(m.expires)));
    match kind {
        Kind::Put => {
            s.push_str(&xml::element("PopReceipt", &m.pop_receipt));
            s.push_str(&xml::element("TimeNextVisible", &rfc1123(m.next_visible)));
        }
        Kind::Get => {
            s.push_str(&xml::element("PopReceipt", &m.pop_receipt));
            s.push_str(&xml::element("TimeNextVisible", &rfc1123(m.next_visible)));
            s.push_str(&format!("<DequeueCount>{}</DequeueCount>", m.dequeue_count));
            s.push_str(&text_element(&m.text));
        }
        Kind::Peek => {
            s.push_str(&format!("<DequeueCount>{}</DequeueCount>", m.dequeue_count));
            s.push_str(&text_element(&m.text));
        }
    }
    s.push_str("</QueueMessage>");
    s
}

fn text_element(text: &str) -> String {
    if text.is_empty() {
        "<MessageText />".to_string()
    } else {
        format!("<MessageText>{}</MessageText>", xml::text(text))
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Put,
    Get,
    Peek,
}

fn list_xml(items: &[Message], kind: Kind) -> String {
    if items.is_empty() {
        return "<QueueMessagesList />".to_string();
    }
    let mut s = String::from("<QueueMessagesList>");
    for m in items {
        s.push_str(&message_xml(m, kind));
    }
    s.push_str("</QueueMessagesList>");
    s
}

pub fn put(state: &mut State, req: &Request, name: &str) -> Response {
    let visibility = match int_param(req, "visibilitytimeout", 0, MAX_VISIBILITY_SECS) {
        Ok(v) => v.unwrap_or(0),
        Err(r) => return r,
    };
    let ttl = match req.query.get("messagettl") {
        None => DEFAULT_TTL_SECS,
        Some(raw) => match raw.trim().parse::<i64>() {
            Ok(v) if v == -1 || v >= 1 => v,
            Ok(_) => return errors::invalid_query(&req.ctx, "messagettl", raw, "Message time to live must be -1 or greater than zero."),
            Err(_) => return errors::invalid_query(&req.ctx, "messagettl", raw, ""),
        },
    };
    if ttl > 0 && visibility >= ttl {
        return errors::invalid_query(
            &req.ctx,
            "visibilitytimeout",
            &visibility.to_string(),
            "The visibility timeout must be smaller than the message time to live.",
        );
    }
    let text = match read_message_text(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let q = match queue_mut(state, req, name) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let msg = q.put(text, visibility, ttl, req.ctx.time);
    Resp::new(201u16).xml(xml::document(&list_xml(&[msg], Kind::Put))).build()
}

pub fn get(state: &mut State, req: &Request, name: &str) -> Response {
    let count = match int_param(req, "numofmessages", 1, 32) {
        Ok(v) => v.unwrap_or(1),
        Err(r) => return r,
    };
    let visibility = match int_param(req, "visibilitytimeout", 1, MAX_VISIBILITY_SECS) {
        Ok(v) => v.unwrap_or(30),
        Err(r) => return r,
    };
    let q = match queue_mut(state, req, name) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let got = q.dequeue(count as usize, visibility, req.ctx.time);
    Resp::new(200u16).xml(xml::document(&list_xml(&got, Kind::Get))).header("Cache-Control", "no-cache").build()
}

pub fn peek(state: &mut State, req: &Request, name: &str) -> Response {
    let count = match int_param(req, "numofmessages", 1, 32) {
        Ok(v) => v.unwrap_or(1),
        Err(r) => return r,
    };
    let q = match queue_mut(state, req, name) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let got = q.peek(count as usize, req.ctx.time);
    Resp::new(200u16).xml(xml::document(&list_xml(&got, Kind::Peek))).header("Cache-Control", "no-cache").build()
}

pub fn clear(state: &mut State, req: &Request, name: &str) -> Response {
    let q = match queue_mut(state, req, name) {
        Ok(q) => q,
        Err(r) => return r,
    };
    q.clear();
    Resp::new(204u16).build()
}

fn message_error(req: &Request, e: MessageError) -> Response {
    match e {
        MessageError::NotFound => errors::message_not_found(&req.ctx),
        MessageError::PopReceiptMismatch => errors::pop_receipt_mismatch(&req.ctx),
    }
}

fn required<'a>(req: &'a Request, name: &str) -> Result<&'a str, Response> {
    match req.query.get(name) {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(errors::missing_query(&req.ctx, name)),
    }
}

pub fn update(state: &mut State, req: &Request, name: &str, id: &str) -> Response {
    let receipt = match required(req, "popreceipt") {
        Ok(r) => r,
        Err(r) => return r,
    };
    let visibility = match required(req, "visibilitytimeout").and_then(|_| int_param(req, "visibilitytimeout", 0, MAX_VISIBILITY_SECS)) {
        Ok(v) => v.unwrap_or(0),
        Err(r) => return r,
    };
    let text = if req.body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        match read_message_text(req) {
            Ok(t) => Some(t),
            Err(r) => return r,
        }
    };
    let q = match queue_mut(state, req, name) {
        Ok(q) => q,
        Err(r) => return r,
    };
    match q.update(id, receipt, text, visibility, req.ctx.time) {
        Ok((new_receipt, visible)) => {
            Resp::new(204u16).header("x-ms-popreceipt", new_receipt).header("x-ms-time-next-visible", rfc1123(visible)).build()
        }
        Err(e) => message_error(req, e),
    }
}

pub fn delete(state: &mut State, req: &Request, name: &str, id: &str) -> Response {
    let receipt = match required(req, "popreceipt") {
        Ok(r) => r,
        Err(r) => return r,
    };
    let q = match queue_mut(state, req, name) {
        Ok(q) => q,
        Err(r) => return r,
    };
    match q.delete(id, receipt, req.ctx.time) {
        Ok(()) => Resp::new(204u16).build(),
        Err(e) => message_error(req, e),
    }
}
