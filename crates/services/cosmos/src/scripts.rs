//! Stored procedures, triggers and user defined functions: the definitions (create, read, replace, delete, list, query).
//! Running them is `script_run.rs`, `script_host.rs`, `js/` and `udf.rs`.

use crate::errors::R;
use crate::out::{Cx, Out};
use crate::resources::{no_extras, validate_body, validate_trigger, Scope, Validator};
use crate::rid;
use crate::state::Container;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Sproc,
    Trigger,
    Udf,
}

pub fn with_scope<T>(c: &mut Container, db_rid: &str, kind: Kind, f: impl FnOnce(&mut Scope) -> R<T>) -> R<T> {
    let (map, seg, key, validate, id_kind): (_, _, _, Validator, rid::Kind) = match kind {
        Kind::Sproc => (&mut c.sprocs, "sprocs", "StoredProcedures", validate_body, rid::Kind::StoredProcedure),
        Kind::Trigger => (&mut c.triggers, "triggers", "Triggers", validate_trigger, rid::Kind::Trigger),
        Kind::Udf => (&mut c.udfs, "udfs", "UserDefinedFunctions", validate_body, rid::Kind::Udf),
    };
    let mut scope = Scope {
        map,
        parent_bytes: &c.meta.rid_bytes,
        seq: &mut c.child_seq,
        link_prefix: format!("dbs/{db_rid}/colls/{}/{seg}/", c.meta.rid),
        list_key: key,
        parent_rid: c.meta.rid.clone(),
        kind: id_kind,
        validate,
        extras: no_extras,
    };
    f(&mut scope)
}

pub fn list(cx: &Cx, c: &mut Container, db_rid: &str, kind: Kind) -> R<Out> {
    with_scope(c, db_rid, kind, |s| s.list(cx))
}

pub fn create(cx: &Cx, c: &mut Container, db_rid: &str, kind: Kind) -> R<Out> {
    let upsert = cx.flag("x-ms-documentdb-is-upsert");
    with_scope(c, db_rid, kind, |s| s.create(cx, upsert))
}

pub fn read(c: &mut Container, db_rid: &str, kind: Kind, key: &str) -> R<Out> {
    with_scope(c, db_rid, kind, |s| s.read(key))
}

pub fn replace(cx: &Cx, c: &mut Container, db_rid: &str, kind: Kind, key: &str) -> R<Out> {
    with_scope(c, db_rid, kind, |s| s.replace(cx, key))
}

pub fn delete(c: &mut Container, db_rid: &str, kind: Kind, key: &str) -> R<Out> {
    with_scope(c, db_rid, kind, |s| s.delete(key))
}

/// `POST /sprocs/{id}`: executing a stored procedure (step 1 of `script_run.rs`; the script runs after the lock is released).
pub fn execute(cx: &Cx, c: &mut Container, loc: &crate::documents::Loc, db_name: &str, key: &str) -> R<Out> {
    crate::script_run::prepare_sproc(cx, c, loc, db_name, key)
}
