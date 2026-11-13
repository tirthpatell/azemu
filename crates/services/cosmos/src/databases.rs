//! `/dbs`: databases.

use serde_json::{Map, Value};

use crate::errors::{check_id, CosmosError, R};
use crate::offers;
use crate::out::{Cx, Out};
use crate::query;
use crate::resources::resource_out;
use crate::rid;
use crate::state::{Account, Database, Meta};

pub fn list_or_query(cx: &Cx, acct: &Account) -> R<Out> {
    let mut dbs: Vec<&Database> = acct.databases.values().collect();
    dbs.sort_by_key(|d| d.meta.seq);
    query::feed(cx, "", "Databases", dbs.into_iter().map(|d| d.to_json()).collect())
}

pub fn create(cx: &Cx, acct: &mut Account) -> R<Out> {
    let body = cx.body_object()?;
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CosmosError::bad_request("The input content is invalid because the required properties - 'id; ' - are missing"))?
        .to_string();
    check_id(&id)?;
    if acct.databases.contains_key(&id) {
        return Err(CosmosError::conflict());
    }
    let tp = offers::from_headers(cx)?;
    let (bytes, used) = rid::database(acct.db_seq + 1);
    acct.db_seq = used;
    let seq = acct.next_seq();
    let meta = Meta::new(&id, bytes, seq, cx.now);
    let mut db = Database {
        meta,
        props: Map::new(),
        containers: Default::default(),
        users: Default::default(),
        permissions: Default::default(),
        coll_seq: 0,
        user_seq: 0,
        perm_seq: 0,
        offer: None,
    };
    if let Some(tp) = tp {
        db.offer = Some(offers::create(acct, cx.now, &tp, db.link(), db.meta.rid.clone()));
    }
    let v = db.to_json();
    let rid = db.meta.rid.clone();
    acct.databases.insert(id.clone(), db);
    Ok(resource_out(201, v).paths(format!("dbs/{id}"), rid))
}

pub fn read(acct: &mut Account, key: &str) -> R<Out> {
    let db = acct.db(key)?;
    let v = db.to_json();
    Ok(resource_out(200, v).paths(format!("dbs/{}", db.meta.id), db.meta.rid.clone()))
}

pub fn delete(acct: &mut Account, key: &str) -> R<Out> {
    let name = acct.db_name(key).ok_or_else(CosmosError::not_found_default)?;
    if let Some(db) = acct.databases.remove(&name) {
        offers::remove(acct, &db.offer);
        for c in db.containers.values() {
            offers::remove(acct, &c.offer);
        }
    }
    Ok(Out::empty(204))
}
