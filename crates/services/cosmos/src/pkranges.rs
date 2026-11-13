//! `/pkranges`: every container has one physical partition, id "0", covering the whole
//! effective partition key space.

use serde_json::{json, Value};

use crate::documents::Loc;
use crate::out::{Cx, Out};
use crate::query;
use crate::rid;
use crate::state::Container;

pub fn range_json(loc: &Loc, c: &Container) -> Value {
    let r = rid::encode(&rid::pkrange(&c.meta.rid_bytes, 0));
    json!({
        "_rid": r,
        "id": "0",
        "_etag": etag(c),
        "minInclusive": "",
        "maxExclusive": "FF",
        "ridPrefix": 0,
        "_self": format!("dbs/{}/colls/{}/pkranges/{}/", loc.db_rid, c.meta.rid, r),
        "throughputFraction": 1,
        "status": "online",
        "parents": [],
        "_ts": c.meta.ts
    })
}

pub fn etag(c: &Container) -> String {
    format!("\"azemu-pkr-{}\"", c.meta.rid)
}

pub fn read(cx: &Cx, loc: &Loc, c: &Container) -> crate::errors::R<Out> {
    let tag = etag(c);
    if let Some(inm) = cx.header("if-none-match") {
        if inm.trim() == tag {
            return Ok(Out::feed_not_modified(tag).lsn(c.lsn).charge(1.0));
        }
    }
    let r = range_json(loc, c);
    let mut out = query::feed(cx, &c.meta.rid, "PartitionKeyRanges", vec![r])?;
    out = out.header("etag", tag).lsn(c.lsn);
    Ok(out)
}
