//! `/dbs/{db}/colls`: containers, with the policies the service fills in.

use serde_json::{json, Map, Value};

use crate::errors::{check_id, CosmosError, R};
use crate::offers::{self, Throughput};
use crate::out::{Cx, Out};
use crate::pk::PartitionKeyDef;
use crate::query;
use crate::resources::resource_out;
use crate::rid;
use crate::state::{Account, Container, Database, Meta};

pub fn list_or_query(cx: &Cx, db: &Database) -> R<Out> {
    let mut cs: Vec<&Container> = db.containers.values().collect();
    cs.sort_by_key(|c| c.meta.seq);
    query::feed(cx, &db.meta.rid, "DocumentCollections", cs.into_iter().map(|c| c.to_json(&db.meta.rid)).collect())
}

const ETAG_EXCLUDE: &str = "/\"_etag\"/?";

/// Indexing policy with the service defaults filled in.
fn indexing_policy(given: Option<&Value>) -> R<Value> {
    let mut m = match given {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(o)) => o.clone(),
        Some(_) => return Err(CosmosError::bad_request("indexingPolicy must be an object.")),
    };
    // Any case is accepted (the Go SDK's constants are "Consistent" and "None"); the policy stores it
    // in lower case, as the service shows it.
    let given_mode = m.get("indexingMode").and_then(Value::as_str).unwrap_or("consistent").to_string();
    let mode = given_mode.to_ascii_lowercase();
    if mode != "consistent" && mode != "none" && mode != "lazy" {
        return Err(CosmosError::bad_request(format!("indexingMode {given_mode} is not supported.")));
    }
    let mut out = Map::new();
    out.insert("indexingMode".into(), json!(mode));
    out.insert("automatic".into(), m.shift_remove("automatic").unwrap_or(json!(true)));
    m.shift_remove("indexingMode");
    let mut included = match m.shift_remove("includedPaths") {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    let mut excluded = match m.shift_remove("excludedPaths") {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    if mode != "none" {
        if included.is_empty() {
            included.push(json!({ "path": "/*" }));
        }
        let has = excluded.iter().any(|e| e.get("path").and_then(Value::as_str) == Some(ETAG_EXCLUDE));
        if !has {
            excluded.push(json!({ "path": ETAG_EXCLUDE }));
        }
    }
    out.insert("includedPaths".into(), Value::Array(included));
    out.insert("excludedPaths".into(), Value::Array(excluded));
    for (k, v) in m {
        out.insert(k, v);
    }
    if let Some(ci) = out.get("compositeIndexes").and_then(Value::as_array) {
        for c in ci {
            let arr = c.as_array().map(Vec::as_slice).unwrap_or(&[]);
            if arr.len() < 2 || arr.iter().any(|p| p.get("path").and_then(Value::as_str).is_none()) {
                return Err(CosmosError::bad_request("A composite index needs at least two paths."));
            }
        }
    }
    Ok(Value::Object(out))
}

fn conflict_policy(given: Option<&Value>) -> R<Value> {
    let g = given.and_then(Value::as_object);
    let given_mode = g.and_then(|o| o.get("mode")).and_then(Value::as_str).unwrap_or("LastWriterWins");
    // Any case is accepted; the policy shows the documented spelling.
    let mode = ["LastWriterWins", "Custom"]
        .into_iter()
        .find(|m| m.eq_ignore_ascii_case(given_mode))
        .ok_or_else(|| CosmosError::bad_request(format!("Conflict resolution mode {given_mode} is not supported.")))?;
    let path = g.and_then(|o| o.get("conflictResolutionPath")).and_then(Value::as_str);
    let proc = g.and_then(|o| o.get("conflictResolutionProcedure")).and_then(Value::as_str);
    let path = match (mode, path) {
        ("LastWriterWins", None) => Some("/_ts"),
        (_, p) => p,
    };
    let mut m = Map::new();
    m.insert("mode".into(), json!(mode));
    if let Some(p) = path {
        m.insert("conflictResolutionPath".into(), json!(p));
    }
    m.insert("conflictResolutionProcedure".into(), json!(proc.unwrap_or("")));
    Ok(Value::Object(m))
}

fn unique_keys(given: Option<&Value>) -> R<(Value, Vec<Vec<String>>)> {
    let keys = match given.and_then(|g| g.get("uniqueKeys")) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err(CosmosError::bad_request("uniqueKeys must be an array.")),
    };
    let mut parsed = Vec::new();
    for k in &keys {
        let paths: Vec<String> = k
            .get("paths")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if paths.is_empty() || paths.iter().any(|p| !p.starts_with('/')) {
            return Err(CosmosError::bad_request("Each unique key needs at least one path that starts with '/'."));
        }
        parsed.push(paths);
    }
    Ok((json!({ "uniqueKeys": keys }), parsed))
}

fn default_ttl(body: &Map<String, Value>) -> R<Option<i64>> {
    match body.get("defaultTtl") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_i64() {
            Some(n) if n == -1 || (n > 0 && n <= 2_147_483_647) => Ok(Some(n)),
            _ => Err(CosmosError::bad_request(
                "The value of DefaultTimeToLive must be either -1 (never expire) or a positive integer up to 2147483647.",
            )),
        },
    }
}

/// Build the stored definition (everything returned to the client except system properties).
struct Built {
    props: Map<String, Value>,
    pk: PartitionKeyDef,
    unique: Vec<Vec<String>>,
    ttl: Option<i64>,
}

fn build(body: &Map<String, Value>, id: &str) -> R<Built> {
    let pk_json = match body.get("partitionKey") {
        Some(v) => v.clone(),
        None => json!({ "paths": ["/_partitionKey"], "kind": "Hash" }),
    };
    let pk = PartitionKeyDef::from_json(&pk_json)?;
    let mut pk_out = Map::new();
    pk_out.insert("paths".into(), json!(pk.paths));
    pk_out.insert("kind".into(), json!(pk.kind));
    pk_out.insert("version".into(), json!(pk.version));
    if pk_json.get("systemKey").is_some() {
        pk_out.insert("systemKey".into(), pk_json["systemKey"].clone());
    }
    let (uk_json, unique) = unique_keys(body.get("uniqueKeyPolicy"))?;
    let ttl = default_ttl(body)?;
    let mut props = Map::new();
    props.insert("id".into(), json!(id));
    props.insert("indexingPolicy".into(), indexing_policy(body.get("indexingPolicy"))?);
    props.insert("geospatialConfig".into(), body.get("geospatialConfig").cloned().unwrap_or(json!({ "type": "Geography" })));
    props.insert("uniqueKeyPolicy".into(), uk_json);
    props.insert("conflictResolutionPolicy".into(), conflict_policy(body.get("conflictResolutionPolicy"))?);
    props.insert("partitionKey".into(), Value::Object(pk_out));
    if let Some(t) = ttl {
        props.insert("defaultTtl".into(), json!(t));
    }
    for k in [
        "computedProperties",
        "analyticalStorageTtl",
        "changeFeedPolicy",
        "vectorEmbeddingPolicy",
        "fullTextPolicy",
        "clientEncryptionPolicy",
    ] {
        if let Some(v) = body.get(k) {
            props.insert(k.into(), v.clone());
        }
    }
    Ok(Built { props, pk, unique, ttl })
}

pub fn create(cx: &Cx, acct: &mut Account, db_key: &str) -> R<Out> {
    let body = cx.body_object()?;
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CosmosError::bad_request("The input content is invalid because the required properties - 'id; ' - are missing"))?
        .to_string();
    check_id(&id)?;
    let built = build(&body, &id)?;
    let tp = offers::from_headers(cx)?;
    let db = acct.db(db_key)?;
    if db.containers.contains_key(&id) {
        return Err(CosmosError::conflict());
    }
    let shared = db.offer.is_some();
    let (bytes, used) = rid::collection(&db.meta.rid_bytes, db.coll_seq + 1);
    db.coll_seq = used;
    let meta = Meta::new(&id, bytes, used, cx.now);
    let mut c = Container {
        meta,
        props: built.props,
        pk: built.pk,
        unique_keys: built.unique,
        default_ttl: built.ttl,
        docs: Default::default(),
        by_rid: Default::default(),
        changes: Default::default(),
        lsn: 0,
        doc_seq: 0,
        child_seq: 0,
        sprocs: Default::default(),
        triggers: Default::default(),
        udfs: Default::default(),
        offer: None,
        min_expiry: 0,
        log: None,
    };
    let link = c.link(&db.meta.rid);
    let crid = c.meta.rid.clone();
    let v = c.to_json(&db.meta.rid);
    let db_rid = db.meta.rid.clone();
    // A container in a database with shared throughput has no offer of its own unless asked
    // for; otherwise it gets the default 400 RU/s, like a provisioned account.
    let tp = match tp {
        Some(t) => Some(t),
        None if shared => None,
        None => Some(Throughput::Manual(400)),
    };
    if let Some(tp) = tp {
        c.offer = Some(offers::create(acct, cx.now, &tp, link, crid));
    }
    let db = acct.db(&db_rid)?;
    db.containers.insert(id.clone(), c);
    Ok(resource_out(201, v))
}

pub fn read(db: &mut Database, key: &str) -> R<Out> {
    let c = db.containers.get(&db.container_name(key).ok_or_else(CosmosError::not_found_default)?);
    let c = c.ok_or_else(CosmosError::not_found_default)?;
    let v = c.to_json(&db.meta.rid);
    Ok(resource_out(200, v).lsn(c.lsn))
}

pub fn replace(cx: &Cx, db: &mut Database, key: &str) -> R<Out> {
    let body = cx.body_object()?;
    let name = db.container_name(key).ok_or_else(CosmosError::not_found_default)?;
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CosmosError::bad_request("The input content is invalid because the required properties - 'id; ' - are missing"))?
        .to_string();
    if id != name {
        return Err(CosmosError::bad_request("The container id cannot be changed."));
    }
    let built = build(&body, &id)?;
    let db_rid = db.meta.rid.clone();
    let c = db.containers.get_mut(&name).ok_or_else(CosmosError::not_found_default)?;
    if built.pk.paths != c.pk.paths || built.pk.kind != c.pk.kind {
        return Err(CosmosError::bad_request("Partition key path or kind cannot be changed after the container is created."));
    }
    if built.unique != c.unique_keys {
        return Err(CosmosError::bad_request("The unique key policy cannot be changed after the container is created."));
    }
    // keep the stored version when the request did not state one
    let mut props = built.props;
    if body.get("partitionKey").and_then(|p| p.get("version")).is_none() {
        props.insert("partitionKey".into(), c.props.get("partitionKey").cloned().unwrap_or(Value::Null));
    }
    c.props = props;
    if built.ttl != c.default_ttl {
        c.min_expiry = 0;
    }
    c.default_ttl = built.ttl;
    c.meta.touch(cx.now);
    let v = c.to_json(&db_rid);
    let lsn = c.lsn;
    Ok(resource_out(200, v).lsn(lsn))
}

pub fn delete(acct: &mut Account, db_key: &str, key: &str) -> R<Out> {
    let db = acct.db(db_key)?;
    let name = db.container_name(key).ok_or_else(CosmosError::not_found_default)?;
    let removed = db.containers.remove(&name);
    if let Some(c) = removed {
        offers::remove(acct, &c.offer);
    }
    Ok(Out::empty(204))
}
