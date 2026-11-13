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
    let max = match req.query.get("maxresults") {
        None | Some("") => DEFAULT_PAGE,
        Some(s) => match s.parse::<usize>() {
            Ok(n) if n >= 1 => n.min(DEFAULT_PAGE),
            _ => return Err(bad_parameter("The 'maxresults' parameter must be a positive integer.")),
        },
    };
    let after = match req.query.get("$skiptoken") {
        None | Some("") => None,
        // A token we did not issue is ignored (the first page comes back) instead of failing.
        Some(t) => b64u_decode(t).and_then(|b| String::from_utf8(b).ok()),
    };
    Ok(PageParams { max, after })
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
    let token = if more { last_key.map(|k| b64u(k.as_bytes())) } else { None };
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
