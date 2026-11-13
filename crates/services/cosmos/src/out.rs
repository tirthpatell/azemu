//! Request context, handler results and the response headers every Cosmos reply carries.

use std::cell::RefCell;

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use serde_json::Value;

use crate::errors::{CosmosError, R};

/// What a handler needs to know about the request.
pub struct Cx<'a> {
    pub req: &'a Request,
    /// Request time, unix seconds.
    pub now: i64,
    pub activity: String,
    /// The request body, parsed before the service lock is taken (see `preparse`).
    parsed: RefCell<Option<R<Value>>>,
    /// Kept query results (continuation tokens); set by the service.
    pub cache: Option<std::sync::Arc<crate::paging::PageCache>>,
    /// Script settings; set by the service.
    pub settings: Option<std::sync::Arc<crate::js::ScriptSettings>>,
    /// The body is a HybridRow batch (the .NET SDK) that `preparse` turned into the JSON operations: the answer
    /// is written in HybridRow too.
    hybrid_batch: std::cell::Cell<bool>,
}

/// First byte of a Cosmos binary JSON buffer. No UTF-8 text starts with it, so one byte tells the formats apart.
const BINARY_JSON_PREAMBLE: u8 = 0x80;

/// What a client that sends binary JSON is told. azemu reads and writes text JSON only (answers are always text,
/// whatever `x-ms-cosmos-supported-serialization-formats` says; the SDKs tell the two apart by the first byte).
const BINARY_JSON_REFUSED: &str = "The request payload is Cosmos binary JSON, which the azemu emulator does not support (Microsoft's Linux emulator has the same limit). \
Turn binary encoding off in the client: for the Rust SDK set AZURE_COSMOS_BINARY_ENCODING_ENABLED=false or use BinaryEncodingOptions::new().with_enabled(false).";

impl<'a> Cx<'a> {
    pub fn new(req: &'a Request) -> Cx<'a> {
        let activity =
            req.header("x-ms-activity-id").filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| req.ctx.request_id.clone());
        Cx {
            req,
            now: req.ctx.time.unix_timestamp(),
            activity,
            parsed: RefCell::new(None),
            cache: None,
            settings: None,
            hybrid_batch: std::cell::Cell::new(false),
        }
    }

    /// Parse the JSON body now, so that no parsing happens while the service lock is held.
    /// `body_json` hands the result out; a parse error is reported only when a handler asks.
    pub fn preparse(&self) {
        if self.req.body.is_empty() {
            return;
        }
        let ct = self.header("content-type").unwrap_or("").to_ascii_lowercase();
        if ct.contains("application/sql") {
            return;
        }
        // The .NET SDK sends a batch as a HybridRow record stream (no content type, first byte 0x81).
        if self.req.method == "POST" && crate::hybridrow::is_hybrid_row(&self.req.body) && self.flag("x-ms-cosmos-is-batch-request") {
            let ops = crate::hybridrow::decode_operations(&self.req.body, crate::batch::MAX_OPS)
                .map(Value::Array)
                .map_err(|m| CosmosError::bad_request(format!("The batch request body is not a valid HybridRow payload: {m}.")));
            self.hybrid_batch.set(ops.is_ok());
            *self.parsed.borrow_mut() = Some(ops);
            return;
        }
        *self.parsed.borrow_mut() = Some(self.parse_body_now());
    }

    /// True when the request body was a HybridRow batch (see `preparse`).
    pub fn is_hybrid_batch(&self) -> bool {
        self.hybrid_batch.get()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.req.header(name)
    }

    /// A boolean request header ("true"/"True").
    pub fn flag(&self, name: &str) -> bool {
        self.header(name).map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false)
    }

    pub fn body_json(&self) -> R<Value> {
        if let Some(r) = self.parsed.borrow_mut().take() {
            return r;
        }
        self.parse_body_now()
    }

    fn parse_body_now(&self) -> R<Value> {
        if self.req.body.is_empty() {
            return Err(CosmosError::bad_request("The request payload is invalid: the body is empty."));
        }
        if self.req.body.first() == Some(&BINARY_JSON_PREAMBLE) {
            return Err(CosmosError::bad_request(BINARY_JSON_REFUSED));
        }
        serde_json::from_slice(&self.req.body).map_err(|e| {
            // serde_json stops at 128 levels with "recursion limit exceeded"
            if e.to_string().starts_with("recursion limit exceeded") {
                crate::limits::nesting_error()
            } else {
                CosmosError::bad_request("The request payload is not valid JSON.")
            }
        })
    }

    pub fn body_object(&self) -> R<serde_json::Map<String, Value>> {
        match self.body_json()? {
            Value::Object(m) => Ok(m),
            _ => Err(CosmosError::bad_request("The request body must be a JSON object.")),
        }
    }

    pub fn max_item_count(&self) -> Option<usize> {
        self.header("x-ms-max-item-count").and_then(|s| s.trim().parse::<i64>().ok()).filter(|n| *n > 0).map(|n| n as usize)
    }
}

pub enum Body {
    None,
    Json(Value),
    /// A query that still has to run. The service lock is released first, so a long query does
    /// not block other requests (see `doc_query::QueryJob`).
    Deferred(Box<crate::doc_query::QueryJob>),
    /// A stored procedure or a request with triggers, to run after the lock is released (`script_run.rs`).
    Script(Box<crate::script_run::ScriptJob>),
}

/// A successful handler result; `finish` turns it into a response.
pub struct Out {
    pub status: u16,
    pub body: Body,
    pub headers: Vec<(String, String)>,
    pub charge: f64,
    /// Container LSN, which becomes `lsn` and the session token.
    pub lsn: Option<u64>,
    /// `x-ms-alt-content-path`, e.g. `dbs/mydb/colls/mycoll`.
    pub alt_path: Option<String>,
    /// `x-ms-content-path`, the rid path.
    pub content_path: Option<String>,
    /// Charge a point operation by the size of the body, measured when the response is written
    /// (outside the lock): `(write, drop_body)`; `drop_body` is `Prefer: return=minimal`.
    pub doc_charge: Option<(bool, bool)>,
    /// A body that is not JSON (the HybridRow answer of a batch) and its content type; replaces `body`.
    pub raw: Option<(Vec<u8>, &'static str)>,
}

impl Out {
    pub fn json(status: u16, v: Value) -> Out {
        Out {
            status,
            body: Body::Json(v),
            headers: Vec::new(),
            charge: 1.0,
            lsn: None,
            alt_path: None,
            content_path: None,
            doc_charge: None,
            raw: None,
        }
    }

    /// An answer whose body is bytes, not JSON.
    pub fn raw(status: u16, bytes: Vec<u8>, content_type: &'static str) -> Out {
        Out { raw: Some((bytes, content_type)), ..Out::empty(status) }
    }

    /// A script to run after the lock is released.
    pub fn script(job: crate::script_run::ScriptJob) -> Out {
        Out { body: Body::Script(Box::new(job)), ..Out::json(200, Value::Null) }
    }

    /// A query to run after the lock is released.
    pub fn deferred(job: crate::doc_query::QueryJob) -> Out {
        Out { body: Body::Deferred(Box::new(job)), ..Out::json(200, Value::Null) }
    }

    pub fn charge_by_size(mut self, write: bool, drop_body: bool) -> Out {
        self.doc_charge = Some((write, drop_body));
        self
    }

    pub fn empty(status: u16) -> Out {
        Out { body: Body::None, ..Out::json(status, Value::Null) }
    }

    /// The 304 answer of a feed read that has nothing new. Azure sends `x-ms-item-count: 0` with it, and the
    /// .NET SDK parses that header without a null check (`NetworkAttachedDocumentContainer`: `int.Parse`),
    /// so a feed answer without it throws `ArgumentNullException` in the client. `etag` is the continuation
    /// the client sends next time.
    pub fn feed_not_modified(etag: impl ToString) -> Out {
        Out::empty(304).header("etag", etag).header("x-ms-item-count", 0)
    }

    pub fn header(mut self, k: &str, v: impl ToString) -> Out {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }

    pub fn charge(mut self, c: f64) -> Out {
        self.charge = c;
        self
    }

    pub fn lsn(mut self, l: u64) -> Out {
        self.lsn = Some(l);
        self
    }

    pub fn paths(mut self, alt: String, content: String) -> Out {
        self.alt_path = Some(alt);
        self.content_path = Some(content);
        self
    }
}

/// Request charge for a point operation on a document of `bytes` size.
pub fn doc_charge(write: bool, bytes: usize) -> f64 {
    let kb = (bytes as f64 / 1024.0).max(1.0);
    let c = if write { 5.71 + (kb - 1.0) * 5.0 } else { 1.0 + (kb - 1.0) * 1.0 };
    (c * 100.0).round() / 100.0
}

/// Request charge for a query that read `docs` documents and returned `out` of them.
pub fn query_charge(docs: usize, out: usize) -> f64 {
    let c = 2.37 + docs as f64 * 0.05 + out as f64 * 0.02;
    (c * 100.0).round() / 100.0
}

fn common(mut r: Resp, cx: &Cx, charge: f64) -> Resp {
    r = r
        .header("x-ms-activity-id", &cx.activity)
        .header("x-ms-request-charge", format!("{charge:.2}"))
        .header("x-ms-schemaversion", "1.18")
        .header("x-ms-serviceversion", " version=2.14.0")
        .header("x-ms-gatewayversion", "version=2.14.0")
        .header("x-ms-last-state-change-utc", "Tue, 10 Nov 2026 08:00:00.000 GMT")
        .header("x-ms-resource-quota", "documentSize=10240;documentsSize=10485760;documentsCount=-1;collectionSize=10485760;")
        .header("x-ms-resource-usage", "documentSize=0;documentsSize=0;documentsCount=0;collectionSize=0;")
        .header("x-ms-xp-role", "1")
        .header("x-ms-number-of-read-regions", "0")
        .header("Cache-Control", "no-store, no-cache")
        .header("Strict-Transport-Security", "max-age=31536000");
    r
}

/// Turn a handler result into the HTTP response.
/// The operation of a request as `GET /_azemu/unsupported` shows it: the method and the resource types in the path
/// (`PATCH dbs/colls/docs`), without names.
fn operation_name(cx: &Cx) -> String {
    let segs = cx.req.segments();
    let mut types: Vec<&str> = segs.iter().enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, s)| s.as_str()).collect();
    if segs.len().is_multiple_of(2) && segs.len() >= 2 && segs[segs.len() - 2] == "operations" {
        // `operations/partitionkeydelete`: the second word is the operation, not a name
        types.push(segs[segs.len() - 1].as_str());
    }
    if types == ["addresses"] {
        return format!("{} /addresses (direct mode)", cx.req.method);
    }
    format!("{} /{}", cx.req.method, types.join("/"))
}

pub fn finish(cx: &Cx, result: R<Out>) -> Response {
    match result {
        Ok(mut out) => {
            let raw = out.raw.take();
            // serialise first: the size of a document decides its request charge
            let mut bytes: Option<Vec<u8>> = match &out.body {
                Body::Json(v) => Some(serde_json::to_vec(v).unwrap_or_default()),
                Body::None | Body::Deferred(_) | Body::Script(_) => None,
            };
            let mut charge = out.charge;
            if let Some((write, drop_body)) = out.doc_charge {
                charge = doc_charge(write, bytes.as_ref().map(Vec::len).unwrap_or(0));
                if drop_body {
                    bytes = None;
                }
            }
            let mut r = Resp::new(out.status);
            r = common(r, cx, charge);
            if let Some(l) = out.lsn {
                r = r
                    .header("lsn", l)
                    .header("x-ms-session-token", format!("0:1#{l}"))
                    .header("x-ms-global-committed-lsn", l)
                    .header("x-ms-item-lsn", l);
            }
            if let Some(p) = &out.alt_path {
                r = r.header("x-ms-alt-content-path", p);
            }
            if let Some(p) = &out.content_path {
                r = r.header("x-ms-content-path", p);
            }
            for (k, v) in &out.headers {
                r = r.header(k, v);
            }
            if let Some((b, content_type)) = raw {
                return r.header("Content-Type", content_type).body(b).build();
            }
            match bytes {
                None => r.build(),
                Some(b) => r.header("Content-Type", "application/json").body(b).build(),
            }
        }
        Err(e) => {
            let mut r = Resp::new(e.status);
            r = common(r, cx, 1.0);
            if let Some(s) = e.substatus {
                r = r.header("x-ms-substatus", s);
            }
            for (k, v) in &e.headers {
                r = r.header(k, v);
            }
            let resp = r.json(&e.body(&cx.activity)).header("Content-Type", "application/json").build();
            if e.status == 501 {
                // names the operation for `GET /_azemu/unsupported`
                return azemu_core::unsupported::mark(resp, operation_name(cx));
            }
            resp
        }
    }
}
