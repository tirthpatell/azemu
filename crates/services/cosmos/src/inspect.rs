//! The inspector of Cosmos DB (`docs/inspector.md`): accounts, databases, containers with their definitions and counts, documents,
//! stored procedures, triggers and user-defined functions. Only reads: no account is made by looking, no document that has passed its
//! time to live is removed (it is left out of the lists instead), and no change feed position, log or ETag moves.

use std::ops::Bound;

use azemu_core::inspect::{InspectError, InspectRequest, InspectResult, Node};
use azemu_core::time::OffsetDateTime;
use serde_json::{json, Map, Value};

use crate::pk::{DocKey, PkComp};
use crate::state::{Account, Container, Database, Doc, Resource, State};

/// A document bigger than this is shown by its size only in a list; its own node has all of it.
const ROW_DOC_LIMIT: usize = 16 * 1024;

pub(crate) fn node(state: &State, req: &InspectRequest) -> InspectResult {
    let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let missing = || InspectError::not_found(format!("There is no node {}.", req.own_path()));
    let [acct_name, rest @ ..] = segs.as_slice() else {
        return Ok(service_node(state, req));
    };
    let acct =
        state.accounts.get(*acct_name).ok_or_else(|| InspectError::not_found(format!("There is no Cosmos DB account '{acct_name}'.")))?;
    match rest {
        [] => Ok(account_node(acct_name, acct, req)),
        ["databases"] => Ok(databases_node(acct_name, acct, req)),
        ["databases", db, tail @ ..] => {
            let d = acct.databases.get(*db).ok_or_else(|| InspectError::not_found(format!("There is no database '{db}'.")))?;
            match tail {
                [] => Ok(database_node(acct_name, acct, d, req)),
                ["containers"] => Ok(containers_node(acct_name, d, req)),
                ["containers", c, ctail @ ..] => {
                    let cont =
                        d.containers.get(*c).ok_or_else(|| InspectError::not_found(format!("There is no container '{c}' in '{db}'.")))?;
                    container_tree(acct_name, acct, d, cont, ctail, req, now).ok_or_else(missing)?
                }
                _ => Err(missing()),
            }
        }
        _ => Err(missing()),
    }
}

fn service_node(state: &State, req: &InspectRequest) -> Node {
    let mut names: Vec<&String> = state.accounts.keys().collect();
    names.sort();
    let (range, next) = req.page_range(names.len());
    let items =
        names[range].iter().map(|n| json!({"name": n, "path": req.path(&[n]), "databases": state.accounts[*n].databases.len()})).collect();
    Node::new(req.path(&[]), "service", "cosmos").prop("accounts", names.len()).with_items(items, next)
}

fn account_node(name: &str, a: &Account, req: &InspectRequest) -> Node {
    Node::new(req.path(&[name]), "account", name).prop("databases", a.databases.len()).prop("offers", a.offers.len()).child(
        "databases",
        "databases",
        Some(a.databases.len() as u64),
        req.path(&[name, "databases"]),
    )
}

fn databases_node(name: &str, a: &Account, req: &InspectRequest) -> Node {
    let (rows, next) = req.page_map(&a.databases);
    let items = rows
        .into_iter()
        .map(|(k, d)| json!({"name": k, "path": req.path(&[name, "databases", k]), "containers": d.containers.len(), "rid": d.meta.rid, "throughput": throughput(a, &d.offer)}))
        .collect();
    Node::new(req.path(&[name, "databases"]), "databases", "databases").prop("count", a.databases.len()).with_items(items, next)
}

/// The throughput setting of an offer, as it is stored.
fn throughput(a: &Account, offer: &Option<String>) -> Value {
    offer.as_ref().and_then(|r| a.offers.get(r)).map_or(Value::Null, |o| o.content.clone())
}

fn database_node(name: &str, a: &Account, d: &Database, req: &InspectRequest) -> Node {
    Node::new(req.path(&[name, "databases", &d.meta.id]), "database", &d.meta.id)
        .prop("rid", &d.meta.rid)
        .prop("etag", &d.meta.etag)
        .prop("timestamp", azemu_core::inspect::unix_time_value(d.meta.ts))
        .prop("throughput", throughput(a, &d.offer))
        .prop("users", d.users.len())
        .prop("properties", &d.props)
        .child("containers", "containers", Some(d.containers.len() as u64), req.path(&[name, "databases", &d.meta.id, "containers"]))
}

fn containers_node(name: &str, d: &Database, req: &InspectRequest) -> Node {
    let (rows, next) = req.page_map(&d.containers);
    let items = rows
        .into_iter()
        .map(|(k, c)| {
            json!({"name": k, "path": req.path(&[name, "databases", &d.meta.id, "containers", k]), "partitionKey": c.pk.paths, "documents": c.docs.len(), "defaultTtl": c.default_ttl})
        })
        .collect();
    Node::new(req.path(&[name, "databases", &d.meta.id, "containers"]), "containers", "containers")
        .prop("count", d.containers.len())
        .with_items(items, next)
}

fn container_tree(
    name: &str,
    a: &Account,
    d: &Database,
    c: &Container,
    tail: &[&str],
    req: &InspectRequest,
    now: i64,
) -> Option<InspectResult> {
    let base = |more: &[&str]| {
        let mut p: Vec<&str> = vec![name, "databases", &d.meta.id, "containers", &c.meta.id];
        p.extend_from_slice(more);
        req.path(&p)
    };
    Some(match tail {
        [] => Ok(container_node(a, c, &base)),
        ["documents"] => documents_node(c, req, now, &base),
        ["documents", id] => document_node(c, id, req, now, &base),
        ["sprocs"] => Ok(resources_node(&c.sprocs, "stored procedures", "sprocs", req, &base)),
        ["triggers"] => Ok(resources_node(&c.triggers, "triggers", "triggers", req, &base)),
        ["udfs"] => Ok(resources_node(&c.udfs, "user-defined functions", "udfs", req, &base)),
        _ => return None,
    })
}

fn container_node(a: &Account, c: &Container, base: &dyn Fn(&[&str]) -> String) -> Node {
    let policy = |k: &str| c.props.get(k).cloned().unwrap_or(Value::Null);
    Node::new(base(&[]), "container", &c.meta.id)
        .prop("rid", &c.meta.rid)
        .prop("etag", &c.meta.etag)
        .prop("timestamp", azemu_core::inspect::unix_time_value(c.meta.ts))
        .prop("partitionKey", json!({"paths": c.pk.paths, "kind": c.pk.kind, "version": c.pk.version}))
        .prop("indexingPolicy", policy("indexingPolicy"))
        .prop("defaultTtl", c.default_ttl)
        .prop("uniqueKeys", &c.unique_keys)
        .prop("documents", c.docs.len())
        .prop("throughput", throughput(a, &c.offer))
        .prop(
            "changeFeed",
            json!({
                "latestVersionEntries": c.changes.len(),
                "lastSequenceNumber": c.lsn,
                "allVersionsAndDeletesRecords": c.log.as_ref().map(|l| l.records.len()),
                "allVersionsAndDeletesStartedAfter": c.log.as_ref().map(|l| l.floor),
            }),
        )
        .child("documents", "documents", Some(c.docs.len() as u64), base(&["documents"]))
        .child("sprocs", "sprocs", Some(c.sprocs.len() as u64), base(&["sprocs"]))
        .child("triggers", "triggers", Some(c.triggers.len() as u64), base(&["triggers"]))
        .child("udfs", "udfs", Some(c.udfs.len() as u64), base(&["udfs"]))
}

fn expired(c: &Container, doc: &Doc, now: i64) -> bool {
    c.expiry_of(doc).is_some_and(|at| at <= now)
}

fn pk_json(k: &DocKey) -> Value {
    Value::Array(k.pk.iter().map(PkComp::to_json).collect())
}

/// `?partitionKey=` as the SDKs write it: a JSON array (`["a"]`), or a bare JSON value or plain text for one component.
fn parse_pk(text: &str) -> Result<Vec<PkComp>, InspectError> {
    let bad = |e: String| InspectError::bad_request(format!("partitionKey is not a partition key value: {e}"));
    let v: Value = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()));
    let items = match v {
        Value::Array(a) => a,
        other => vec![other],
    };
    items.iter().map(|i| PkComp::from_json(i).map_err(|e| bad(e.message))).collect()
}

fn doc_row(c: &Container, k: &DocKey, d: &Doc, base: &dyn Fn(&[&str]) -> String) -> Value {
    let size = serde_json::to_string(&*d.value).map_or(0, |s| s.len());
    let document = if size > ROW_DOC_LIMIT { json!({"shownInTheNode": true, "bytes": size}) } else { (*d.value).clone() };
    let mut path = base(&["documents", &k.id]);
    path.push_str(&format!("?partitionKey={}", azemu_core::inspect::seg(&pk_json(k).to_string())));
    json!({
        "name": k.id,
        "path": path,
        "partitionKey": pk_json(k),
        "rid": d.rid,
        "lsn": d.lsn,
        "timestamp": azemu_core::inspect::unix_time_value(d.ts),
        "etag": d.etag,
        "expiresAt": c.expiry_of(d).map(azemu_core::inspect::unix_time_value),
        "document": document,
    })
}

fn documents_node(c: &Container, req: &InspectRequest, now: i64, base: &dyn Fn(&[&str]) -> String) -> InspectResult {
    let pk = req.param("partitionKey").map(parse_pk).transpose()?;
    // the marker is the last row's key: its partition key and id
    let after: Option<DocKey> = match req.marker() {
        Some(m) => {
            let v: Value = serde_json::from_str(m).map_err(|_| InspectError::bad_request("The marker is not one this node gave out."))?;
            let comps: Vec<PkComp> = v["pk"]
                .as_array()
                .ok_or_else(|| InspectError::bad_request("The marker is not one this node gave out."))?
                .iter()
                .filter_map(|x| PkComp::from_json(x).ok())
                .collect();
            Some(DocKey { pk: comps, id: v["id"].as_str().unwrap_or("").to_string() })
        }
        None => None,
    };
    let lower = match (&after, &pk) {
        (Some(a), _) => Bound::Excluded(a.clone()),
        (None, Some(p)) => Bound::Included(DocKey { pk: p.clone(), id: String::new() }),
        (None, None) => Bound::Unbounded,
    };
    let rows = c
        .docs
        .range((lower, Bound::Unbounded))
        .take_while(|(k, _)| pk.as_ref().is_none_or(|p| k.pk.len() >= p.len() && k.pk[..p.len()] == p[..]))
        .filter(|(_, d)| !expired(c, d, now))
        .map(|(k, d)| (json!({"pk": pk_json(k), "id": k.id}).to_string(), doc_row(c, k, d, base)));
    let (items, next) = req.page_keyed(rows);
    Ok(Node::new(base(&["documents"]), "documents", "documents")
        .accepts(&["partitionKey"])
        .prop("count", c.docs.len())
        .prop(
            "note",
            "Documents past their time to live are left out; they are removed when the service next reads or writes the container.",
        )
        .with_items(items, next))
}

fn document_node(c: &Container, id: &str, req: &InspectRequest, now: i64, base: &dyn Fn(&[&str]) -> String) -> InspectResult {
    let pk = req.param("partitionKey").map(parse_pk).transpose()?;
    let found: Vec<(&DocKey, &Doc)> = match &pk {
        Some(p) => c.docs.get_key_value(&DocKey { pk: p.clone(), id: id.to_string() }).into_iter().collect(),
        None => c.docs.iter().filter(|(k, _)| k.id == id).collect(),
    };
    let found: Vec<_> = found.into_iter().filter(|(_, d)| !expired(c, d, now)).collect();
    match found.as_slice() {
        [] => Err(InspectError::not_found(format!("There is no document '{id}' in this container."))),
        [(k, d)] => Ok(Node::new(base(&["documents", id]), "document", id)
            .accepts(&["partitionKey"])
            .prop("partitionKey", pk_json(k))
            .prop("rid", &d.rid)
            .prop("lsn", d.lsn)
            .prop("timestamp", azemu_core::inspect::unix_time_value(d.ts))
            .prop("etag", &d.etag)
            .prop("expiresAt", c.expiry_of(d).map(azemu_core::inspect::unix_time_value))
            .prop("document", &*d.value)),
        _ => Err(InspectError::bad_request(format!(
            "Several documents have the id '{id}' (one in each of {} partitions): add ?partitionKey=.",
            found.len()
        ))),
    }
}

fn resources_node(
    map: &std::collections::BTreeMap<String, Resource>,
    title: &str,
    group: &str,
    req: &InspectRequest,
    base: &dyn Fn(&[&str]) -> String,
) -> Node {
    let (rows, next) = req.page_map(map);
    let items = rows
        .into_iter()
        .map(|(k, r)| {
            // what the client stored is a value of its own (`properties`): a stored procedure's own `path` or `name` must not become the row's
            // (the page builds a link from them), and one that is long is cut by the row limit like any other value
            let stored: Map<String, Value> =
                r.props.iter().filter(|(pk, _)| *pk != "id").map(|(pk, pv)| (pk.clone(), pv.clone())).collect();
            json!({
                "name": k,
                "rid": r.meta.rid,
                "timestamp": azemu_core::inspect::unix_time_value(r.meta.ts),
                "properties": stored,
            })
        })
        .collect();
    Node::new(base(&[group]), group, title).prop("count", map.len()).with_items(items, next)
}
