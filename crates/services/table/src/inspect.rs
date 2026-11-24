//! The inspector of the table service: a read-only look at accounts, tables and entities with every property and its type.
//!
//! The state is only read: no account is made because someone asked for its path (the request path of the service itself makes
//! one), no ETag or timestamp moves and the save generation stays where it was.

use std::ops::Bound;

use azemu_core::inspect::{marker_for, InspectError, InspectRequest, InspectResult, Node};
use azemu_core::serde_json::{json, Map, Value as Json};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::state::{key, Account, Entity, State, Table};
use crate::values::{format_ticks, Value};

/// A marker for an entity: the partition key and the row key, which cannot contain a NUL character.
const SEP: char = '\0';

pub fn inspect(state: &State, req: &InspectRequest) -> InspectResult {
    match req.segments.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => Ok(service_node(state, req)),
        [account] => Ok(account_node(find_account(state, account)?, account, req)),
        [account, "tables"] => Ok(tables_node(find_account(state, account)?, account, req)),
        [account, "tables", table] => Ok(table_node(find_table(state, account, table)?, account, table, req)),
        [account, "tables", table, "entities"] => entities_node(find_table(state, account, table)?, account, table, req),
        _ => Err(InspectError::not_found("There is nothing at this path.")),
    }
}

fn find_account<'a>(state: &'a State, name: &str) -> Result<&'a Account, InspectError> {
    state.accounts.get(name).ok_or_else(|| InspectError::not_found(format!("There is no storage account '{name}' with tables.")))
}

/// Tables are found by their lower-case name, as the service does.
fn find_table<'a>(state: &'a State, account: &str, table: &str) -> Result<&'a Table, InspectError> {
    find_account(state, account)?
        .tables
        .get(&table.to_ascii_lowercase())
        .ok_or_else(|| InspectError::not_found(format!("The table '{table}' does not exist in account '{account}'.")))
}

fn service_node(state: &State, req: &InspectRequest) -> Node {
    let mut names: Vec<&String> = state.accounts.keys().collect();
    names.sort();
    let after = req.marker();
    let mut page: Vec<&String> = names.into_iter().filter(|n| after.is_none_or(|m| n.as_str() > m)).collect();
    let more = page.len() > req.limit();
    page.truncate(req.limit());
    let next = if more { page.last().map(|n| marker_for(n)) } else { None };
    let items = page.iter().map(|n| json!({"name": n, "path": req.path(&[n]), "tables": state.accounts[*n].tables.len()})).collect();
    Node::new(req.path(&[]), "service", "table").prop("accounts", state.accounts.len()).with_items(items, next)
}

fn account_node(account: &Account, name: &str, req: &InspectRequest) -> Node {
    Node::new(req.path(&[name]), "account", name).prop("tables", account.tables.len()).prop("corsRules", account.props.cors.len()).child(
        "tables",
        "tables",
        Some(account.tables.len() as u64),
        req.path(&[name, "tables"]),
    )
}

fn tables_node(account: &Account, name: &str, req: &InspectRequest) -> Node {
    let (rows, next) = req.page_map(&account.tables);
    let items = rows
        .into_iter()
        .filter(|(_, t)| req.matches_filter("prefix", &t.name))
        .map(|(k, t)| {
            json!({
                "name": t.name,
                "path": req.path(&[name, "tables", k]),
                "entities": t.entities.len(),
                "accessPolicies": t.acl.len(),
            })
        })
        .collect();
    Node::new(req.path(&[name, "tables"]), "tables", "tables")
        .accepts(&["prefix"])
        .prop("count", account.tables.len())
        .with_items(items, next)
}

fn table_node(table: &Table, account: &str, key_name: &str, req: &InspectRequest) -> Node {
    let key_name = key_name.to_ascii_lowercase();
    let acl: Vec<Json> = table
        .acl
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "start": s.policy.as_ref().and_then(|p| p.start.as_deref()),
                "expiry": s.policy.as_ref().and_then(|p| p.expiry.as_deref()),
                "permission": s.policy.as_ref().and_then(|p| p.permission.as_deref()),
                "hasPolicy": s.policy.is_some(),
            })
        })
        .collect();
    Node::new(req.path(&[account, "tables", &key_name]), "table", &table.name)
        .prop("name", &table.name)
        .prop("entities", table.entities.len())
        .prop("accessPolicies", acl)
        .child("entities", "entities", Some(table.entities.len() as u64), req.path(&[account, "tables", &key_name, "entities"]))
}

/// A property as the page shows it: its Edm type and its value. Big integers are text (a JSON number loses digits above 2^53). A string or binary
/// value longer than `room` bytes is cut: `{"type", "value": <the first of it>, "cut": true, "bytes": <the whole length>}`, so that a page of
/// big entities is not copied whole while the table is locked.
fn typed(v: &Value, room: usize) -> Json {
    match v {
        Value::Str(s) if s.len() > room => {
            let mut end = room;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            json!({"type": "Edm.String", "value": &s[..end], "cut": true, "bytes": s.len()})
        }
        Value::Str(s) => json!({"type": "Edm.String", "value": s}),
        Value::Int32(i) => json!({"type": "Edm.Int32", "value": i}),
        Value::Int64(i) => json!({"type": "Edm.Int64", "value": i.to_string()}),
        Value::Double(d) if d.is_finite() => json!({"type": "Edm.Double", "value": d}),
        Value::Double(d) => {
            json!({"type": "Edm.Double", "value": if d.is_nan() { "NaN" } else if *d > 0.0 { "Infinity" } else { "-Infinity" }})
        }
        Value::Bool(b) => json!({"type": "Edm.Boolean", "value": b}),
        Value::DateTime(t) => json!({"type": "Edm.DateTime", "value": format_ticks(*t)}),
        Value::Guid(g) => json!({"type": "Edm.Guid", "value": g}),
        // base64 takes 4 bytes for 3: the room is for the text
        Value::Binary(b) if b.len() > room / 4 * 3 => {
            json!({"type": "Edm.Binary", "value": B64.encode(&b[..room / 4 * 3]), "cut": true, "bytes": b.len()})
        }
        Value::Binary(b) => json!({"type": "Edm.Binary", "value": B64.encode(b)}),
    }
}

/// The room the properties of one row share, and the most one value takes of it: a row has at most 252 properties of up to 64 KiB each.
const ROW_ROOM: usize = azemu_core::inspect::ROW_LIMIT / 2;
const VALUE_ROOM: usize = 8 * 1024;

fn entity_row(e: &Entity) -> Json {
    let mut room = ROW_ROOM;
    let props: Map<String, Json> = e
        .props
        .iter()
        .map(|(k, v)| {
            let t = typed(v, room.min(VALUE_ROOM));
            room = room.saturating_sub(k.len() + t["value"].as_str().map_or(16, str::len));
            (k.clone(), t)
        })
        .collect();
    json!({
        "partitionKey": e.pk,
        "rowKey": e.rk,
        "timestamp": format_ticks(e.ts),
        "etag": e.etag(),
        "size": e.size(),
        "properties": props,
    })
}

/// Entities in key order (PartitionKey, then RowKey, by UTF-16 units as the service orders them), after the marker, and only the
/// partition `?partition=` names when it is given. The page is read straight from the tree: only `limit + 1` entities are visited.
fn entities_node(table: &Table, account: &str, name: &str, req: &InspectRequest) -> InspectResult {
    let partition = req.param("partition");
    let from_marker = match req.marker() {
        Some(m) => {
            let (pk, rk) = m.split_once(SEP).ok_or_else(|| InspectError::bad_request("The marker is not one this node gave out."))?;
            Some(key(pk, rk))
        }
        None => None,
    };
    let lower = match (from_marker, partition) {
        (Some(m), Some(p)) if m >= key(p, "") => Bound::Excluded(m),
        (_, Some(p)) => Bound::Included(key(p, "")),
        (Some(m), None) => Bound::Excluded(m),
        (None, None) => Bound::Unbounded,
    };
    let rows = table
        .entities
        .range((lower, Bound::Unbounded))
        .take_while(|(_, e)| partition.is_none_or(|p| e.pk == p))
        .map(|(_, e)| (format!("{}{SEP}{}", e.pk, e.rk), e));
    let (page, next) = req.page_keyed(rows);
    let items = page.into_iter().map(entity_row).collect();
    let path = req.path(&[account, "tables", &name.to_ascii_lowercase(), "entities"]);
    Ok(Node::new(path, "entities", "entities").accepts(&["partition"]).prop("count", table.entities.len()).with_items(items, next))
}
