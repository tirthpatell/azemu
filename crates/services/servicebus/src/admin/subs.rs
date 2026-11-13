//! Subscriptions: `/{topic}/subscriptions[/{sub}]`.

use azemu_core::Response;

use super::entities::{next_link, paging};
use super::error::{ApiError, ApiResult, ErrKind};
use super::render::{self, Entry, CT_ENTRY, CT_FEED};
use super::settings::{self, Kind, Opts};
use super::util::{enc_segment, etag_of};
use super::{rule_xml, xml, Cx};
use crate::rules::Rule;
use crate::state::{key, Shared, Subscription, Topic};

/// The id the real service uses in its "not found" and "already exists" texts.
pub fn entity_id(cx: &Cx, topic: &str, sub: &str) -> String {
    if sub.is_empty() {
        format!("{}:Topic:{topic}", cx.account)
    } else {
        format!("{}:Topic:{topic}|{sub}", cx.account)
    }
}

fn sub_entry(cx: &Cx, topic: &Topic, sub: &Subscription, full: bool, single: bool, published: bool) -> String {
    let s = &sub.core.settings;
    let name = enc_segment(&sub.name);
    let word = cx.path_word(1, "subscriptions");
    let e = Entry {
        title: &sub.name,
        id: if single {
            cx.url()
        } else {
            format!("{}/{}/{word}/{name}?api-version={}", cx.base, enc_segment(&topic.name), enc_segment(&cx.api))
        },
        self_href: if single { cx.url() } else { format!("{word}/{name}?api-version={}", enc_segment(&cx.api)) },
        published: published.then_some(s.created_ms),
        updated: s.updated_ms,
        author: false,
        feed_base: (!single).then(|| cx.url()),
    };
    render::entry(cx, &e, &render::sub_desc(cx, sub, full))
}

pub fn get(shared: &Shared, cx: &Cx, topic: &str, name: &str) -> ApiResult<Response> {
    let mut st = shared.state.lock();
    let ns = st.ns(&cx.account);
    let t = ns.topic(topic).ok_or_else(|| ApiError::not_found_entity(entity_id(cx, topic, name)))?;
    let sub = t.subs.get(&key(name)).ok_or_else(|| ApiError::not_found_entity(entity_id(cx, topic, name)))?;
    Ok(render::respond(200, CT_ENTRY, Some(&etag_of(sub.core.settings.updated_ms)), sub_entry(cx, t, sub, true, true, true)))
}

pub fn put(shared: &Shared, cx: &Cx, topic: &str, name: &str) -> ApiResult<Response> {
    let root = xml::parse(&cx.req.body).ok_or_else(ApiError::bad_request)?;
    let desc = settings::description(&root).ok_or_else(ApiError::bad_request)?;
    if settings::kind_of(desc) != Some(Kind::Subscription) {
        return Err(ApiError::bad_request());
    }
    let update = cx.req.header("if-match").is_some();
    let missing = || ApiError::not_found_entity(entity_id(cx, topic, name));

    let mut st = shared.state.lock();
    let ns = st.ns(&cx.account);
    let t = ns.topic_mut(topic).ok_or_else(|| ApiError::not_found_entity(entity_id(cx, topic, "")))?;
    let k = key(name);
    let old = t.subs.get(&k).map(|s| s.core.settings.clone());
    let parsed = settings::parse(
        desc,
        &Opts { kind: Kind::Subscription, api: &cx.api, existing: if update { old.as_ref() } else { None }, now: cx.now },
    );
    let mut new = match (parsed, &old) {
        (Ok(s), _) => s,
        (Err(e), Some(o)) if update => return Err(e.with_etag(etag_of(o.updated_ms))),
        (Err(e), _) => return Err(e),
    };

    if update {
        let old = old.ok_or_else(missing)?;
        if let Some(p) = settings::immutable_change(Kind::Subscription, &old, &new) {
            let msg = format!("The value for the {p} property of an existing Subscription cannot be changed.");
            return Err(ApiError::bad(msg, false).with_etag(etag_of(old.updated_ms)));
        }
        new.updated_ms = cx.now;
        if let Some(sub) = t.subs.get_mut(&k) {
            sub.core.settings = new;
        }
        let t = &*t;
        let sub = t.subs.get(&k).ok_or_else(missing)?;
        return Ok(render::respond(200, CT_ENTRY, Some(&etag_of(cx.now)), sub_entry(cx, t, sub, false, true, true)));
    }

    if let Some(old) = old {
        return Err(ApiError::new(ErrKind::AlreadyExists(entity_id(cx, topic, name))).with_etag(etag_of(old.updated_ms)));
    }
    let etag = etag_of(new.created_ms);
    // `DefaultRuleDescription` (the JavaScript SDK's `defaultRuleOptions`) replaces `$Default`
    let first_rule = match desc.child("DefaultRuleDescription") {
        Some(rd) => {
            let rule_name =
                rd.child("Name").map(|n| n.val().trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "$Default".to_string());
            rule_xml::parse(&rule_name, rd, cx.now)?
        }
        None => Rule::default_rule(cx.now),
    };
    ns.create_subscription(topic, name, new, vec![first_rule]).map_err(|e| {
        if e.status == 403 {
            ApiError::new(ErrKind::Quota(e.description))
        } else {
            ApiError::new(ErrKind::AlreadyExists(entity_id(cx, topic, name)))
        }
    })?;
    let t = ns.topic(topic).ok_or_else(missing)?;
    let sub = t.subs.get(&k).ok_or_else(missing)?;
    Ok(render::respond(201, CT_ENTRY, Some(&etag), sub_entry(cx, t, sub, false, true, true)))
}

pub fn delete(shared: &Shared, cx: &Cx, topic: &str, name: &str) -> ApiResult<Response> {
    let mut st = shared.state.lock();
    let ns = st.ns(&cx.account);
    let t = ns.topic_mut(topic).ok_or_else(|| ApiError::new(ErrKind::NoService))?;
    let mut sub = t.subs.remove(&key(name)).ok_or_else(|| ApiError::new(ErrKind::NoService))?;
    sub.core.wake_all();
    Ok(render::ok_empty(&etag_of(sub.core.settings.updated_ms)))
}

pub fn list(shared: &Shared, cx: &Cx, topic: &str) -> ApiResult<Response> {
    let (skip, top) = paging(cx)?;
    let mut st = shared.state.lock();
    let ns = st.ns(&cx.account);
    let t = ns.topic(topic).ok_or_else(|| ApiError::not_found_entity(entity_id(cx, topic, "")))?;
    let entries: String = t.subs.values().skip(skip).take(top).map(|s| sub_entry(cx, t, s, true, false, true)).collect();
    let body = render::feed(cx, "Subscriptions", next_link(cx, skip, top, t.subs.len()), &entries);
    Ok(render::respond(200, CT_FEED, Some(&etag_of(cx.now)), body))
}
