//! Query Tables, Create Table, Delete Table, Get Table.

use std::borrow::Cow;

use azemu_core::Response;

use crate::api::{base_headers, check_name, decode_token, encode_token, Rq};
use crate::errors::{invalid_input, invalid_input_msg, needs_value, resource_not_found, table_exists, Res};
use crate::filter::{self, Lookup};
use crate::json::{self, ObjWriter, J};
use crate::odata::{envelope, Level};
use crate::sas::Op;
use crate::state::{Account, Table};
use crate::values::Value;
use azemu_core::resp::Resp;

struct TableRow<'a>(&'a str);

impl Lookup for TableRow<'_> {
    fn get(&self, name: &str) -> Option<Cow<'_, Value>> {
        (name == "TableName").then(|| Cow::Owned(Value::Str(self.0.to_string())))
    }
}

fn table_json(rq: &Rq, name: &str, single: bool) -> String {
    let mut w = ObjWriter::new();
    if rq.level != Level::No && single {
        w.str("odata.metadata", &format!("{}/$metadata#Tables/@Element", rq.base));
    }
    if rq.level == Level::Full {
        w.str("odata.type", &format!("{}.Tables", rq.req.account));
        w.str("odata.id", &format!("{}/Tables('{}')", rq.base, name));
        w.str("odata.editLink", &format!("Tables('{}')", name));
    }
    w.str("TableName", name);
    w.finish()
}

pub fn query(rq: &Rq, acct: &Account) -> Res<Response> {
    rq.grant.check(Op::QueryTables, None)?;
    let expr = match rq.query("$filter") {
        Some(f) if !f.trim().is_empty() => Some(filter::parse(f).map_err(|e| invalid_input_msg(&e.message(f)))?),
        _ => None,
    };
    let top = match rq.query("$top") {
        Some(t) => t.trim().parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(invalid_input)?.min(1000),
        None => 1000,
    };
    let from = rq.query("NextTableName").map(|t| decode_token(t).unwrap_or_else(|| t.to_string()).to_ascii_lowercase());
    let mut items = Vec::new();
    let mut next: Option<String> = None;
    for (lower, t) in &acct.tables {
        if from.as_ref().is_some_and(|f| lower < f) {
            continue;
        }
        if !expr.as_ref().map(|e| filter::eval(e, &TableRow(&t.name))).unwrap_or(true) {
            continue;
        }
        if items.len() == top {
            next = Some(t.name.clone());
            break;
        }
        items.push(table_json(rq, &t.name, false));
    }
    let body = envelope(rq.level, &format!("{}/$metadata#Tables", rq.base), &items);
    let mut r = rq.json(200, body);
    if let Some(n) = next {
        r = r.header("x-ms-continuation-NextTableName", encode_token(&n));
    }
    Ok(r.build())
}

pub fn create(rq: &Rq, acct: &mut Account) -> Res<Response> {
    rq.grant.check(Op::CreateTable, None)?;
    let text = std::str::from_utf8(&rq.req.body).map_err(|_| invalid_input())?;
    let doc = json::parse(text).map_err(|_| invalid_input())?;
    let name = match doc.get("TableName") {
        Some(J::Str(s)) => s.clone(),
        Some(J::Null) | None => return Err(needs_value()),
        Some(_) => return Err(invalid_input()),
    };
    check_name(&name)?;
    let lower = name.to_ascii_lowercase();
    if acct.tables.contains_key(&lower) {
        return Err(table_exists());
    }
    acct.tables.insert(lower, Table { name: name.clone(), ..Default::default() });
    let location = format!("{}/Tables('{}')", rq.base, name);
    if rq.req.header("prefer").is_some_and(|p| p.to_ascii_lowercase().contains("return-no-content")) {
        return Ok(base_headers(Resp::new(204u16))
            .header("Location", &location)
            .header("DataServiceId", &location)
            .header("Preference-Applied", "return-no-content")
            .build());
    }
    let mut r = rq.json(201, table_json(rq, &name, true)).header("Location", &location);
    if rq.req.header("prefer").is_some() {
        r = r.header("Preference-Applied", "return-content");
    }
    Ok(r.build())
}

pub fn delete(rq: &Rq, acct: &mut Account, name: &str) -> Res<Response> {
    rq.grant.check(Op::DeleteTable, None)?;
    check_name(name)?;
    match acct.tables.remove(&name.to_ascii_lowercase()) {
        Some(_) => Ok(base_headers(Resp::new(204u16)).build()),
        None => Err(resource_not_found()),
    }
}

pub fn get(rq: &Rq, acct: &Account, name: &str) -> Res<Response> {
    rq.grant.check(Op::QueryTables, None)?;
    check_name(name)?;
    match acct.tables.get(&name.to_ascii_lowercase()) {
        Some(t) => Ok(rq.json(200, table_json(rq, &t.name, true)).build()),
        None => Err(resource_not_found()),
    }
}
