//! `/pkranges`: every container has one physical partition, id "0", covering the whole
//! effective partition key space.

use serde_json::{json, Value};

use crate::documents::Loc;
use crate::errors::CosmosError;
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
        "ownedArchivalPKRangeIds": [],
        "_ts": c.meta.ts,
        "lsn": c.lsn
    })
}

/// The `_etag` of a range, and the legacy position of the change feed of ranges that version 0.1.0 handed out.
pub fn etag(c: &Container) -> String {
    format!("\"azemu-pkr-{}\"", c.meta.rid)
}

/// The position of the change feed of partition key ranges. The ranges never change here, so it is always 1.
const FEED_POSITION: i64 = 1;

/// The list of ranges. Azure (checked in November 2026) sends a plain read with no `etag` and no request charge and ignores
/// `If-None-Match`. With `A-IM: Incremental feed` (the Python SDK reads the ranges that way and drains the feed until a 304) it
/// is a change feed: the answer carries `etag` (a position, `"<lsn>"`), a position that is not behind is a 304, zero or a negative
/// number starts over, and a position that is not a number is a 400. The feed has no request charge either.
pub fn read(cx: &Cx, loc: &Loc, c: &Container) -> crate::errors::R<Out> {
    let feed = cx.header("a-im").is_some_and(|v| v.to_ascii_lowercase().contains("incremental"));
    if feed {
        if let Some(inm) = cx.header("if-none-match") {
            let t = inm.trim().trim_matches('"');
            let behind = if t == etag(c).trim_matches('"') {
                false
            } else if t.is_empty() {
                true
            } else {
                match t.parse::<i64>() {
                    Ok(n) => n < FEED_POSITION,
                    Err(_) => return Err(CosmosError::bad_request("Change feed token format is invalid")),
                }
            };
            if !behind {
                return Ok(Out::feed_not_modified(format!("\"{FEED_POSITION}\"")).lsn(c.lsn).without_charge());
            }
        }
    }
    let r = range_json(loc, c);
    let mut out = query::feed(cx, &c.meta.rid, "PartitionKeyRanges", vec![r])?.lsn(c.lsn).without_charge();
    if feed {
        out = out.header("etag", format!("\"{FEED_POSITION}\""));
    }
    Ok(out)
}
