//! Entity group transactions (`POST /$batch`).
//!
//! A change set is atomic: every operation is checked first, then they run in
//! order against the table and are undone if one fails. The response holds one
//! part per operation, or a single error part naming the failing operation.

use azemu_core::bytes;
use azemu_core::http::{self, StatusCode};
use azemu_core::resp::Resp;
use azemu_core::{Request, Response};

use crate::api::{base_headers, check_name, table, Rq};
use crate::entity::{self, Cond, Mode, Payload};
use crate::errors::{self, invalid_input, ApiError, Res};
use crate::multipart::{self, Inner, Part};
use crate::odata::{Fmt, Level};
use crate::routing::{self, Target};
use crate::sas::Op;
use crate::state::{Account, Table};

const MAX_OPS: usize = 100;

enum Action {
    Insert(Payload),
    Update { pk: String, rk: String, payload: Payload, mode: Mode, cond: Cond },
    Delete { pk: String, rk: String, cond: Cond },
}

struct Planned {
    table: String,
    pk: String,
    rk: String,
    action: Action,
    level: Level,
    no_content: bool,
}

impl Planned {
    fn op(&self) -> Op {
        match &self.action {
            Action::Insert(_) => Op::EntityAdd,
            Action::Update { cond, .. } if *cond == Cond::Any => Op::EntityUpsert,
            Action::Update { .. } => Op::EntityUpdate,
            Action::Delete { .. } => Op::EntityDelete,
        }
    }
}

fn reason(status: u16) -> &'static str {
    StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("")
}

fn part_text(status: u16, headers: &[(&str, String)], body: &str) -> String {
    let mut s = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("\r\n");
    s.push_str(body);
    s
}

fn error_part(e: &ApiError, rq: &Rq) -> String {
    part_text(
        e.status,
        &[
            ("X-Content-Type-Options", "nosniff".into()),
            ("Cache-Control", "no-cache".into()),
            ("DataServiceVersion", "3.0;".into()),
            ("Content-Type", Level::Minimal.part_content_type().to_string()),
        ],
        &errors::json_body(e, &rq.req.ctx),
    )
}

/// The path of an embedded request without the account segment.
fn inner_target(rq: &Rq, inner: &Inner) -> String {
    let path = inner.target.split('?').next().unwrap_or("");
    if rq.req.ctx.endpoint.path_style {
        let trimmed = path.strip_prefix('/').unwrap_or(path);
        return match trimmed.split_once('/') {
            Some((_, rest)) => format!("/{rest}"),
            None => "/".to_string(),
        };
    }
    path.to_string()
}

fn plan(rq: &Rq, inner: &Inner) -> Res<Planned> {
    let level = Level::from_accept(inner.header("accept"));
    let no_content = inner.header("prefer").is_some_and(|p| p.to_ascii_lowercase().contains("return-no-content"));
    let cond = Cond::from_header(inner.header("if-match"));
    let decode = |require: bool| entity::decode(inner.body.as_bytes(), require);
    let target = routing::parse(&inner_target(rq, inner));
    let method = if inner.method == "MERGE" { "PATCH" } else { inner.method.as_str() };
    let (table, pk, rk, action) = match (method, target) {
        ("POST", Target::Table(t)) => {
            check_name(&t)?;
            let p = decode(true)?;
            let (pk, rk) = (p.pk.clone().unwrap_or_default(), p.rk.clone().unwrap_or_default());
            (t, pk, rk, Action::Insert(p))
        }
        ("PUT" | "PATCH", Target::Entity { table, pk, rk }) => {
            check_name(&table)?;
            entity::validate_key(&pk)?;
            entity::validate_key(&rk)?;
            let mode = if method == "PUT" { Mode::Replace } else { Mode::Merge };
            let payload = decode(false)?;
            (table, pk.clone(), rk.clone(), Action::Update { pk, rk, payload, mode, cond })
        }
        ("DELETE", Target::Entity { table, pk, rk }) => {
            check_name(&table)?;
            if inner.header("if-match").is_none() {
                return Err(ApiError::new(
                    400,
                    "MissingRequiredHeader",
                    "An HTTP header that's mandatory for this request is not specified.",
                ));
            }
            (table, pk.clone(), rk.clone(), Action::Delete { pk, rk, cond })
        }
        _ => return Err(invalid_input()),
    };
    Ok(Planned { table, pk, rk, action, level, no_content })
}

fn success_part(rq: &Rq, p: &Planned, tbl: &Table, stored: Option<&crate::state::Entity>) -> String {
    let f = Fmt { level: p.level, base: &rq.base, account: &rq.req.account, table: &tbl.name, select_raw: None };
    let no_content = |etag: Option<String>| {
        let mut h: Vec<(&str, String)> =
            vec![("X-Content-Type-Options", "nosniff".into()), ("Cache-Control", "no-cache".into()), ("DataServiceVersion", "1.0;".into())];
        if let Some(e) = etag {
            h.push(("ETag", e));
        }
        part_text(204, &h, "")
    };
    match (&p.action, stored) {
        (Action::Insert(_), Some(e)) if !p.no_content => part_text(
            201,
            &[
                ("DataServiceVersion", "3.0;".into()),
                ("Content-Type", p.level.part_content_type().into()),
                ("X-Content-Type-Options", "nosniff".into()),
                ("Cache-Control", "no-cache".into()),
                ("Location", f.entity_url(&e.pk, &e.rk)),
                ("ETag", e.etag()),
            ],
            &f.entity(e, true, None),
        ),
        (_, Some(e)) => no_content(Some(e.etag())),
        (_, None) => no_content(None),
    }
}

/// Run one change set. Returns the response parts, or the single error part.
fn run_change_set(rq: &Rq, acct: &mut Account, ops: &[Inner]) -> Vec<String> {
    match try_change_set(rq, acct, ops) {
        Ok(parts) => parts,
        Err(e) => vec![error_part(&e, rq)],
    }
}

/// Codes the service reports without the operation index in front.
fn without_index(code: &str) -> bool {
    matches!(code, "ResourceNotFound" | "UpdateConditionNotSatisfied")
}

fn try_change_set(rq: &Rq, acct: &mut Account, ops: &[Inner]) -> Res<Vec<String>> {
    if ops.len() > MAX_OPS {
        return Err(ApiError::new(400, "InvalidInput", "99:The batch request operation exceeds the maximum 100 changes per change set."));
    }
    if ops.is_empty() {
        return Err(invalid_input());
    }
    let mut planned = Vec::with_capacity(ops.len());
    for (i, inner) in ops.iter().enumerate() {
        planned.push(plan(rq, inner).map_err(|e| e.at_index(i))?);
    }
    let first = &planned[0];
    let tbl = table(acct, &first.table).map_err(|e| e.at_index(0))?;
    let mut seen = std::collections::HashSet::new();
    for (i, p) in planned.iter().enumerate() {
        if !p.table.eq_ignore_ascii_case(&first.table) {
            return Err(ApiError::new(400, "InvalidInput", "All commands in a batch must operate on the same table.").at_index(i));
        }
        if p.pk != first.pk {
            return Err(ApiError::new(
                400,
                "CommandsInBatchActOnDifferentPartitions",
                "All commands in a batch must operate on the same entity group.",
            )
            .at_index(i));
        }
        if !seen.insert((p.pk.clone(), p.rk.clone())) {
            return Err(ApiError::new(
                400,
                "InvalidDuplicateRow",
                "The batch request contains multiple changes with same row key. An entity can appear only once in a batch request.",
            )
            .at_index(i));
        }
        rq.grant.check(p.op(), Some((&p.pk, &p.rk)))?;
    }

    let mut done = Vec::new();
    let mut parts = Vec::with_capacity(planned.len());
    for (i, p) in planned.iter().enumerate() {
        let result = match &p.action {
            Action::Insert(payload) => tbl.insert_entity(payload.clone(), rq.now),
            Action::Update { pk, rk, payload, mode, cond } => tbl.update_entity(pk, rk, payload.clone(), *mode, cond, rq.now),
            Action::Delete { pk, rk, cond } => tbl.delete_entity(pk, rk, cond),
        };
        match result {
            Ok(m) => {
                parts.push(success_part(rq, p, tbl, m.stored.as_ref()));
                done.push(m);
            }
            Err(e) => {
                while let Some(m) = done.pop() {
                    tbl.undo(m);
                }
                return Err(if without_index(&e.code) { e } else { e.at_index(i) });
            }
        }
    }
    Ok(parts)
}

/// A single (non change set) request in a batch: only reads are allowed.
fn run_single(rq: &Rq, acct: &mut Account, inner: &Inner) -> String {
    match try_single(rq, acct, inner) {
        Ok(r) => response_part(&r),
        Err(e) => error_part(&e, rq),
    }
}

fn try_single(rq: &Rq, acct: &mut Account, inner: &Inner) -> Res<Response> {
    if inner.method != "GET" {
        return Err(invalid_input());
    }
    let (path, query) = inner.target.split_once('?').unwrap_or((&inner.target, ""));
    let mut headers = http::HeaderMap::new();
    for (k, v) in &inner.headers {
        if let (Ok(n), Ok(val)) = (http::HeaderName::from_bytes(k.as_bytes()), http::HeaderValue::from_str(v)) {
            headers.insert(n, val);
        }
    }
    let sub = Request {
        method: http::Method::GET,
        path: path.to_string(),
        query: azemu_core::Query::parse(query),
        headers,
        body: bytes::Bytes::new(),
        account: rq.req.account.clone(),
        ctx: rq.req.ctx.clone(),
    };
    let sub_rq =
        Rq { req: &sub, base: rq.base.clone(), level: Level::from_accept(inner.header("accept")), grant: rq.grant.clone(), now: rq.now };
    let target = routing::parse(&inner_target(rq, inner));
    match target {
        Target::Entity { table, pk, rk } => {
            check_name(&table)?;
            crate::entity_api::get(&sub_rq, acct, &table, &pk, &rk)
        }
        Target::Query(table) => {
            check_name(&table)?;
            crate::entity_api::query(&sub_rq, acct, &table)
        }
        _ => Err(invalid_input()),
    }
}

fn response_part(r: &Response) -> String {
    let mut h: Vec<(&str, String)> = vec![("DataServiceVersion", "3.0;".into())];
    for name in ["content-type", "x-content-type-options", "cache-control", "etag"] {
        if let Some(v) = r.headers().get(name).and_then(|v| v.to_str().ok()) {
            let display = match name {
                "content-type" => "Content-Type",
                "x-content-type-options" => "X-Content-Type-Options",
                "cache-control" => "Cache-Control",
                _ => "ETag",
            };
            h.push((display, v.to_string()));
        }
    }
    part_text(r.status().as_u16(), &h, &String::from_utf8_lossy(r.body()))
}

fn frame(boundary: &str, parts: &[String]) -> String {
    let mut s = String::new();
    for p in parts {
        s.push_str(&format!("--{boundary}\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\n{p}\r\n"));
    }
    s
}

pub fn handle(rq: &Rq, acct: &mut Account) -> Res<Response> {
    let ct = rq.req.header("content-type").unwrap_or("");
    let unsupported = || ApiError::new(415, "MediaTypeNotSupported", "None of the provided media types are supported");
    if multipart::boundary(ct).is_none() {
        return Err(unsupported());
    }
    let body = String::from_utf8_lossy(&rq.req.body).into_owned();
    let parts = multipart::parse(ct, &body).map_err(|_| invalid_input())?;

    let batch_id = format!("batchresponse_{}", uuid::Uuid::new_v4());
    let mut out = String::new();
    for part in &parts {
        match part {
            Part::Single(inner) => {
                out.push_str(&frame(&batch_id, &[run_single(rq, acct, inner)]));
            }
            Part::ChangeSet(ops) => {
                let cs_id = format!("changesetresponse_{}", uuid::Uuid::new_v4());
                let results = run_change_set(rq, acct, ops);
                out.push_str(&format!("--{batch_id}\r\nContent-Type: multipart/mixed; boundary={cs_id}\r\n\r\n"));
                out.push_str(&frame(&cs_id, &results));
                out.push_str(&format!("--{cs_id}--\r\n\r\n"));
            }
        }
    }
    out.push_str(&format!("--{batch_id}--\r\n"));
    Ok(base_headers(Resp::new(202u16)).header("Content-Type", format!("multipart/mixed; boundary={batch_id}")).body(out).build())
}
