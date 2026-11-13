//! Queues and topics: `PUT/GET/DELETE /{name}` and the `/$Resources/queues|topics` feeds.

use azemu_core::Response;

use super::error::{ApiError, ApiResult, ErrKind};
use super::render::{self, Entry, CT_ENTRY, CT_FEED};
use super::settings::{self, Kind, Opts};
use super::util::{enc_segment, etag_of};
use super::xml;
use super::{misc, Cx};
use crate::state::{key, Namespace, Queue, Shared, Topic};

const MAX_NAME: usize = 260;

pub fn queue_entry(cx: &Cx, q: &Queue, full: bool, single: bool, published: bool) -> String {
    let s = &q.core.settings;
    let e = Entry {
        title: &q.name,
        id: if single { cx.url() } else { cx.entity_url(&q.name) },
        self_href: if single { cx.url() } else { format!("../{}", cx.entity_rel(&q.name)) },
        published: published.then_some(s.created_ms),
        updated: s.updated_ms,
        author: true,
        feed_base: (!single).then(|| cx.url()),
    };
    render::entry(cx, &e, &render::queue_desc(cx, q, full))
}

pub fn topic_entry(cx: &Cx, t: &Topic, full: bool, single: bool, published: bool) -> String {
    let s = &t.settings;
    let e = Entry {
        title: &t.name,
        id: if single { cx.url() } else { cx.entity_url(&t.name) },
        self_href: if single { cx.url() } else { format!("../{}", cx.entity_rel(&t.name)) },
        published: published.then_some(s.created_ms),
        updated: s.updated_ms,
        author: true,
        feed_base: (!single).then(|| cx.url()),
    };
    render::entry(cx, &e, &render::topic_desc(cx, t, full))
}

pub fn get(shared: &Shared, cx: &Cx, name: &str) -> ApiResult<Response> {
    let mut st = shared.state.lock();
    let ns = st.ns(&cx.account);
    if let Some(q) = ns.queue(name) {
        return Ok(render::respond(200, CT_ENTRY, Some(&etag_of(q.core.settings.updated_ms)), queue_entry(cx, q, true, true, true)));
    }
    if let Some(t) = ns.topic(name) {
        return Ok(render::respond(200, CT_ENTRY, Some(&etag_of(t.settings.updated_ms)), topic_entry(cx, t, true, true, true)));
    }
    // The real service answers a GET for an unknown entity with the namespace's service
    // document; the SDK reads "no content" as "does not exist".
    Ok(misc::root_feed(cx))
}

fn existing_etag(ns: &Namespace, name: &str) -> Option<String> {
    let k = key(name);
    ns.queues.get(&k).map(|q| etag_of(q.core.settings.updated_ms)).or_else(|| ns.topics.get(&k).map(|t| etag_of(t.settings.updated_ms)))
}

pub fn put(shared: &Shared, cx: &Cx, name: &str) -> ApiResult<Response> {
    let root = xml::parse(&cx.req.body).ok_or_else(ApiError::bad_request)?;
    let desc = settings::description(&root).ok_or_else(ApiError::bad_request)?;
    let kind = settings::kind_of(desc).filter(|k| *k != Kind::Subscription).ok_or_else(ApiError::bad_request)?;
    let update = cx.req.header("if-match").is_some();

    let mut st = shared.state.lock();
    let ns = st.ns(&cx.account);
    let old = match kind {
        Kind::Queue => ns.queue(name).map(|q| q.core.settings.clone()),
        _ => ns.topic(name).map(|t| t.settings.clone()),
    };
    let parsed = settings::parse(desc, &Opts { kind, api: &cx.api, existing: if update { old.as_ref() } else { None }, now: cx.now });
    let mut new = match (parsed, &old) {
        (Ok(s), _) => s,
        // The real service sends the ETag of the existing entity with a refused update.
        (Err(e), Some(o)) if update => return Err(e.with_etag(etag_of(o.updated_ms))),
        (Err(e), _) => return Err(e),
    };

    if update {
        let old = old.ok_or_else(|| ApiError::new(ErrKind::NotFoundOperation))?;
        if let Some(p) = settings::immutable_change(kind, &old, &new) {
            let msg = format!("The value for the {p} property of an existing {} cannot be changed.", kind.label());
            return Err(ApiError::bad(msg, true).with_etag(etag_of(old.updated_ms)));
        }
        new.updated_ms = cx.now;
        return Ok(match kind {
            Kind::Queue => {
                let q = ns.queue_mut(name).ok_or_else(|| ApiError::new(ErrKind::NotFoundOperation))?;
                q.core.settings = new;
                render::respond(200, CT_ENTRY, Some(&etag_of(cx.now)), queue_entry(cx, q, true, true, false))
            }
            _ => {
                let t = ns.topic_mut(name).ok_or_else(|| ApiError::new(ErrKind::NotFoundOperation))?;
                t.settings = new;
                render::respond(200, CT_ENTRY, Some(&etag_of(cx.now)), topic_entry(cx, t, true, true, false))
            }
        });
    }

    if name.len() > MAX_NAME {
        return Err(ApiError::bad_request());
    }
    if let Some(etag) = existing_etag(ns, name) {
        return Err(ApiError::new(ErrKind::Conflict).with_etag(etag));
    }
    match kind {
        Kind::Queue => {
            ns.create_queue(name, new).map_err(create_error)?;
            let q = ns.queue(name).ok_or_else(ApiError::bad_request)?;
            Ok(render::respond(201, CT_ENTRY, None, queue_entry(cx, q, false, true, true)))
        }
        _ => {
            ns.create_topic(name, new).map_err(create_error)?;
            let t = ns.topic(name).ok_or_else(ApiError::bad_request)?;
            Ok(render::respond(201, CT_ENTRY, None, topic_entry(cx, t, false, true, true)))
        }
    }
}

pub fn delete(shared: &Shared, cx: &Cx, name: &str) -> ApiResult<Response> {
    let mut st = shared.state.lock();
    let ns = st.ns(&cx.account);
    let k = key(name);
    if let Some(mut q) = ns.queues.remove(&k) {
        q.core.wake_all();
        return Ok(render::ok_empty(&etag_of(q.core.settings.updated_ms)));
    }
    if let Some(mut t) = ns.topics.remove(&k) {
        for sub in t.subs.values_mut() {
            sub.core.wake_all();
        }
        return Ok(render::ok_empty(&etag_of(t.settings.updated_ms)));
    }
    Err(ApiError::new(ErrKind::NoService))
}

/// `$skip` and `$top` of a list request.
pub fn paging(cx: &Cx) -> ApiResult<(usize, usize)> {
    let num = |name: &str, default: usize| -> ApiResult<usize> {
        match cx.req.query.get(name) {
            None => Ok(default),
            Some(v) => v.trim().parse::<usize>().map_err(|_| ApiError::bad_request()),
        }
    };
    Ok((num("$skip", 0)?, num("$top", 100)?))
}

/// The `next` link of a full page.
pub fn next_link(cx: &Cx, skip: usize, top: usize, total: usize) -> Option<String> {
    (skip.saturating_add(top) < total)
        .then(|| format!("{}{}?$skip={}&$top={}&api-version={}", cx.base, cx.req.path, skip + top, top, enc_segment(&cx.api)))
}

pub fn list(shared: &Shared, cx: &Cx, kind: Kind) -> ApiResult<Response> {
    let (skip, top) = paging(cx)?;
    let mut st = shared.state.lock();
    let ns = st.ns(&cx.account);
    let (title, total, entries) = match kind {
        Kind::Queue => {
            let all: Vec<&Queue> = ns.queues.values().collect();
            let page: String = all.iter().skip(skip).take(top).map(|q| queue_entry(cx, q, true, false, true)).collect();
            ("Queues", all.len(), page)
        }
        _ => {
            let all: Vec<&Topic> = ns.topics.values().collect();
            let page: String = all.iter().skip(skip).take(top).map(|t| topic_entry(cx, t, true, false, true)).collect();
            ("Topics", all.len(), page)
        }
    };
    Ok(render::respond(200, CT_FEED, None, render::feed(cx, title, next_link(cx, skip, top, total), &entries)))
}

/// What a refused `create_queue` / `create_topic` means over HTTP: a name that is taken is a 409, a
/// used-up quota is a 403.
pub(super) fn create_error(e: crate::errors::SbError) -> ApiError {
    if e.status == 403 {
        ApiError::new(ErrKind::Quota(e.description))
    } else {
        ApiError::new(ErrKind::Conflict)
    }
}
