//! Entity operations: Insert, Query, Get, Update, Merge, Insert-or-Replace,
//! Insert-or-Merge and Delete.

use azemu_core::resp::Resp;
use azemu_core::Response;

use crate::api::{base_headers, decode_token, encode_token, table, table_ref, Rq};
use crate::entity::{self, Cond, Mode};
use crate::errors::{invalid_input, invalid_input_msg, ApiError, Res};
use crate::filter;
use crate::odata::{envelope, parse_select};
use crate::sas::Op;
use crate::state::{Account, Entity};

/// Headers of a successful write: the new etag.
fn etag_response(e: &Entity) -> Resp {
    Resp::new(204u16).header("ETag", e.etag())
}

pub fn wants_no_content(prefer: Option<&str>) -> bool {
    prefer.is_some_and(|p| p.to_ascii_lowercase().contains("return-no-content"))
}

pub fn insert(rq: &Rq, acct: &mut Account, table_name: &str) -> Res<Response> {
    let t = table(acct, table_name)?;
    let payload = entity::decode(&rq.req.body, true)?;
    let (pk, rk) = (payload.pk.clone().unwrap_or_default(), payload.rk.clone().unwrap_or_default());
    rq.grant.check(Op::EntityAdd, Some((&pk, &rk)))?;
    let m = t.insert_entity(payload, rq.now)?;
    let e = m.stored.ok_or_else(invalid_input)?;
    let name = t.name.clone();
    let f = rq.fmt(&name);
    let location = f.entity_url(&e.pk, &e.rk);
    if wants_no_content(rq.req.header("prefer")) {
        return Ok(base_headers(etag_response(&e))
            .header("Location", &location)
            .header("DataServiceId", &location)
            .header("Preference-Applied", "return-no-content")
            .build());
    }
    let body = f.entity(&e, true, None);
    let mut r = rq.json(201, body).header("ETag", e.etag()).header("Location", &location);
    if rq.req.header("prefer").is_some() {
        r = r.header("Preference-Applied", "return-content");
    }
    Ok(r.build())
}

pub fn query(rq: &Rq, acct: &Account, table_name: &str) -> Res<Response> {
    let t = table_ref(acct, table_name)?;
    rq.grant.check(Op::EntityRead, None)?;
    let expr = match rq.query("$filter") {
        Some(f) if !f.trim().is_empty() => Some(filter::parse(f).map_err(|e| invalid_input_msg(&e.message(f)))?),
        _ => None,
    };
    let top = match rq.query("$top") {
        Some(n) => n.trim().parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(invalid_input)?.min(1000),
        None => 1000,
    };
    let select = parse_select(rq.query("$select"));
    let token = |name: &str| rq.query(name).map(|v| decode_token(v).unwrap_or_else(|| v.to_string()));
    let (npk, nrk) = (token("NextPartitionKey"), token("NextRowKey"));
    let from = match (&npk, &nrk) {
        (None, None) => None,
        (p, r) => Some((p.clone().unwrap_or_default(), r.clone().unwrap_or_default())),
    };
    let range = rq.grant.range().cloned();
    let allow = move |e: &Entity| range.as_ref().is_none_or(|r| r.contains(&e.pk, &e.rk));
    let page = t.query(expr.as_ref(), from.as_ref().map(|(p, r)| (p.as_str(), r.as_str())), top, &allow);
    let f = rq.fmt(&t.name);
    let items: Vec<String> = page.items.iter().map(|e| f.entity(e, false, select.as_deref())).collect();
    let body = envelope(rq.level, &f.metadata_url(false), &items);
    let mut r = rq.json(200, body);
    if let Some((p, k)) = page.next {
        r = r.header("x-ms-continuation-NextPartitionKey", encode_token(&p)).header("x-ms-continuation-NextRowKey", encode_token(&k));
    }
    Ok(r.build())
}

pub fn get(rq: &Rq, acct: &Account, table_name: &str, pk: &str, rk: &str) -> Res<Response> {
    let t = table_ref(acct, table_name)?;
    rq.grant.check(Op::EntityRead, Some((pk, rk)))?;
    let e = t.entities.get(&crate::state::key(pk, rk)).ok_or_else(crate::errors::resource_not_found)?;
    let select = parse_select(rq.query("$select"));
    let f = rq.fmt(&t.name);
    Ok(rq.json(200, f.entity(e, true, select.as_deref())).header("ETag", e.etag()).build())
}

pub fn update(rq: &Rq, acct: &mut Account, table_name: &str, pk: &str, rk: &str, mode: Mode) -> Res<Response> {
    let t = table(acct, table_name)?;
    let cond = Cond::from_header(rq.req.header("if-match"));
    let op = if cond == Cond::Any { Op::EntityUpsert } else { Op::EntityUpdate };
    rq.grant.check(op, Some((pk, rk)))?;
    entity::validate_key(pk)?;
    entity::validate_key(rk)?;
    let payload = entity::decode(&rq.req.body, false)?;
    let m = t.update_entity(pk, rk, payload, mode, &cond, rq.now)?;
    let e = m.stored.ok_or_else(invalid_input)?;
    Ok(base_headers(etag_response(&e)).build())
}

pub fn delete(rq: &Rq, acct: &mut Account, table_name: &str, pk: &str, rk: &str) -> Res<Response> {
    let t = table(acct, table_name)?;
    rq.grant.check(Op::EntityDelete, Some((pk, rk)))?;
    let Some(h) = rq.req.header("if-match") else {
        return Err(ApiError::new(400, "MissingRequiredHeader", "An HTTP header that's mandatory for this request is not specified."));
    };
    t.delete_entity(pk, rk, &Cond::from_header(Some(h)))?;
    Ok(base_headers(Resp::new(204u16)).build())
}
