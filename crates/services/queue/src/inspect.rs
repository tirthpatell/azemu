//! The inspector of the queue service: a read-only look at accounts, queues and every message, hidden ones included.
//!
//! It never goes through `Queue::dequeue`, `peek`, `approximate_count` or `purge_expired`: those hide, count or drop messages.
//! "Is this message expired / visible" is worked out here from the stored times, and the state is only read, so looking changes no
//! dequeue count, no visibility time and no counter that a save watches. An account that does not exist is a 404 and is not made.

use azemu_core::inspect::{marker_for, text_with_decoded_base64, time_value, InspectError, InspectRequest, InspectResult, Node};
use azemu_core::serde_json::{json, Map, Value};
use azemu_core::time::OffsetDateTime;

use crate::state::{Account, Message, Queue, State};

pub fn inspect(state: &State, req: &InspectRequest) -> InspectResult {
    let now = OffsetDateTime::now_utc();
    match req.segments.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => Ok(service_node(state, req)),
        [account] => Ok(account_node(find_account(state, account)?, account, req)),
        [account, "queues"] => queues_node(find_account(state, account)?, account, req, now),
        [account, "queues", queue] => {
            let q = find_queue(find_account(state, account)?, account, queue)?;
            Ok(queue_node(q, account, queue, req, now))
        }
        [account, "queues", queue, "messages"] => {
            let q = find_queue(find_account(state, account)?, account, queue)?;
            Ok(messages_node(q, account, queue, req, now))
        }
        _ => Err(InspectError::not_found("There is nothing at this path.")),
    }
}

fn find_account<'a>(state: &'a State, name: &str) -> Result<&'a Account, InspectError> {
    state.accounts.get(name).ok_or_else(|| InspectError::not_found(format!("There is no storage account '{name}' with queues.")))
}

fn find_queue<'a>(account: &'a Account, account_name: &str, name: &str) -> Result<&'a Queue, InspectError> {
    account
        .queues
        .get(name)
        .ok_or_else(|| InspectError::not_found(format!("The queue '{name}' does not exist in account '{account_name}'.")))
}

/// What the SDK sees: expired messages are gone (the service drops them lazily), the rest are counted, visible or hidden.
struct Counts {
    total: u64,
    visible: u64,
    hidden: u64,
}

fn counts(q: &Queue, now: OffsetDateTime) -> Counts {
    let mut c = Counts { total: 0, visible: 0, hidden: 0 };
    for m in q.messages.iter().filter(|m| !m.is_expired(now)) {
        c.total += 1;
        if m.is_visible(now) {
            c.visible += 1;
        } else {
            c.hidden += 1;
        }
    }
    c
}

fn service_node(state: &State, req: &InspectRequest) -> Node {
    let mut names: Vec<&String> = state.accounts.keys().collect();
    names.sort();
    let after = req.marker();
    let mut page: Vec<&String> = names.iter().copied().filter(|n| after.is_none_or(|m| n.as_str() > m)).collect();
    let more = page.len() > req.limit();
    page.truncate(req.limit());
    let next = if more { page.last().map(|n| marker_for(n)) } else { None };
    let items = page.iter().map(|n| json!({"name": n, "path": req.path(&[n]), "queues": state.accounts[*n].queues.len()})).collect();
    Node::new(req.path(&[]), "service", "queue").prop("accounts", state.accounts.len()).with_items(items, next)
}

fn account_node(account: &Account, name: &str, req: &InspectRequest) -> Node {
    Node::new(req.path(&[name]), "account", name)
        .prop("queues", account.queues.len())
        .prop("corsRules", account.props.cors.len())
        .prop("loggingRead", account.props.logging.read)
        .prop("loggingWrite", account.props.logging.write)
        .prop("loggingDelete", account.props.logging.delete)
        .prop("hourMetricsEnabled", account.props.hour.enabled)
        .prop("minuteMetricsEnabled", account.props.minute.enabled)
        .child("queues", "queues", Some(account.queues.len() as u64), req.path(&[name, "queues"]))
}

fn queues_node(account: &Account, name: &str, req: &InspectRequest, now: OffsetDateTime) -> InspectResult {
    let (rows, next) = req.page_map(&account.queues);
    let items = rows
        .into_iter()
        .filter(|(n, _)| req.matches_filter("prefix", n))
        .map(|(n, q)| {
            let c = counts(q, now);
            json!({
                "name": n,
                "path": req.path(&[name, "queues", n]),
                "messages": c.total,
                "visible": c.visible,
                "hidden": c.hidden,
                "metadata": q.metadata.len(),
            })
        })
        .collect();
    Ok(Node::new(req.path(&[name, "queues"]), "queues", "queues")
        .accepts(&["prefix"])
        .prop("count", account.queues.len())
        .with_items(items, next))
}

fn metadata_value(q: &Queue) -> Value {
    Value::Object(q.metadata.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<Map<String, Value>>())
}

fn queue_node(q: &Queue, account: &str, name: &str, req: &InspectRequest, now: OffsetDateTime) -> Node {
    let c = counts(q, now);
    let acl: Vec<Value> = q
        .acl
        .iter()
        .map(|s| json!({"id": s.id, "start": s.start, "expiry": s.expiry, "permission": s.permission, "hasPolicy": s.has_policy}))
        .collect();
    Node::new(req.path(&[account, "queues", name]), "queue", name)
        .prop("approximateMessagesCount", c.total)
        .prop("visibleMessages", c.visible)
        .prop("hiddenMessages", c.hidden)
        .prop("metadata", metadata_value(q))
        .prop("accessPolicies", acl)
        .child("messages", "messages", Some(c.total), req.path(&[account, "queues", name, "messages"]))
}

fn message_row(m: &Message, position: usize, now: OffsetDateTime) -> Value {
    json!({
        "position": position,
        "id": m.id,
        "dequeueCount": m.dequeue_count,
        "insertionTime": time_value(m.inserted),
        "expirationTime": time_value(m.expires),
        "nextVisibleTime": time_value(m.next_visible),
        "visible": m.is_visible(now),
        "body": text_with_decoded_base64(&m.text),
    })
}

/// Messages in insertion order, expired ones left out, paged by position among the rest. Only the page is copied out of the queue.
fn messages_node(q: &Queue, account: &str, name: &str, req: &InspectRequest, now: OffsetDateTime) -> Node {
    let start = req.start_index();
    let live = || q.messages.iter().filter(|m| !m.is_expired(now));
    let total = live().count();
    let page: Vec<Value> = live().enumerate().skip(start).take(req.limit()).map(|(i, m)| message_row(m, i, now)).collect();
    let end = start + page.len();
    let next = (end < total).then(|| marker_for(&end.to_string()));
    Node::new(req.path(&[account, "queues", name, "messages"]), "messages", "messages").prop("count", total).with_items(page, next)
}
