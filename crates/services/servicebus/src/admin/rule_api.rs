//! Rules: `/{topic}/subscriptions/{sub}/rules[/{rule}]`.

use azemu_core::Response;

use super::entities::{check_name, next_link, paging};
use super::error::{ApiError, ApiResult, ErrKind};
use super::render::{self, Entry, CT_ENTRY, CT_FEED};
use super::subs::entity_id;
use super::util::{enc_segment, etag_of};
use super::{rule_xml, xml, Cx};
use crate::rules::Rule;
use crate::state::{key, Shared, Subscription};

fn rule_entry(cx: &Cx, topic: &str, sub: &str, rule: &Rule, single: bool, full: bool) -> String {
    let name = enc_segment(&rule.name);
    let (subs_word, rules_word) = (cx.path_word(1, "subscriptions"), cx.path_word(3, "rules"));
    let base = format!("{}/{}/{subs_word}/{}/{rules_word}", cx.base, enc_segment(topic), enc_segment(sub));
    let e = Entry {
        title: &rule.name,
        id: if single { cx.url() } else { format!("{base}/{name}?api-version={}", enc_segment(&cx.api)) },
        self_href: if single { cx.url() } else { format!("{rules_word}/{name}?api-version={}", enc_segment(&cx.api)) },
        published: Some(rule.created_ms),
        updated: rule.created_ms,
        author: false,
        feed_base: (!single).then(|| cx.url()),
    };
    render::entry(cx, &e, &rule_xml::desc_xml(rule, full))
}

fn rule_id(cx: &Cx, topic: &str, sub: &str, rule: &str) -> String {
    format!("{}:Topic:{topic}|{sub}|{rule}", cx.account)
}

/// The subscription a rule request is about. `Err(None)`: the topic is not there (the service answers a read with its list of services, and
/// a write with "not recognized"); `Err(Some(e))`: the subscription is not there.
fn find_sub<'a>(
    shared_ns: &'a mut crate::state::Namespace,
    cx: &Cx,
    topic: &str,
    sub: &str,
) -> Result<&'a mut Subscription, Option<ApiError>> {
    let t = shared_ns.topic_mut(topic).ok_or(None)?;
    t.subs.get_mut(&key(sub)).ok_or_else(|| Some(ApiError::not_found_entity(entity_id(cx, topic, sub))))
}

fn pos(sub: &Subscription, name: &str) -> Option<usize> {
    sub.rules.iter().position(|r| r.name.eq_ignore_ascii_case(name))
}

pub fn get(shared: &Shared, cx: &Cx, topic: &str, sub: &str, name: &str) -> ApiResult<Response> {
    let mut st = shared.state.lock();
    let s = match find_sub(st.ns(&cx.account), cx, topic, sub) {
        Ok(s) => s,
        Err(None) => return Ok(super::misc::root_feed(cx)),
        Err(Some(e)) => return Err(e),
    };
    let i = pos(s, name).ok_or_else(|| ApiError::not_found_entity(rule_id(cx, topic, sub, name)))?;
    let rule = &s.rules[i];
    Ok(render::respond(200, CT_ENTRY, Some(&etag_of(rule.created_ms)), rule_entry(cx, topic, sub, rule, true, true)))
}

pub fn put(shared: &Shared, cx: &Cx, topic: &str, sub: &str, name: &str) -> ApiResult<Response> {
    let not_recognized = || ApiError::new(ErrKind::NotRecognized);
    let root = xml::parse(&cx.req.body).ok_or_else(not_recognized)?;
    let content = root.child("content");
    let desc = content.and_then(|c| c.child("RuleDescription")).or_else(|| (root.name == "RuleDescription").then_some(&root)).ok_or_else(
        || match content.and_then(|c| c.children.iter().find(|n| n.name.ends_with("Description"))) {
            // the description of another kind of entity is a description, but not one of a rule
            Some(_) => ApiError::bad_request(),
            None => not_recognized(),
        },
    )?;
    check_name(cx, name, "Rule")?;
    let update = cx.req.header("if-match").is_some();
    let id = || rule_id(cx, topic, sub, name);

    let mut st = shared.state.lock();
    let s = match find_sub(st.ns(&cx.account), cx, topic, sub) {
        Ok(s) => s,
        // a rule of a topic that is not there: not recognized; of a subscription that is not there: not found, named with the rule
        Err(None) => return Err(not_recognized()),
        Err(Some(_)) => return Err(ApiError::new(ErrKind::NotFoundMessaging(rule_id(cx, topic, sub, name)))),
    };
    let existing = pos(s, name);
    let mut rule = rule_xml::parse(name, desc, cx.now)?;
    if update {
        let i = existing.ok_or_else(|| ApiError::not_found_entity(id()))?;
        rule.name = s.rules[i].name.clone();
        rule.created_ms = s.rules[i].created_ms;
        s.rules[i] = rule;
        let rule = &s.rules[i];
        return Ok(render::respond(200, CT_ENTRY, Some(&etag_of(cx.now)), rule_entry(cx, topic, sub, rule, true, false)));
    }
    if let Some(i) = existing {
        return Err(ApiError::new(ErrKind::AlreadyExists(id())).with_etag(etag_of(s.rules[i].created_ms)));
    }
    let etag = etag_of(rule.created_ms);
    let body = rule_entry(cx, topic, sub, &rule, true, false);
    s.rules.push(rule);
    Ok(render::respond(201, CT_ENTRY, Some(&etag), body))
}

pub fn delete(shared: &Shared, cx: &Cx, topic: &str, sub: &str, name: &str) -> ApiResult<Response> {
    let mut st = shared.state.lock();
    let ns = st.ns(&cx.account);
    let s = ns.topic_mut(topic).and_then(|t| t.subs.get_mut(&key(sub))).ok_or_else(|| ApiError::new(ErrKind::NoService))?;
    let i = pos(s, name).ok_or_else(|| ApiError::new(ErrKind::NotFoundMessaging(rule_id(cx, topic, sub, name))))?;
    let rule = s.rules.remove(i);
    Ok(render::ok_empty(&etag_of(rule.created_ms)))
}

pub fn list(shared: &Shared, cx: &Cx, topic: &str, sub: &str) -> ApiResult<Response> {
    let (skip, top) = paging(cx)?;
    let mut st = shared.state.lock();
    let s = match find_sub(st.ns(&cx.account), cx, topic, sub) {
        Ok(s) => s,
        Err(None) => return Ok(super::misc::root_feed(cx)),
        Err(Some(e)) => return Err(e),
    };
    let entries: String = s.rules.iter().skip(skip).take(top).map(|r| rule_entry(cx, topic, sub, r, false, true)).collect();
    let body = render::feed(cx, "Rules", next_link(cx, skip, top, s.rules.len()), &entries);
    Ok(render::respond(200, CT_FEED, Some(&etag_of(cx.now)), body))
}
