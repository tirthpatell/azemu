//! `GET/HEAD /kv`, `/revisions`, `/keys` and `/labels`.

use std::collections::BTreeSet;

use azemu_core::time::OffsetDateTime;
use azemu_core::{Request, Response};
use serde_json::{json, Value};

use crate::auth;
use crate::errors;
use crate::filters::{parse_tags, tags_match, Filter};
use crate::keyvalues::{accept_datetime, select_param};
use crate::paging::{decode_cursor, encode_cursor, link_target, memento, page_etag, respond, Page, PAGE_SIZE};
use crate::render::{setting_json, KV_FIELDS};
use crate::state::{Ident, Rev, Setting, Store};
use crate::timefmt::trunc;

const KVSET_CT: &str = "application/vnd.microsoft.appconfig.kvset+json; charset=utf-8";
const KEYSET_CT: &str = "application/vnd.microsoft.appconfig.keyset+json; charset=utf-8";
const LABELSET_CT: &str = "application/vnd.microsoft.appconfig.labelset+json; charset=utf-8";

/// A filter parameter. `shown` is the name the error message uses.
pub fn filter_param(req: &Request, param: &str, label: bool, shown: &str) -> Result<Filter, Response> {
    let Some(text) = req.query.get(param) else {
        return Ok(Filter::any());
    };
    let f = Filter::parse(text, label)
        .map_err(|e| errors::invalid_argument(shown, &format!("{shown}({}): Invalid character", e.pos), Some(e.pos)))?;
    if f.len() > MAX_FILTER_VALUES {
        return Err(errors::invalid_argument(
            shown,
            &format!("{shown}: at most {MAX_FILTER_VALUES} comma-separated values are allowed."),
            None,
        ));
    }
    Ok(f)
}

pub fn tags_param(req: &Request) -> Result<Vec<(String, String)>, Response> {
    parse_tags(req.query.get_all("tags")).map_err(|d| errors::invalid_argument("tags", &d, None))
}

fn after_param(req: &Request) -> Option<String> {
    req.query.get("after").filter(|a| !a.is_empty()).and_then(decode_cursor)
}

fn newer(req: &Request) -> bool {
    auth::is_newer(auth::version(req))
}

fn memento_header(at: Option<OffsetDateTime>) -> Vec<(&'static str, String)> {
    at.map(|t| vec![("Memento-Datetime", memento(t))]).unwrap_or_default()
}

/// Work one list request may do while it holds the lock of the service: one unit for every item looked at and one for every
/// tag of an item compared against a tag filter. When a page is not full by then, the answer is what was found so far and the
/// next link continues after the last item looked at (so a filter that matches nothing in a huge store takes many short requests,
/// not one long one).
pub const SCAN_BUDGET: usize = 200_000;
/// The same for a `Range` read of revisions, which has to count what matches to answer `Content-Range`.
const RANGE_BUDGET: usize = 2_000_000;

/// Most comma-separated values in a `key` or `label` filter and most `tags` filters in a request (Azure: 5).
pub const MAX_FILTER_VALUES: usize = 5;

fn work_of(s: &Setting, tags_wanted: bool) -> usize {
    1 + if tags_wanted { s.tags.len() } else { 0 }
}

fn kv_page(
    req: &Request,
    settings: &[&Setting],
    select: Option<&[&str]>,
    next: Option<(&str, Option<&str>)>,
    at: Option<OffsetDateTime>,
) -> Response {
    let items: Vec<Value> = settings.iter().map(|s| setting_json(s, select, newer(req))).collect();
    let mut page = Page::new(KVSET_CT, items);
    page.etag = Some(page_etag(settings.iter().map(|s| s.etag.as_str())));
    if let Some((key, label)) = next {
        let cursor = encode_cursor(&format!("{key}\n{}", label.unwrap_or("")));
        page.next = Some(link_target(req, "/kv", &cursor));
    }
    page.extra_headers = memento_header(at);
    respond(req, page, newer(req))
}

/// The settings to look through, in listing order, starting after `after`: the live ones, the ones at a time, or a snapshot's.
fn candidates<'a>(store: &'a Store, at: Option<OffsetDateTime>, after: Option<&Ident>) -> Box<dyn Iterator<Item = &'a Setting> + 'a> {
    match at {
        Some(t) => sorted_after(store.at(t), after),
        None => Box::new(store.live_after(after)),
    }
}

fn sorted_after<'a>(sorted: Vec<&'a Setting>, after: Option<&Ident>) -> Box<dyn Iterator<Item = &'a Setting> + 'a> {
    let start = match after {
        Some(c) => sorted.partition_point(|s| (s.key.as_str(), s.label.as_deref()) <= (c.0.as_str(), c.1.as_deref())),
        None => 0,
    };
    Box::new(sorted.into_iter().skip(start))
}

pub fn kv_list(store: &Store, req: &Request) -> Response {
    let (key, label) = match (filter_param(req, "key", false, "key"), filter_param(req, "label", true, "label")) {
        (Ok(k), Ok(l)) => (k, l),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let tags = match tags_param(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let select = match select_param(req, KV_FIELDS) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let at = match accept_datetime(req) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let cursor: Option<Ident> = after_param(req).map(|c| {
        let (k, l) = c.split_once('\n').unwrap_or((c.as_str(), ""));
        (k.to_string(), if l.is_empty() { None } else { Some(l.to_string()) })
    });
    let source: Box<dyn Iterator<Item = &Setting> + '_> = match req.query.get("snapshot") {
        Some(name) => match store.snapshots.get(name) {
            Some(s) => sorted_after(s.items.iter().map(|a| &**a).collect(), cursor.as_ref()),
            None => return errors::not_found(),
        },
        None => candidates(store, at, cursor.as_ref()),
    };
    let tags_wanted = !tags.is_empty();
    let mut page: Vec<&Setting> = Vec::new();
    let mut last: Option<&Setting> = None;
    let mut work = 0;
    let mut more = false;
    for s in source {
        work += work_of(s, tags_wanted);
        last = Some(s);
        if key.matches_str(&s.key) && label.matches(s.label.as_deref()) && tags_match(&s.tags, &tags) {
            page.push(s);
            if page.len() >= PAGE_SIZE {
                more = true;
                break;
            }
        }
        if work >= SCAN_BUDGET {
            more = true;
            break;
        }
    }
    let next = if more { last.map(|s| (s.key.as_str(), s.label.as_deref())) } else { None };
    kv_page(req, &page, select.as_deref(), next, at)
}

/// `Range: items=a-b`
fn parse_range(h: &str) -> Option<(usize, usize)> {
    let r = h.trim().strip_prefix("items=")?;
    let (a, b) = r.split_once('-')?;
    let a: usize = a.trim().parse().ok()?;
    let b: usize = if b.trim().is_empty() { usize::MAX } else { b.trim().parse().ok()? };
    (b >= a).then_some((a, b))
}

pub fn revisions(store: &Store, req: &Request) -> Response {
    let (key, label) = match (filter_param(req, "key", false, "key"), filter_param(req, "label", true, "label")) {
        (Ok(k), Ok(l)) => (k, l),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    let tags = match tags_param(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let select = match select_param(req, KV_FIELDS) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let at = match accept_datetime(req) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let cursor: Option<u64> = after_param(req).and_then(|c| c.parse().ok());
    let tags_wanted = !tags.is_empty();
    let wanted = |r: &Rev| {
        let s = &*r.setting;
        key.matches_str(&s.key)
            && label.matches(s.label.as_deref())
            && tags_match(&s.tags, &tags)
            && at.map(|t| trunc(s.last_modified) <= trunc(t)).unwrap_or(true)
    };
    let mut extra = memento_header(at);
    extra.push(("Accept-Ranges", "items".to_string()));

    if let Some((a, b)) = req.header("range").and_then(parse_range) {
        // newest first: skip the first `a` matches, keep at most one page, go on counting for `Content-Range`
        let want = (b - a).saturating_add(1).min(PAGE_SIZE);
        let mut kept: Vec<&Rev> = Vec::new();
        let (mut seen, mut work, mut total_known) = (0usize, 0usize, true);
        for r in store.revs_before(None) {
            work += work_of(&r.setting, tags_wanted);
            if wanted(r) {
                if seen >= a && kept.len() < want {
                    kept.push(r);
                }
                seen += 1;
            }
            if work >= RANGE_BUDGET {
                total_known = false;
                break;
            }
        }
        if !total_known && seen <= a {
            return errors::invalid_argument(
                "Range",
                "Range: the start is too far into this store's history to count; follow the Link header of the pages instead.",
                None,
            );
        }
        if a >= seen {
            let mut r = errors::empty(416);
            if let Ok(v) = azemu_core::http::HeaderValue::from_str(&format!("items */{seen}")) {
                r.headers_mut().insert("Content-Range", v);
            }
            return r;
        }
        let settings: Vec<&Setting> = kept.iter().map(|r| &*r.setting).collect();
        let items = settings.iter().map(|s| setting_json(s, select.as_deref(), newer(req))).collect();
        let mut page = Page::new(KVSET_CT, items);
        page.etag = Some(page_etag(settings.iter().map(|s| s.etag.as_str())));
        page.status = 206;
        let total = if total_known { seen.to_string() } else { "*".to_string() };
        extra.push(("Content-Range", format!("items {a}-{}/{total}", a + kept.len() - 1)));
        page.extra_headers = extra;
        return respond(req, page, newer(req));
    }

    let mut page_revs: Vec<&Rev> = Vec::new();
    let mut last_seq = None;
    let mut work = 0;
    let mut more = false;
    for r in store.revs_before(cursor) {
        work += work_of(&r.setting, tags_wanted);
        last_seq = Some(r.seq);
        if wanted(r) {
            page_revs.push(r);
            if page_revs.len() >= PAGE_SIZE {
                more = true;
                break;
            }
        }
        if work >= SCAN_BUDGET {
            more = true;
            break;
        }
    }
    let settings: Vec<&Setting> = page_revs.iter().map(|r| &*r.setting).collect();
    let items = settings.iter().map(|s| setting_json(s, select.as_deref(), newer(req))).collect();
    let mut page = Page::new(KVSET_CT, items);
    page.etag = Some(page_etag(settings.iter().map(|s| s.etag.as_str())));
    if more {
        if let Some(seq) = last_seq {
            page.next = Some(link_target(req, "/revisions", &encode_cursor(&seq.to_string())));
        }
    }
    page.extra_headers = extra;
    respond(req, page, newer(req))
}

pub fn keys(store: &Store, req: &Request) -> Response {
    let name = match filter_param(req, "name", false, "key") {
        Ok(f) => f,
        Err(r) => return r,
    };
    let select = match select_param(req, &["name"]) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let at = match accept_datetime(req) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let _ = select; // `name` is the only field
    let cursor = after_param(req);
    // settings are in key order, so the same key is on neighbouring entries; every entry of the cursor's key is skipped
    let after: Option<Ident> = cursor.as_ref().map(|c| (c.clone(), None));
    let mut page_names: Vec<&str> = Vec::new();
    let (mut work, mut more) = (0, false);
    let mut last: Option<&str> = None;
    for s in candidates(store, at, after.as_ref()) {
        work += 1;
        let k = s.key.as_str();
        if cursor.as_deref().is_some_and(|c| k <= c) || last == Some(k) {
            continue;
        }
        last = Some(k);
        if name.matches_str(k) {
            page_names.push(k);
            if page_names.len() >= PAGE_SIZE {
                more = true;
                break;
            }
        }
        if work >= SCAN_BUDGET {
            more = true;
            break;
        }
    }
    let items = page_names.iter().map(|k| json!({ "name": k })).collect();
    let mut page = Page::new(KEYSET_CT, items);
    if more {
        if let Some(last) = last {
            page.next = Some(link_target(req, "/keys", &encode_cursor(last)));
        }
    }
    page.extra_headers = memento_header(at);
    respond(req, page, newer(req))
}

pub fn labels(store: &Store, req: &Request) -> Response {
    let name = match filter_param(req, "name", true, "label") {
        Ok(f) => f,
        Err(r) => return r,
    };
    let select = match select_param(req, &["name"]) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let at = match accept_datetime(req) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let _ = select;
    let cursor: Option<Option<String>> = after_param(req).map(|c| Some(c).filter(|c| !c.is_empty()));
    // the labels of the existing settings are kept in their own ordered index; an `Accept-Datetime` read looks at the history
    let names: Box<dyn Iterator<Item = Option<&str>> + '_> = match at {
        None => Box::new(store.labels_after(cursor.as_ref()).map(|l| l.as_deref())),
        Some(t) => {
            let set: BTreeSet<Option<&str>> = store.at(t).into_iter().map(|s| s.label.as_deref()).collect();
            let c = cursor.clone();
            Box::new(set.into_iter().filter(move |l| match &c {
                Some(c) => *l > c.as_deref(),
                None => true,
            }))
        }
    };
    let mut page_labels: Vec<Option<&str>> = Vec::new();
    let (mut work, mut more) = (0, false);
    let mut last: Option<Option<&str>> = None;
    for l in names {
        work += 1;
        last = Some(l);
        if name.matches(l) {
            page_labels.push(l);
            if page_labels.len() >= PAGE_SIZE {
                more = true;
                break;
            }
        }
        if work >= SCAN_BUDGET {
            more = true;
            break;
        }
    }
    let items = page_labels.iter().map(|l| json!({ "name": l })).collect();
    let mut page = Page::new(LABELSET_CT, items);
    if more {
        if let Some(last) = last {
            page.next = Some(link_target(req, "/labels", &encode_cursor(last.unwrap_or(""))));
        }
    }
    page.extra_headers = memento_header(at);
    respond(req, page, newer(req))
}
