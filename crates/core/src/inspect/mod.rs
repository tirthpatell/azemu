//! The inspector: a read-only tree over the state of every service (`docs/inspector.md`).
//!
//! `GET /_azemu/inspect/{service}/{account}/...` answers one [`Node`] with a shape that is the same everywhere, so one client (the
//! page at `/_azemu/ui`, a script) can browse every service. A service implements [`Service::inspect`](crate::Service::inspect)
//! against its own state; this module holds the shape, the paging and the helpers the services share, and the route in [`route`].
//!
//! What an inspector must keep to: it only reads. Nothing is locked, received or counted as delivered, no clock or sequence number
//! moves, and no account comes into being because someone looked at its path (a path that names an account that does not exist
//! is a 404).

use std::collections::BTreeMap;
use std::ops::Range;

use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64_URL};
use base64::Engine;
use http::StatusCode;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{json, Map, Value};

use crate::Query;

pub(crate) mod route;
#[cfg(feature = "testing")]
pub mod walk;

/// Rows per page when `?limit=` is not given.
pub const DEFAULT_LIMIT: usize = 50;
/// The most rows one answer carries, whatever `?limit=` asks for.
pub const MAX_LIMIT: usize = 500;
/// How much of a body or a blob one answer carries: the first 64 KiB.
pub const BODY_LIMIT: usize = 64 * 1024;
/// The most one row of a list carries: a body at its 64 KiB and the properties around it. A value that does not fit is shown by what it is and how
/// long it is ([`cut_form`]), the largest first.
pub const ROW_LIMIT: usize = 2 * BODY_LIMIT;
/// The most the rows of one answer carry together. A page that would be longer stops early and its `next` marker goes on from there.
pub const PAGE_LIMIT: usize = 4 * 1024 * 1024;

/// What goes into a path segment unescaped: letters, digits and `-._~`; everything else (a `/` in a blob name, a space, `%`) is escaped.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// Escape one name for use as a path segment of a node (`a/b c` becomes `a%2Fb%20c`).
pub fn seg(name: &str) -> String {
    utf8_percent_encode(name, SEGMENT).to_string()
}

/// Why a node could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectError {
    /// This service has no inspector (the default of [`Service::inspect`](crate::Service::inspect)).
    NoInspector,
    /// The path names nothing: an account, container or queue that does not exist. 404.
    NotFound(String),
    /// A parameter the node cannot use. 400.
    BadRequest(String),
}

impl InspectError {
    pub fn not_found(message: impl Into<String>) -> InspectError {
        InspectError::NotFound(message.into())
    }
    pub fn bad_request(message: impl Into<String>) -> InspectError {
        InspectError::BadRequest(message.into())
    }
    pub(crate) fn status_code(&self) -> (StatusCode, &'static str, String) {
        match self {
            InspectError::NoInspector => (StatusCode::NOT_FOUND, "NoInspector", "This service has no inspector.".to_string()),
            InspectError::NotFound(m) => (StatusCode::NOT_FOUND, "NotFound", m.clone()),
            InspectError::BadRequest(m) => (StatusCode::BAD_REQUEST, "InvalidParameter", m.clone()),
        }
    }
}

/// The answer of [`Service::inspect`](crate::Service::inspect).
pub type InspectResult = Result<Node, InspectError>;

/// One question to a service's inspector.
#[derive(Debug, Clone)]
pub struct InspectRequest {
    service: String,
    /// The path below the service name, percent-decoded: `[account, ...]`. Empty for the service's own node.
    pub segments: Vec<String>,
    pub query: Query,
    limit: usize,
    marker: Option<String>,
    /// How the service paged (see [`Paging`]): what core needs to end a page early when its rows are too long.
    paging: std::sync::Arc<parking_lot::Mutex<Paging>>,
}

/// How a node was paged, written down by [`InspectRequest::page_range`] and [`InspectRequest::page_keyed`].
#[derive(Debug, Default)]
pub(crate) enum Paging {
    #[default]
    Unknown,
    /// Rows `start..start + count` of a list held in order.
    Index { start: usize, count: usize },
    /// The keys of the rows of the page, in order.
    Keyed(Vec<String>),
}

impl InspectRequest {
    /// Built by core from the path and query of the request.
    pub fn new(service: &str, segments: Vec<String>, query: Query) -> Result<InspectRequest, InspectError> {
        let limit = match query.get("limit") {
            None | Some("") => DEFAULT_LIMIT,
            Some(v) => match v.parse::<usize>() {
                Ok(n) if n >= 1 => n.min(MAX_LIMIT),
                _ => return Err(InspectError::bad_request(format!("limit must be a number from 1 to {MAX_LIMIT}; got '{v}'."))),
            },
        };
        let marker = match query.get("marker") {
            None | Some("") => None,
            Some(m) => match B64_URL.decode(m).ok().and_then(|b| String::from_utf8(b).ok()) {
                Some(s) => Some(s),
                None => {
                    return Err(InspectError::bad_request(
                        "marker is not one this inspector gave out (it is opaque: use the 'next' of the previous answer).",
                    ));
                }
            },
        };
        Ok(InspectRequest { service: service.to_string(), segments, query, limit, marker, paging: Default::default() })
    }

    /// A request for tests of a service's inspector: the segments are given directly, no `?limit=` or `?marker=`.
    pub fn for_test(service: &str, segments: &[&str], query: &str) -> InspectRequest {
        InspectRequest::new(service, segments.iter().map(|s| s.to_string()).collect(), Query::parse(query)).expect("a valid test request")
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    /// Rows wanted in this page.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The decoded marker of the page asked for: the key to start after (or a number, for a node that pages by position). `None`: the first page.
    pub fn marker(&self) -> Option<&str> {
        self.marker.as_deref()
    }

    /// The segment at `i`, or `None`.
    pub fn seg(&self, i: usize) -> Option<&str> {
        self.segments.get(i).map(String::as_str)
    }

    /// A query parameter (`?prefix=`, `?partition=`).
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.get(name).filter(|v| !v.is_empty())
    }

    /// Is the parameter given as `1`, `true` or with no value (`?content`, `?content=1`)?
    pub fn flag(&self, name: &str) -> bool {
        match self.query.get(name) {
            None => false,
            Some(v) => v.is_empty() || v == "1" || v.eq_ignore_ascii_case("true"),
        }
    }

    /// The path of a node of this service: `req.path(&["myaccount", "containers"])` is `/blob/myaccount/containers`. Each name is escaped.
    pub fn path(&self, parts: &[&str]) -> String {
        let mut p = format!("/{}", self.service);
        for part in parts {
            p.push('/');
            p.push_str(&seg(part));
        }
        p
    }

    /// The path of the node asked for, as the request spelled it.
    pub fn own_path(&self) -> String {
        let parts: Vec<&str> = self.segments.iter().map(String::as_str).collect();
        self.path(&parts)
    }

    /// For a node that pages by position: the first row of this page.
    pub fn start_index(&self) -> usize {
        self.marker.as_deref().and_then(|m| m.parse::<usize>().ok()).unwrap_or(0)
    }

    /// For a list held in order in memory: the rows of this page (`len` is how many there are) and the marker of the next page.
    pub fn page_range(&self, len: usize) -> (Range<usize>, Option<String>) {
        let start = self.start_index().min(len);
        let end = start.saturating_add(self.limit).min(len);
        *self.paging.lock() = Paging::Index { start, count: end - start };
        (start..end, (end < len).then(|| marker_for(&end.to_string())))
    }

    /// For rows read in key order from a position after the marker: takes up to `limit` rows and the marker of the next page. `rows` yields
    /// `(key, row)` already positioned after [`marker`](InspectRequest::marker) (`BTreeMap::range`), in ascending key order; the
    /// iterator is read for one row more than the page, to know whether there is a next one, and no further.
    pub fn page_keyed<K: AsRef<str>, T>(&self, rows: impl Iterator<Item = (K, T)>) -> (Vec<T>, Option<String>) {
        let mut out = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        let mut more = false;
        for (k, v) in rows {
            if out.len() == self.limit {
                more = true;
                break;
            }
            keys.push(k.as_ref().to_string());
            out.push(v);
        }
        let next = if more { keys.last().map(|k| marker_for(k)) } else { None };
        *self.paging.lock() = Paging::Keyed(keys);
        (out, next)
    }

    /// Rows of a `BTreeMap` with string keys, after the marker, in key order.
    pub fn page_map<'a, V>(&self, map: &'a BTreeMap<String, V>) -> (Vec<(&'a String, &'a V)>, Option<String>) {
        use std::ops::Bound::{Excluded, Unbounded};
        let start = match self.marker() {
            Some(m) => Excluded(m.to_string()),
            None => Unbounded,
        };
        let (rows, next) = self.page_keyed(map.range((start, Unbounded)).map(|(k, v)| (k.as_str(), (k, v))));
        (rows, next)
    }

    /// Does the filter `?name=` (a substring) accept `text`? With no filter given, everything.
    pub fn matches_filter(&self, name: &str, text: &str) -> bool {
        self.param(name).is_none_or(|f| text.contains(f))
    }
}

/// The opaque marker for a key.
pub fn marker_for(key: &str) -> String {
    B64_URL.encode(key.as_bytes())
}

/// A link to a child node.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Child {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    pub path: String,
}

/// One node of the tree. See the module comment; the JSON is `{path, kind, title, properties, children, items, next}`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Node {
    pub path: String,
    pub kind: String,
    pub title: String,
    pub properties: Map<String, Value>,
    pub children: Vec<Child>,
    pub items: Vec<Value>,
    pub next: Option<String>,
    /// The content of a blob or file, when `?content=1` asked for it (see [`body_view`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    /// The query parameters this node understands (`prefix`, `partition`): the page offers an input for each.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<String>,
    /// Other ways to look at this node, each a button on the page that asks again with `query` added (`content=1`, `reveal=1`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<View>,
}

/// A button of the page: look at the same node again with this query.
#[derive(Debug, Clone, serde::Serialize)]
pub struct View {
    pub label: String,
    pub query: String,
}

impl Node {
    pub fn new(path: impl AsRef<str>, kind: &str, title: impl AsRef<str>) -> Node {
        Node {
            path: path.as_ref().to_string(),
            kind: kind.to_string(),
            title: title.as_ref().to_string(),
            properties: Map::new(),
            children: Vec::new(),
            items: Vec::new(),
            next: None,
            content: None,
            parameters: Vec::new(),
            views: Vec::new(),
        }
    }

    /// The query parameters this node understands (`?prefix=`).
    pub fn accepts(mut self, names: &[&str]) -> Node {
        self.parameters = names.iter().map(|n| n.to_string()).collect();
        self
    }

    /// A button that asks for this node again with `query` added.
    pub fn view(mut self, label: &str, query: &str) -> Node {
        self.views.push(View { label: label.to_string(), query: query.to_string() });
        self
    }

    /// A property. `null` values are kept (the page shows them as a dash), so a missing setting and an unset one look different.
    pub fn prop(mut self, name: &str, value: impl serde::Serialize) -> Node {
        self.properties.insert(name.to_string(), serde_json::to_value(value).unwrap_or(Value::Null));
        self
    }

    /// A property that is left out when `None`.
    pub fn prop_opt(self, name: &str, value: Option<impl serde::Serialize>) -> Node {
        match value {
            Some(v) => self.prop(name, v),
            None => self,
        }
    }

    pub fn child(mut self, name: &str, kind: &str, count: Option<u64>, path: String) -> Node {
        self.children.push(Child { name: name.to_string(), kind: kind.to_string(), count, path });
        self
    }

    pub fn with_items(mut self, items: Vec<Value>, next: Option<String>) -> Node {
        self.items = items;
        self.next = next;
        self
    }

    pub fn with_content(mut self, content: Value) -> Node {
        self.content = Some(content);
        self
    }
}

/// The size of a value as compact JSON, counted only up to `stop` (a value that is longer answers a little over it, and the rest is not walked).
pub fn json_size(v: &Value, stop: usize) -> usize {
    fn walk(v: &Value, n: &mut usize, stop: usize) {
        if *n > stop {
            return;
        }
        match v {
            Value::Null => *n += 4,
            Value::Bool(_) => *n += 5,
            Value::Number(x) => *n += x.to_string().len(),
            // an escaped character may take up to six bytes; counting the text itself is near enough for a limit
            Value::String(t) => *n += t.len() + 2,
            Value::Array(a) => {
                *n += 2 + a.len();
                for i in a {
                    walk(i, n, stop);
                }
            }
            Value::Object(m) => {
                *n += 2 + m.len() * 2;
                for (k, i) in m {
                    *n += k.len() + 2;
                    walk(i, n, stop);
                }
            }
        }
    }
    let mut n = 0;
    walk(v, &mut n, stop);
    n
}

/// What a row shows for a value that did not fit in it: that it was cut, how long the whole was, and the first of it.
/// (Cosmos DB's documents say `{"shownInTheNode": true, "bytes": n}`: there the node of the document has all of it.)
pub fn cut_form(v: &Value) -> Value {
    let text = v.to_string();
    let mut end = text.len().min(256);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({"cut": true, "bytes": text.len(), "preview": &text[..end]})
}

/// A row that fits in [`ROW_LIMIT`]: while it is longer, its largest value is replaced by [`cut_form`]. A row that is not an object is cut whole.
pub fn cap_row(row: Value) -> Value {
    if json_size(&row, ROW_LIMIT) <= ROW_LIMIT {
        return row;
    }
    let Value::Object(mut m) = row else { return cut_form(&row) };
    loop {
        let sizes: Vec<(String, usize)> = m.iter().map(|(k, v)| (k.clone(), json_size(v, usize::MAX / 4))).collect();
        let total: usize = sizes.iter().map(|(k, n)| n + k.len() + 4).sum();
        if total <= ROW_LIMIT {
            return Value::Object(m);
        }
        let Some((k, n)) =
            sizes.into_iter().filter(|(k, _)| m.get(k).is_some_and(|v| v.get("cut") != Some(&Value::Bool(true)))).max_by_key(|(_, n)| *n)
        else {
            return Value::Object(m);
        };
        // a value that is no longer than its cut form is not worth cutting: what is left is what the row is
        if n <= 300 {
            return Value::Object(m);
        }
        let v = m.remove(&k).unwrap_or(Value::Null);
        m.insert(k, cut_form(&v));
    }
}

impl InspectRequest {
    /// Applied by core to every answer of a service: each row within [`ROW_LIMIT`], the rows together within [`PAGE_LIMIT`]. A page that is
    /// too long stops early and `next` is made to go on from the first row left out, from what [`page_range`](InspectRequest::page_range) or
    /// [`page_keyed`](InspectRequest::page_keyed) wrote down (when the service paged some other way, the rows are capped and the page is left).
    pub(crate) fn limit_answer(&self, mut node: Node) -> Node {
        node.items = std::mem::take(&mut node.items).into_iter().map(cap_row).collect();
        let mut total = 0usize;
        let mut keep = node.items.len();
        for (i, row) in node.items.iter().enumerate() {
            total += json_size(row, PAGE_LIMIT);
            if total > PAGE_LIMIT && i > 0 {
                keep = i;
                break;
            }
        }
        if keep < node.items.len() {
            let next = match &*self.paging.lock() {
                Paging::Index { start, count } if *count == node.items.len() => Some(marker_for(&(start + keep).to_string())),
                Paging::Keyed(keys) if keys.len() == node.items.len() => keys.get(keep - 1).map(|k| marker_for(k)),
                _ => None,
            };
            if next.is_some() {
                node.items.truncate(keep);
                node.next = next;
            }
        }
        node
    }
}

/// A time as the page shows it: RFC 3339 in UTC with milliseconds.
pub fn time_value(t: time::OffsetDateTime) -> Value {
    Value::String(rfc3339(t))
}

pub fn rfc3339(t: time::OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.nanosecond() / 1_000_000
    )
}

/// A time given as seconds since 1970, as [`time_value`].
pub fn unix_time_value(secs: i64) -> Value {
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(t) => time_value(t),
        Err(_) => Value::Null,
    }
}

/// Bytes as the page shows a body: `{size, shown, truncated, text}` when the first 64 KiB are text (valid UTF-8 without control
/// characters other than tab, newline and return), otherwise `{size, shown, truncated, base64}`. `contentType` is added when given.
pub fn body_view(bytes: &[u8], content_type: Option<&str>) -> Value {
    body_view_limited(bytes, content_type, BODY_LIMIT)
}

pub fn body_view_limited(bytes: &[u8], content_type: Option<&str>, limit: usize) -> Value {
    let size = bytes.len();
    let mut shown = &bytes[..size.min(limit)];
    let mut v = Map::new();
    v.insert("size".into(), json!(size));
    if let Some(ct) = content_type {
        v.insert("contentType".into(), json!(ct));
    }
    // a cut in the middle of a character is not a reason to call the body binary
    if shown.len() < size {
        if let Err(e) = std::str::from_utf8(shown) {
            if e.error_len().is_none() {
                shown = &shown[..e.valid_up_to()];
            }
        }
    }
    v.insert("shown".into(), json!(shown.len()));
    v.insert("truncated".into(), json!(shown.len() < size));
    match std::str::from_utf8(shown) {
        Ok(s) if !s.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) => {
            v.insert("text".into(), json!(s));
        }
        _ => {
            v.insert("base64".into(), json!(B64.encode(shown)));
        }
    }
    Value::Object(v)
}

/// A queue message text: shown as it is, and when it is base64 of UTF-8 text, decoded as well (`decoded`).
pub fn text_with_decoded_base64(text: &str) -> Value {
    let mut v = Map::new();
    v.insert("text".into(), json!(text));
    let t = text.trim();
    if t.len() >= 4 && t.len().is_multiple_of(4) {
        if let Some(s) = B64.decode(t).ok().and_then(|b| String::from_utf8(b).ok()) {
            if !s.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) {
                v.insert("decoded".into(), json!(s));
            }
        }
    }
    Value::Object(v)
}

#[cfg(test)]
mod tests;
