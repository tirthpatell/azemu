//! Atom entries, feeds and the entity descriptions inside them, in the element order and with
//! the namespaces of the real recordings.

use azemu_core::resp::Resp;
use azemu_core::Response;

use super::util::*;
use super::Cx;
use crate::core::Counts;
use crate::duration;
use crate::model::{AuthRule, Settings};
use crate::rules::RuleFilter;
use crate::state::{Queue, Subscription, Topic};

pub const SB_NS: &str = "http://schemas.microsoft.com/netservices/2010/10/servicebus/connect";
pub const XSI_NS: &str = "http://www.w3.org/2001/XMLSchema-instance";
const COUNT_NS: &str = "http://schemas.microsoft.com/netservices/2011/06/servicebus";
pub const CT_ENTRY: &str = "application/atom+xml; type=entry; charset=utf-8";
pub const CT_FEED: &str = "application/atom+xml; type=feed; charset=utf-8";

pub fn el(out: &mut String, name: &str, val: &str) {
    out.push('<');
    out.push_str(name);
    out.push('>');
    out.push_str(&esc(val));
    out.push_str("</");
    out.push_str(name);
    out.push('>');
}

fn el_bool(out: &mut String, name: &str, v: bool) {
    el(out, name, if v { "true" } else { "false" });
}

fn el_opt(out: &mut String, name: &str, v: Option<&str>) {
    if let Some(v) = v {
        el(out, name, v);
    }
}

fn auth_rules(out: &mut String, rules: &[AuthRule]) {
    out.push_str("<AuthorizationRules>");
    for r in rules {
        out.push_str("<AuthorizationRule i:type=\"SharedAccessAuthorizationRule\">");
        el(out, "ClaimType", &r.claim_type);
        el(out, "ClaimValue", &r.claim_value);
        out.push_str("<Rights>");
        for a in &r.rights {
            el(out, "AccessRights", a);
        }
        out.push_str("</Rights>");
        el(out, "KeyName", &r.key_name);
        el(out, "PrimaryKey", &r.primary_key);
        el(out, "SecondaryKey", &r.secondary_key);
        out.push_str("</AuthorizationRule>");
    }
    out.push_str("</AuthorizationRules>");
}

fn count_details(out: &mut String, prefix: &str, c: &Counts) {
    out.push_str(&format!("<CountDetails xmlns:{prefix}=\"{COUNT_NS}\">"));
    for (n, v) in [
        ("ActiveMessageCount", c.active),
        ("DeadLetterMessageCount", c.dead_letter),
        ("ScheduledMessageCount", c.scheduled),
        ("TransferMessageCount", c.transfer),
        ("TransferDeadLetterMessageCount", c.transfer_dead_letter),
    ] {
        el(out, &format!("{prefix}:{n}"), &v.to_string());
    }
    out.push_str("</CountDetails>");
}

pub fn total(c: &Counts) -> usize {
    c.active + c.dead_letter + c.scheduled + c.transfer + c.transfer_dead_letter
}

fn dur_text(ms: Option<i64>) -> String {
    duration::format(ms)
}

fn accessed(ms: i64) -> String {
    if ms <= 0 {
        ZERO_TS.to_string()
    } else {
        ts_trimmed(ms)
    }
}

fn open(name: &str) -> String {
    format!("<{name} xmlns=\"{SB_NS}\" xmlns:i=\"{XSI_NS}\">")
}

/// `full` is what GET and the lists return (runtime data included); the PUT answers leave the
/// runtime part out.
pub fn queue_desc(cx: &Cx, q: &Queue, full: bool) -> String {
    let s: &Settings = &q.core.settings;
    let c = q.core.counts();
    let mut o = open("QueueDescription");
    el(&mut o, "LockDuration", &dur_text(Some(s.lock_duration_ms)));
    el(&mut o, "MaxSizeInMegabytes", &s.max_size_mb.to_string());
    el_bool(&mut o, "RequiresDuplicateDetection", s.requires_duplicate_detection);
    el_bool(&mut o, "RequiresSession", s.requires_session);
    el(&mut o, "DefaultMessageTimeToLive", &dur_text(s.default_ttl_ms));
    el_bool(&mut o, "DeadLetteringOnMessageExpiration", s.dead_letter_on_expiry);
    el(&mut o, "DuplicateDetectionHistoryTimeWindow", &dur_text(Some(s.duplicate_window_ms)));
    el(&mut o, "MaxDeliveryCount", &s.max_delivery_count.to_string());
    el_bool(&mut o, "EnableBatchedOperations", s.enable_batched_operations);
    el(&mut o, "SizeInBytes", &q.core.size_bytes().to_string());
    el(&mut o, "MessageCount", &total(&c).to_string());
    el_bool(&mut o, "IsAnonymousAccessible", s.is_anonymous_accessible);
    auth_rules(&mut o, &s.auth_rules);
    el(&mut o, "Status", &s.status);
    el(&mut o, "CreatedAt", &ts_trimmed(s.created_ms));
    el(&mut o, "UpdatedAt", &ts_trimmed(s.updated_ms));
    if full {
        el(&mut o, "AccessedAt", &accessed(s.accessed_ms));
    }
    el_bool(&mut o, "SupportOrdering", s.support_ordering);
    if full {
        count_details(&mut o, "d2p1", &c);
    }
    el(&mut o, "AutoDeleteOnIdle", &dur_text(s.auto_delete_on_idle_ms));
    el_bool(&mut o, "EnablePartitioning", s.enable_partitioning);
    el(&mut o, "EntityAvailabilityStatus", "Available");
    el_bool(&mut o, "EnableExpress", s.enable_express);
    el_opt(&mut o, "ForwardTo", s.forward_to.as_deref().map(|p| cx.forward_url(p)).as_deref());
    el_opt(&mut o, "UserMetadata", s.user_metadata.as_deref());
    el_opt(&mut o, "ForwardDeadLetteredMessagesTo", s.forward_dead_lettered_to.as_deref().map(|p| cx.forward_url(p)).as_deref());
    if cx.api.as_str() >= "2021-05" {
        el(&mut o, "MaxMessageSizeInKilobytes", &s.max_message_size_kb.to_string());
    }
    o.push_str("</QueueDescription>");
    o
}

/// Topic counts: messages sent to a topic go to its subscriptions at once; only scheduled
/// messages wait in the topic itself.
pub fn topic_counts(t: &Topic) -> Counts {
    Counts { active: 0, dead_letter: 0, scheduled: t.scheduled.count_scheduled(), transfer: 0, transfer_dead_letter: 0 }
}

/// (SQL filters, correlation filters) over all subscriptions. True and false filters are SQL filters.
pub fn filter_counts(t: &Topic) -> (usize, usize) {
    let mut sql = 0;
    let mut cor = 0;
    for sub in t.subs.values() {
        for r in &sub.rules {
            match r.filter {
                RuleFilter::Correlation(_) => cor += 1,
                _ => sql += 1,
            }
        }
    }
    (sql, cor)
}

pub fn topic_desc(cx: &Cx, t: &Topic, full: bool) -> String {
    let s = &t.settings;
    let mut o = open("TopicDescription");
    el(&mut o, "DefaultMessageTimeToLive", &dur_text(s.default_ttl_ms));
    el(&mut o, "MaxSizeInMegabytes", &s.max_size_mb.to_string());
    el_bool(&mut o, "RequiresDuplicateDetection", s.requires_duplicate_detection);
    el(&mut o, "DuplicateDetectionHistoryTimeWindow", &dur_text(Some(s.duplicate_window_ms)));
    el_bool(&mut o, "EnableBatchedOperations", s.enable_batched_operations);
    el(&mut o, "SizeInBytes", &t.size_bytes().to_string());
    el_bool(&mut o, "FilteringMessagesBeforePublishing", s.filtering_messages_before_publishing);
    el_bool(&mut o, "IsAnonymousAccessible", s.is_anonymous_accessible);
    auth_rules(&mut o, &s.auth_rules);
    el(&mut o, "Status", &s.status);
    el(&mut o, "CreatedAt", &ts_trimmed(s.created_ms));
    el(&mut o, "UpdatedAt", &ts_trimmed(s.updated_ms));
    if full {
        el(&mut o, "AccessedAt", &accessed(s.accessed_ms));
    }
    el_bool(&mut o, "SupportOrdering", s.support_ordering);
    if full {
        count_details(&mut o, "d2p1", &topic_counts(t));
        el(&mut o, "SubscriptionCount", &t.subs.len().to_string());
        // Newer API versions only (the 2021-05 recordings do not have them).
        if cx.api.as_str() >= "2024-05" {
            let (sql, cor) = filter_counts(t);
            el(&mut o, "SqlFilterCount", &sql.to_string());
            el(&mut o, "CorrelationFilterCount", &cor.to_string());
        }
    }
    el(&mut o, "AutoDeleteOnIdle", &dur_text(s.auto_delete_on_idle_ms));
    el_bool(&mut o, "EnablePartitioning", s.enable_partitioning);
    el(&mut o, "EntityAvailabilityStatus", "Available");
    el_bool(&mut o, "EnableSubscriptionPartitioning", s.enable_subscription_partitioning);
    el_bool(&mut o, "EnableExpress", s.enable_express);
    el_opt(&mut o, "UserMetadata", s.user_metadata.as_deref());
    if cx.api.as_str() >= "2021-05" {
        el(&mut o, "MaxMessageSizeInKilobytes", &s.max_message_size_kb.to_string());
    }
    o.push_str("</TopicDescription>");
    o
}

pub fn sub_desc(cx: &Cx, sub: &Subscription, full: bool) -> String {
    let s = &sub.core.settings;
    let c = sub.core.counts();
    let mut o = open("SubscriptionDescription");
    el(&mut o, "LockDuration", &dur_text(Some(s.lock_duration_ms)));
    el_bool(&mut o, "RequiresSession", s.requires_session);
    el(&mut o, "DefaultMessageTimeToLive", &dur_text(s.default_ttl_ms));
    el_bool(&mut o, "DeadLetteringOnMessageExpiration", s.dead_letter_on_expiry);
    el_bool(&mut o, "DeadLetteringOnFilterEvaluationExceptions", s.dead_letter_on_filter_exception);
    el(&mut o, "MessageCount", &total(&c).to_string());
    el(&mut o, "MaxDeliveryCount", &s.max_delivery_count.to_string());
    el_bool(&mut o, "EnableBatchedOperations", s.enable_batched_operations);
    el(&mut o, "Status", &s.status);
    el_opt(&mut o, "ForwardTo", s.forward_to.as_deref().map(|p| cx.forward_url(p)).as_deref());
    el_opt(&mut o, "UserMetadata", s.user_metadata.as_deref());
    el(&mut o, "CreatedAt", &ts_7(s.created_ms));
    el(&mut o, "UpdatedAt", &ts_7(s.updated_ms));
    if full {
        let at = if s.accessed_ms > 0 { s.accessed_ms } else { s.created_ms };
        el(&mut o, "AccessedAt", &ts_7(at));
        count_details(&mut o, "d3p1", &c);
    } else {
        el(&mut o, "AccessedAt", "0001-01-01T00:00:00");
    }
    el_opt(&mut o, "ForwardDeadLetteredMessagesTo", s.forward_dead_lettered_to.as_deref().map(|p| cx.forward_url(p)).as_deref());
    el(&mut o, "AutoDeleteOnIdle", &dur_text(s.auto_delete_on_idle_ms));
    el(&mut o, "EntityAvailabilityStatus", "Available");
    el_bool(&mut o, "IsClientAffine", false);
    o.push_str("</SubscriptionDescription>");
    o
}

/// The pieces of one `<entry>`.
pub struct Entry<'a> {
    pub title: &'a str,
    pub id: String,
    pub self_href: String,
    pub published: Option<i64>,
    pub updated: i64,
    pub author: bool,
    /// Inside a feed: the feed URL for `xml:base`.
    pub feed_base: Option<String>,
}

pub fn entry(cx: &Cx, e: &Entry, content: &str) -> String {
    let mut o = match &e.feed_base {
        Some(b) => format!("<entry xml:base=\"{}\">", esc(b)),
        None => "<entry xmlns=\"http://www.w3.org/2005/Atom\">".to_string(),
    };
    o.push_str(&format!("<id>{}</id>", esc(&e.id)));
    o.push_str(&format!("<title type=\"text\">{}</title>", esc(e.title)));
    if let Some(p) = e.published {
        o.push_str(&format!("<published>{}</published>", ts_secs(p)));
    }
    o.push_str(&format!("<updated>{}</updated>", ts_secs(e.updated)));
    if e.author {
        o.push_str(&format!("<author><name>{}</name></author>", esc(&cx.account)));
    }
    o.push_str(&format!("<link rel=\"self\" href=\"{}\"/>", esc(&e.self_href)));
    o.push_str(&format!("<content type=\"application/xml\">{content}</content></entry>"));
    o
}

/// A whole feed. `next` is the href of the next page, when there is one.
pub fn feed(cx: &Cx, title: &str, next: Option<String>, entries: &str) -> String {
    let url = esc(&cx.url());
    let mut o = format!(
        "<feed xmlns=\"http://www.w3.org/2005/Atom\"><title type=\"text\">{title}</title><id>{url}</id><updated>{}</updated><link rel=\"self\" href=\"{url}\"/>",
        ts_secs(cx.now)
    );
    if let Some(n) = next {
        o.push_str(&format!("<link rel=\"next\" href=\"{}\"/>", esc(&n)));
    }
    o.push_str(entries);
    o.push_str("</feed>");
    o
}

pub fn respond(status: u16, content_type: &str, etag: Option<&str>, body: String) -> Response {
    let mut r = Resp::new(status).header("Content-Type", content_type).header("Server", "Microsoft-HTTPAPI/2.0");
    if let Some(e) = etag {
        r = r.header("ETag", e);
    }
    r.body(body).build()
}

/// An empty 200 (delete), with the entity's ETag.
pub fn ok_empty(etag: &str) -> Response {
    Resp::new(200).header("Server", "Microsoft-HTTPAPI/2.0").header("ETag", etag).build()
}
