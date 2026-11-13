//! What a script reaches through `__host`: the documents of one logical partition.
//!
//! The script works on a private copy of the partition (`Container::snapshot`). Each collection call is turned into
//! the request the REST API would get and handed to the ordinary document handlers (`documents.rs`), so validation,
//! unique keys, ETags, `If-Match` and the error answers are the same as outside a script. The copy is applied to the
//! real container only when the script ends well (`scripts.rs`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::bytes::Bytes;
use azemu_core::http::{HeaderMap, HeaderName, HeaderValue, Method};
use azemu_core::Request;
use serde_json::{json, Map, Value};

use crate::documents::{self, Loc};
use crate::errors::CosmosError;
use crate::out::{doc_charge, query_charge, Body, Cx};
use crate::pk::PkValue;
use crate::query::{self, QueryBody};
use crate::state::Container;

const MAX_LOG_BYTES: usize = 64 * 1024;
const DEFAULT_PAGE: usize = 100;

pub struct ScriptHost {
    pub scratch: Container,
    loc: Loc,
    db_name: String,
    coll_name: String,
    base: Request,
    pk_header: String,
    pub pk: PkValue,
    deadline: Instant,
    /// Collection calls stop being accepted this long before the deadline.
    margin: Duration,
    ops: usize,
    max_ops: usize,
    /// Request charge of everything the script did.
    pub charge: f64,
    pub log: Vec<String>,
    log_bytes: usize,
    first_lsn: u64,
    /// Results of queries the script ran, so that the pages of one query are cut from one result.
    kept: HashMap<String, Arc<Vec<Value>>>,
    udfs: Vec<(String, String)>,
    settings: Arc<crate::js::ScriptSettings>,
    /// Operation names and headers for `getRequest()` in a trigger.
    pub self_link: String,
    pub alt_link: String,
}

pub struct HostInit {
    pub scratch: Container,
    pub loc: Loc,
    pub db_name: String,
    pub coll_name: String,
    pub base: Request,
    pub pk: PkValue,
    pub deadline: Instant,
    pub udfs: Vec<(String, String)>,
    pub settings: Arc<crate::js::ScriptSettings>,
}

/// The partition key as the header text of a request.
pub fn pk_header_text(pk: &PkValue) -> String {
    Value::Array(pk.iter().map(|c| c.to_json()).collect()).to_string()
}

impl ScriptHost {
    pub fn new(i: HostInit) -> ScriptHost {
        let pk_header = i.base.header("x-ms-documentdb-partitionkey").map(str::to_string).unwrap_or_else(|| pk_header_text(&i.pk));
        let self_link = format!("dbs/{}/colls/{}/", i.loc.db_rid, i.scratch.meta.rid);
        let alt_link = format!("dbs/{}/colls/{}", i.db_name, i.coll_name);
        let margin = (i.settings.timeout / 10).max(Duration::from_millis(20));
        let first_lsn = i.scratch.lsn;
        ScriptHost {
            scratch: i.scratch,
            loc: i.loc,
            db_name: i.db_name,
            coll_name: i.coll_name,
            base: i.base,
            pk_header,
            pk: i.pk,
            deadline: i.deadline,
            margin,
            ops: 0,
            max_ops: i.settings.max_operations,
            charge: 0.0,
            log: Vec::new(),
            log_bytes: 0,
            first_lsn,
            kept: HashMap::new(),
            udfs: i.udfs,
            settings: i.settings,
            self_link,
            alt_link,
        }
    }

    /// Whether the script wrote anything.
    pub fn wrote(&self) -> bool {
        self.scratch.lsn != self.first_lsn
    }

    fn accepted(&mut self) -> bool {
        if self.ops >= self.max_ops || Instant::now() + self.margin >= self.deadline {
            return false;
        }
        self.ops += 1;
        true
    }

    /// A request as the REST API would get it from the SDK, for the document handlers.
    fn request(&self, method: &str, tail: &str, body: Option<&str>, extra: &[(&str, &str)]) -> Request {
        let mut r = self.base.clone();
        r.method = Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET);
        r.path = format!("/dbs/{}/colls/{}/docs{}", self.db_name, self.coll_name, tail);
        r.body = Bytes::from(body.unwrap_or("").to_string());
        let mut h = HeaderMap::new();
        let mut put = |k: &str, v: &str| {
            if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                h.insert(k, v);
            }
        };
        put("authorization", "type%3Dmaster%26ver%3D1.0%26sig%3Dscript");
        put("content-type", "application/json");
        put("x-ms-documentdb-partitionkey", &self.pk_header);
        for (k, v) in extra {
            put(k, v);
        }
        r.headers = h;
        r
    }

    /// `dbs/{db}/colls/{coll}` as a link names it: by name or by resource id.
    fn check_collection(&self, segs: &[&str]) -> Result<(), CosmosError> {
        let ok = segs.len() >= 4
            && segs[0] == "dbs"
            && (segs[1] == self.db_name || segs[1] == self.loc.db_rid)
            && segs[2] == "colls"
            && (segs[3] == self.coll_name || segs[3] == self.scratch.meta.rid);
        if ok {
            Ok(())
        } else {
            Err(CosmosError::not_found_default())
        }
    }

    fn collection_link(&self, link: &str) -> Result<(), CosmosError> {
        let segs: Vec<&str> = link.split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() != 4 {
            return Err(CosmosError::bad_request("The collection link is not valid."));
        }
        self.check_collection(&segs)
    }

    /// The id (or `_rid`) a document link names.
    fn document_id(&self, link: &str) -> Result<String, CosmosError> {
        let segs: Vec<&str> = link.split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() != 6 || segs[4] != "docs" {
            return Err(CosmosError::bad_request("The document link is not valid."));
        }
        self.check_collection(&segs)?;
        Ok(percent_decode(segs[5]))
    }

    fn fail(&self, e: &CosmosError) -> Value {
        let message = match &e.raw_message {
            Some(m) => m.clone(),
            None => json!({ "Errors": [e.message] }).to_string(),
        };
        json!({ "accepted": true, "error": { "status": e.status, "message": message, "body": e.body("script").to_string() } })
    }

    fn page_args(&self, options: &Value) -> (usize, usize) {
        let size = options.get("pageSize").and_then(Value::as_u64).filter(|n| *n > 0).map(|n| n as usize).unwrap_or(DEFAULT_PAGE);
        let skip = options
            .get("continuation")
            .and_then(Value::as_str)
            .and_then(|t| t.strip_prefix("azemu-script:"))
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(0);
        (size, skip)
    }

    fn page(&self, rows: &[Value], options: &Value) -> Value {
        let (size, skip) = self.page_args(options);
        let end = skip.saturating_add(size).min(rows.len());
        let items: Vec<Value> = rows.get(skip..end).unwrap_or(&[]).to_vec();
        let mut opts = Map::new();
        if end < rows.len() {
            opts.insert("continuation".into(), json!(format!("azemu-script:{end}")));
        }
        json!({ "accepted": true, "resource": items, "options": opts })
    }

    fn partition_docs(&self) -> Vec<Arc<Value>> {
        self.scratch.partition(&self.pk).filter(|(k, _)| k.pk == self.pk).map(|(_, d)| d.value.clone()).collect()
    }

    fn write_op(&mut self, op: &str, p: &Value) -> Value {
        let link = p.get("link").and_then(Value::as_str).unwrap_or("");
        let options = p.get("options").cloned().unwrap_or(Value::Null);
        let body_text = p.get("body").and_then(Value::as_str).unwrap_or("");
        let doc_op = matches!(op, "replace" | "delete" | "read");
        let id = if doc_op {
            match self.document_id(link) {
                Ok(id) => Some(id),
                Err(e) => return self.fail(&e),
            }
        } else {
            if let Err(e) = self.collection_link(link) {
                return self.fail(&e);
            }
            None
        };
        if !self.accepted() {
            return json!({ "accepted": false });
        }
        let etag = options.get("etag").and_then(Value::as_str).map(str::to_string);
        let if_none = options.get("ifNoneMatch").and_then(Value::as_str).map(str::to_string);
        let mut body = body_text.to_string();
        if matches!(op, "create" | "upsert") && options.get("disableAutomaticIdGeneration").and_then(Value::as_bool) != Some(true) {
            if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(body_text) {
                if !m.contains_key("id") {
                    m.insert("id".into(), json!(uuid::Uuid::new_v4().to_string()));
                    body = Value::Object(m).to_string();
                }
            }
        }
        let mut extra: Vec<(&str, String)> = Vec::new();
        if op == "upsert" {
            extra.push(("x-ms-documentdb-is-upsert", "true".into()));
        }
        if let Some(e) = &etag {
            extra.push(("if-match", e.clone()));
        }
        if let Some(e) = &if_none {
            extra.push(("if-none-match", e.clone()));
        }
        let extra_ref: Vec<(&str, &str)> = extra.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (method, tail, sent): (&str, String, Option<&str>) = match op {
            "create" | "upsert" => ("POST", String::new(), Some(&body)),
            "replace" => ("PUT", format!("/{}", id.clone().unwrap_or_default()), Some(&body)),
            "delete" => ("DELETE", format!("/{}", id.clone().unwrap_or_default()), None),
            _ => ("GET", format!("/{}", id.clone().unwrap_or_default()), None),
        };
        let req = self.request(method, &tail, sent, &extra_ref);
        let cx = Cx::new(&req);
        cx.preparse();
        let result = match op {
            "create" => documents::create(&cx, &self.loc, &mut self.scratch, false),
            "upsert" => documents::create(&cx, &self.loc, &mut self.scratch, true),
            "replace" => documents::replace(&cx, &self.loc, &mut self.scratch, id.as_deref().unwrap_or("")),
            "delete" => documents::delete(&cx, &mut self.scratch, id.as_deref().unwrap_or("")),
            _ => documents::read(&cx, &mut self.scratch, id.as_deref().unwrap_or("")),
        };
        match result {
            Ok(out) => {
                let value = match out.body {
                    Body::Json(v) if out.status != 304 => Some(v),
                    _ => None,
                };
                let size = value.as_ref().map(crate::limits::json_size).unwrap_or(body.len());
                self.charge += doc_charge(op != "read", size);
                let mut o = json!({ "accepted": true });
                if out.status == 304 {
                    o["options"] = json!({ "notModified": true });
                }
                if let Some(v) = value {
                    o["resource"] = v;
                }
                o
            }
            Err(e) => {
                self.charge += 1.0;
                self.fail(&e)
            }
        }
    }

    fn read_feed(&mut self, p: &Value) -> Value {
        let link = p.get("link").and_then(Value::as_str).unwrap_or("");
        if let Err(e) = self.collection_link(link) {
            return self.fail(&e);
        }
        if !self.accepted() {
            return json!({ "accepted": false });
        }
        let options = p.get("options").cloned().unwrap_or(Value::Null);
        let rows: Vec<Value> = self.partition_docs().iter().map(|d| (**d).clone()).collect();
        let r = self.page(&rows, &options);
        self.charge += 2.0 + rows.len().min(DEFAULT_PAGE) as f64 * 0.01;
        r
    }

    fn query(&mut self, p: &Value) -> Value {
        let link = p.get("link").and_then(Value::as_str).unwrap_or("");
        if let Err(e) = self.collection_link(link) {
            return self.fail(&e);
        }
        if !self.accepted() {
            return json!({ "accepted": false });
        }
        let options = p.get("options").cloned().unwrap_or(Value::Null);
        let body_text = p.get("body").and_then(Value::as_str).unwrap_or("{}");
        let Ok(v) = serde_json::from_str::<Value>(body_text) else {
            return self.fail(&CosmosError::bad_request("The query is not valid."));
        };
        let text = v.get("query").and_then(Value::as_str).unwrap_or("").to_string();
        if text.len() > crate::limits::MAX_QUERY_BYTES {
            return self.fail(&CosmosError::bad_request("The query text is too long: the limit is 512 KB."));
        }
        // a query keeps its result from the first page on, so the pages of one query are slices of one answer
        let key = body_text.to_string();
        let rows = match self.kept.get(&key) {
            Some(r) if self.page_args(&options).1 > 0 => r.clone(),
            _ => {
                let qb = QueryBody { text, params_json: v.get("parameters").cloned().unwrap_or(Value::Array(vec![])) };
                let prepared = match query::prepare(&qb) {
                    Ok(p) => p,
                    Err(e) => return self.fail(&e),
                };
                let mut opts = query::eval_options_at(self.base.ctx.time.unix_timestamp());
                match self.udf_host(&prepared.query.udf_names()) {
                    Ok(h) => opts.udf = h,
                    Err(e) => return self.fail(&e),
                }
                let docs = self.partition_docs();
                let rows = match prepared.query.execute_shared(&docs, &prepared.params, &opts) {
                    Ok(r) => r,
                    Err(e) => return self.fail(&CosmosError::from_sql(&e)),
                };
                self.charge += query_charge(docs.len(), rows.len());
                let rows: Arc<Vec<Value>> = Arc::new(rows.into_iter().map(|i| i.into_value()).collect());
                self.kept.insert(key, rows.clone());
                rows
            }
        };
        self.page(&rows, &options)
    }

    /// The runtime for the user-defined functions a query in this script calls.
    fn udf_host(&self, names: &[String]) -> Result<Option<Arc<dyn azemu_cosmos_sql::UdfFactory>>, CosmosError> {
        if names.is_empty() {
            return Ok(None);
        }
        let defs = crate::udf::definitions(&self.udfs, names)?;
        Ok(Some(Arc::new(crate::udf::UdfSource::new(defs, &self.settings, self.deadline))))
    }

    fn all_docs(&mut self) -> Value {
        if !self.accepted() {
            return json!({ "accepted": false });
        }
        let docs: Vec<Value> = self.partition_docs().iter().map(|d| (**d).clone()).collect();
        self.charge += 2.0 + docs.len() as f64 * 0.01;
        json!({ "accepted": true, "docs": docs })
    }

    /// The document operation a request asks for, run on the copy (the same handlers as outside a script).
    pub fn run_doc_op(&mut self, req: &Request, op: crate::script_run::DocOp, id: Option<&str>) -> Result<crate::out::Out, CosmosError> {
        use crate::script_run::DocOp;
        let cx = Cx::new(req);
        cx.preparse();
        match op {
            DocOp::Create { upsert } => documents::create(&cx, &self.loc, &mut self.scratch, upsert),
            DocOp::Replace => documents::replace(&cx, &self.loc, &mut self.scratch, id.unwrap_or("")),
            DocOp::Delete => documents::delete(&cx, &mut self.scratch, id.unwrap_or("")),
        }
    }
}

fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        (c as char).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl ScriptHost {
    /// A collection call (or a log line) of the script, as the worker sent it; the answer is JSON text. Whatever the
    /// worker sends is untrusted: unknown operations are "not accepted", the number of calls is limited (`accepted`).
    pub fn call(&mut self, op: &str, payload: &str) -> String {
        let p: Value = if payload.is_empty() { Value::Null } else { serde_json::from_str(payload).unwrap_or(Value::Null) };
        let answer = match op {
            "log" => {
                if self.log_bytes < MAX_LOG_BYTES {
                    self.log_bytes += payload.len() + 1;
                    self.log.push(payload.to_string());
                }
                return String::new();
            }
            "create" | "upsert" | "replace" | "delete" | "read" => self.write_op(op, &p),
            "readFeed" => self.read_feed(&p),
            "query" => self.query(&p),
            "readAll" => self.all_docs(),
            _ => json!({ "accepted": false }),
        };
        answer.to_string()
    }
}
