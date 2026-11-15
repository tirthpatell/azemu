//! The requests that create each kind of item. They are the requests a client sends, handed to the service's own `handle`
//! (no socket, no gateway), so the service decides what is allowed and what its answer is.

use bytes::Bytes;
use http::{header, HeaderMap, HeaderName, HeaderValue, Method};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::json;

use super::parse::Item;
use crate::{Endpoint, Query, Request, RequestCtx, Response};

/// Everything but the unreserved characters is escaped, so a name with a `/` or a `:` stays one path segment.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

fn enc(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT).to_string()
}

/// One request to one service.
pub(super) struct Call {
    /// The host label the service is mounted under (`blob`, `documents`): only used to build a host name.
    label: &'static str,
    account: String,
    method: Method,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Bytes,
}

impl Call {
    fn new(label: &'static str, account: &str, method: Method, path: String) -> Call {
        Call { label, account: account.to_string(), method, path, query: String::new(), headers: Vec::new(), body: Bytes::new() }
    }

    fn query(mut self, q: String) -> Call {
        self.query = q;
        self
    }

    fn header(mut self, k: &str, v: &str) -> Call {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }

    fn json(mut self, v: &serde_json::Value) -> Call {
        self.headers.push(("content-type".into(), "application/json".into()));
        self.body = Bytes::from(v.to_string());
        self
    }

    /// The request as core would hand it to the service. Every service here accepts any bearer token.
    pub(super) fn request(&self) -> Request {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer azemu-seed"));
        for (k, v) in &self.headers {
            if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                headers.insert(k, v);
            }
        }
        let authority = format!("{}.{}.localhost", self.account, self.label);
        Request {
            method: self.method.clone(),
            path: self.path.clone(),
            query: Query::parse(&self.query),
            headers,
            body: self.body.clone(),
            account: self.account.clone(),
            ctx: RequestCtx {
                request_id: uuid::Uuid::new_v4().to_string(),
                time: ::time::OffsetDateTime::now_utc(),
                endpoint: Endpoint { scheme: "http", authority, path_style: false, account: self.account.clone() },
            },
        }
    }
}

/// How to create an item, and how to tell that it is already there.
pub(super) struct Plan {
    /// Asked first: an answer of 200 means the item exists and nothing is created (for the services where creating
    /// again would add a version or replace the value).
    pub probes: Vec<Call>,
    pub create: Call,
    /// Does the answer to `create` say "it already exists"?
    pub already: fn(&Response) -> bool,
}

fn error_code_is(r: &Response, code: &str) -> bool {
    r.headers().get("x-ms-error-code").and_then(|v| v.to_str().ok()) == Some(code)
}

fn never(_: &Response) -> bool {
    false
}

const KEYVAULT_VERSION: &str = "7.5";
const APPCONFIG_VERSION: &str = "2023-11-01";
const COSMOS_VERSION: &str = "2018-12-31";

pub(super) fn plan(item: &Item) -> Plan {
    match item {
        Item::BlobContainer { account, name, public_access, metadata } => {
            let mut c = Call::new("blob", account, Method::PUT, format!("/{}", enc(name))).query("restype=container".into());
            if let Some(a) = public_access {
                c = c.header("x-ms-blob-public-access", a);
            }
            for (k, v) in metadata {
                c = c.header(&format!("x-ms-meta-{k}"), v);
            }
            Plan { probes: vec![], create: c, already: |r| error_code_is(r, "ContainerAlreadyExists") }
        }
        // Creating a queue that exists, with the same (no) metadata, answers 204.
        Item::Queue { account, name } => Plan {
            probes: vec![],
            create: Call::new("queue", account, Method::PUT, format!("/{}", enc(name))),
            already: |r| error_code_is(r, "QueueAlreadyExists"),
        },
        Item::Table { account, name } => {
            let c = Call::new("table", account, Method::POST, "/Tables".into())
                .header("accept", "application/json;odata=nometadata")
                .header("prefer", "return-no-content")
                .json(&json!({"TableName": name}));
            Plan { probes: vec![], create: c, already: |r| error_code_is(r, "TableAlreadyExists") }
        }
        Item::CosmosDatabase { account, id } => {
            let c = cosmos(Call::new("documents", account, Method::POST, "/dbs".into())).json(&json!({"id": id}));
            Plan { probes: vec![], create: c, already: |r| r.status() == http::StatusCode::CONFLICT }
        }
        Item::CosmosContainer { account, database, body } => {
            let c = cosmos(Call::new("documents", account, Method::POST, format!("/dbs/{}/colls", enc(database))))
                .json(&serde_json::Value::Object(body.clone()));
            Plan { probes: vec![], create: c, already: |r| r.status() == http::StatusCode::CONFLICT }
        }
        // Setting a secret that exists adds a version, so look first. A deleted secret that can still be recovered
        // counts as existing too (it is in the saved state, and setting it would be refused).
        Item::Secret { vault, name, value } => {
            let q = format!("api-version={KEYVAULT_VERSION}");
            let get = |kind: &str| Call::new("vault", vault, Method::GET, format!("/{kind}/{}", enc(name))).query(q.clone());
            Plan {
                probes: vec![get("secrets"), get("deletedsecrets")],
                create: Call::new("vault", vault, Method::PUT, format!("/secrets/{}", enc(name)))
                    .query(q.clone())
                    .json(&json!({"value": value})),
                already: never,
            }
        }
        // Setting a key-value that exists replaces it and adds a revision, so look first.
        Item::KeyValue { store, key, label, value, content_type } => {
            let mut q = format!("api-version={APPCONFIG_VERSION}");
            if let Some(l) = label {
                q.push_str(&format!("&label={}", enc(l)));
            }
            let mut body = json!({"value": value});
            if let Some(t) = content_type {
                body["content_type"] = json!(t);
            }
            Plan {
                probes: vec![Call::new("azconfig", store, Method::GET, format!("/kv/{}", enc(key))).query(q.clone())],
                create: Call::new("azconfig", store, Method::PUT, format!("/kv/{}", enc(key))).query(q).json(&body),
                already: never,
            }
        }
    }
}

fn cosmos(c: Call) -> Call {
    c.header("x-ms-version", COSMOS_VERSION).header("authorization", "type=master&ver=1.0&sig=seed")
}

/// An error answer as `<status> <code>: <message>`, in the service's own words: the message of a JSON body, or of an XML one,
/// else the start of the text.
pub(super) fn reason(r: &Response) -> String {
    let code = r.headers().get("x-ms-error-code").and_then(|v| v.to_str().ok()).map(str::to_string);
    let body = String::from_utf8_lossy(r.body());
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| find_message(&v))
        .or_else(|| {
            let s = body.find("<Message>")? + "<Message>".len();
            let e = body[s..].find("</Message>")?;
            Some(body[s..s + e].to_string())
        })
        .or_else(|| Some(body.trim().chars().take(300).collect::<String>()).filter(|s| !s.is_empty()));
    let code = code.or_else(|| serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| find_string(&v, "code")));
    let mut out = r.status().as_u16().to_string();
    if let Some(c) = code {
        out.push_str(&format!(" {c}"));
    }
    if let Some(m) = message {
        out.push_str(&format!(": {}", m.lines().next().unwrap_or("").trim()));
    }
    out
}

fn find_string(v: &serde_json::Value, key: &str) -> Option<String> {
    match v {
        serde_json::Value::Object(m) => m
            .iter()
            .find(|(k, x)| k.eq_ignore_ascii_case(key) && x.is_string())
            .and_then(|(_, x)| x.as_str().map(str::to_string))
            .or_else(|| m.values().find_map(|x| find_string(x, key))),
        _ => None,
    }
}

/// `message` (Key Vault, Cosmos, App Configuration), or `odata.error.message.value` (Table).
fn find_message(v: &serde_json::Value) -> Option<String> {
    find_string(v, "message").or_else(|| find_string(v, "value"))
}
