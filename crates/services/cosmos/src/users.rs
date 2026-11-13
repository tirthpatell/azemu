//! `/dbs/{db}/users` and `/users/{user}/permissions`: definitions only.

use crate::errors::{CosmosError, R};
use crate::out::{Cx, Out};
use crate::resources::{no_extras, permission_extras, validate_none, validate_permission, Scope};
use crate::rid;
use crate::state::Database;

fn users_scope(db: &mut Database) -> Scope<'_> {
    Scope {
        map: &mut db.users,
        parent_bytes: &db.meta.rid_bytes,
        seq: &mut db.user_seq,
        link_prefix: format!("dbs/{}/users/", db.meta.rid),
        list_key: "Users",
        parent_rid: db.meta.rid.clone(),
        kind: rid::Kind::User,
        validate: validate_none,
        extras: no_extras,
    }
}

pub fn list(cx: &Cx, db: &mut Database) -> R<Out> {
    users_scope(db).list(cx)
}

pub fn create(cx: &Cx, db: &mut Database, upsert: bool) -> R<Out> {
    users_scope(db).create(cx, upsert)
}

pub fn read(db: &mut Database, key: &str) -> R<Out> {
    users_scope(db).read(key)
}

pub fn replace(cx: &Cx, db: &mut Database, key: &str) -> R<Out> {
    let old = users_scope(db).name_of(key).ok_or_else(CosmosError::not_found_default)?;
    let out = users_scope(db).replace(cx, key)?;
    let new_id = cx.body_object()?.get("id").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| old.clone());
    if new_id != old {
        if let Some(p) = db.permissions.remove(&old) {
            db.permissions.insert(new_id, p);
        }
    }
    Ok(out)
}

pub fn delete(db: &mut Database, key: &str) -> R<Out> {
    let name = users_scope(db).name_of(key);
    let out = users_scope(db).delete(key)?;
    if let Some(n) = name {
        db.permissions.remove(&n);
    }
    Ok(out)
}

fn perm_scope<'a>(db: &'a mut Database, user_key: &str, ubytes: &'a [u8], uname: String, urid: &str) -> Scope<'a> {
    let _ = user_key;
    Scope {
        map: db.permissions.entry(uname).or_default(),
        parent_bytes: ubytes,
        seq: &mut db.perm_seq,
        link_prefix: format!("dbs/{}/users/{}/permissions/", db.meta.rid, urid),
        list_key: "Permissions",
        parent_rid: urid.to_string(),
        kind: rid::Kind::Permission,
        validate: validate_permission,
        extras: permission_extras,
    }
}

/// Run `f` on the permission scope of `user_key`.
pub fn with_permissions<T>(db: &mut Database, user_key: &str, f: impl FnOnce(&mut Scope) -> R<T>) -> R<T> {
    let uname = users_scope(db).name_of(user_key).ok_or_else(CosmosError::not_found_default)?;
    let (ubytes, urid) = {
        let u = db.users.get(&uname).ok_or_else(CosmosError::not_found_default)?;
        (u.meta.rid_bytes.clone(), u.meta.rid.clone())
    };
    let mut scope = perm_scope(db, user_key, &ubytes, uname, &urid);
    f(&mut scope)
}
