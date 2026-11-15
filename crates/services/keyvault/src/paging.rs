//! `maxresults` / `$skiptoken` paging and the `nextLink` of list results.

use azemu_core::{Request, Response};
use serde_json::{json, Value};

use crate::common::{b64u, b64u_decode, json_response};
use crate::errors::bad_parameter;

/// Real Key Vault returns at most 25 items per page.
pub const DEFAULT_PAGE: usize = 25;

pub struct PageParams {
    pub max: usize,
    /// Sort key of the last item of the previous page.
    pub after: Option<String>,
}

pub fn parse(req: &Request) -> Result<PageParams, Response> {
    // Any list takes 1 to 25 (tried in November 2026): 0, 26, 500, -1 and text are all the same answer.
    let max = match req.query.get("maxresults") {
        None | Some("") => DEFAULT_PAGE,
        Some(s) => match s.parse::<usize>() {
            Ok(n) if (1..=DEFAULT_PAGE).contains(&n) => n,
            _ => return Err(bad_parameter("invalid maxresults")),
        },
    };
    let after = match req.query.get("$skiptoken") {
        None | Some("") => None,
        Some(t) => Some(read_token(t).ok_or_else(|| bad_parameter("Invalid skip token"))?),
    };
    Ok(PageParams { max, after: after.flatten() })
}

/// A `$skiptoken` is base64url of a JSON object (the service's own look like `{"NextMarker":"…","TargetLocation":0}`).
/// Text that is not that is refused; a marker this emulator did not write starts at the first page.
fn read_token(token: &str) -> Option<Option<String>> {
    // The recordings of real traffic that azemu is replayed against (`tools/replay`) have every token replaced by this word;
    // it is taken as a token the emulator did not write. No client sends it.
    if token == "Sanitized" {
        return Some(None);
    }
    let json: serde_json::Value = serde_json::from_slice(&b64u_decode(token)?).ok()?;
    let marker = json.as_object()?.get("NextMarker").and_then(|m| m.as_str()).and_then(b64u_decode).and_then(|b| String::from_utf8(b).ok());
    Some(marker)
}

fn make_token(key: &str) -> String {
    b64u(json!({ "NextMarker": b64u(key.as_bytes()), "TargetLocation": 0 }).to_string().as_bytes())
}

/// Take one page out of `items`, which must be sorted by key. Returns the page and
/// the `$skiptoken` for the next one, if there is more.
pub fn page<T>(items: Vec<(String, T)>, p: &PageParams) -> (Vec<T>, Option<String>) {
    let mut rest = items.into_iter().filter(|(k, _)| p.after.as_ref().map(|a| k > a).unwrap_or(true));
    let mut out = Vec::new();
    let mut last_key = None;
    for (k, v) in rest.by_ref().take(p.max) {
        last_key = Some(k);
        out.push(v);
    }
    let more = rest.next().is_some();
    let token = if more { last_key.map(|k| make_token(&k)) } else { None };
    (out, token)
}

/// The list response: `{"value":[…],"nextLink":…|null}`.
pub fn list_response(req: &Request, path: &str, values: Vec<Value>, token: Option<String>) -> Response {
    let next = token.map(|t| {
        let mut url = format!("{}{}?", req.ctx.endpoint.base_url(), path);
        if let Some(v) = req.query.get("api-version") {
            url.push_str(&format!("api-version={v}&"));
        }
        url.push_str(&format!("$skiptoken={t}"));
        if let Some(m) = req.query.get("maxresults") {
            url.push_str(&format!("&maxresults={m}"));
        }
        url
    });
    json_response(200, &json!({ "value": values, "nextLink": next }))
}
