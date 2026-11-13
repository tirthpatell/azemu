//! Request description (`QueueDescription` / `TopicDescription` / `SubscriptionDescription`)
//! to [`Settings`], with the validations the real service applies.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use super::error::{ApiError, ApiResult};
use super::util::timespan_text;
use super::xml::Node;
use crate::duration::{self, TIMESPAN_MAX_MS};
use crate::model::{AuthRule, Settings};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Queue,
    Topic,
    Subscription,
}

impl Kind {
    pub fn desc_name(self) -> &'static str {
        match self {
            Kind::Queue => "QueueDescription",
            Kind::Topic => "TopicDescription",
            Kind::Subscription => "SubscriptionDescription",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Queue => "Queue",
            Kind::Topic => "Topic",
            Kind::Subscription => "Subscription",
        }
    }
}

pub struct Opts<'a> {
    pub kind: Kind,
    pub api: &'a str,
    /// The entity being updated; `None` on create.
    pub existing: Option<&'a Settings>,
    pub now: i64,
}

/// The `QueueDescription` etc. element inside an Atom entry (or the element itself).
pub fn description(root: &Node) -> Option<&Node> {
    if root.name.ends_with("Description") {
        return Some(root);
    }
    root.child("content")?.children.iter().find(|c| c.name.ends_with("Description"))
}

pub fn kind_of(desc: &Node) -> Option<Kind> {
    match desc.name.as_str() {
        "QueueDescription" => Some(Kind::Queue),
        "TopicDescription" => Some(Kind::Topic),
        "SubscriptionDescription" => Some(Kind::Subscription),
        _ => None,
    }
}

/// `sb://host[:port]/path` or `path` to `path`.
pub fn normalize_forward(v: &str) -> Option<String> {
    let v = v.trim();
    let path = match v.find("://") {
        Some(i) => {
            let rest = &v[i + 3..];
            rest.find('/').map(|j| &rest[j + 1..]).unwrap_or("")
        }
        None => v,
    };
    let path = path.trim_matches('/');
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

fn boolean(v: &str) -> ApiResult<bool> {
    match v {
        "true" | "True" => Ok(true),
        "false" | "False" => Ok(false),
        _ => Err(ApiError::bad_request()),
    }
}

fn int(v: &str) -> ApiResult<i64> {
    v.parse::<i64>().map_err(|_| ApiError::bad_request())
}

fn dur(v: &str) -> ApiResult<i64> {
    duration::parse(v).ok_or_else(ApiError::bad_request)
}

fn opt_dur(v: &str) -> ApiResult<Option<i64>> {
    let ms = dur(v)?;
    Ok(if ms >= TIMESPAN_MAX_MS { None } else { Some(ms) })
}

fn gen_key() -> String {
    let mut b = Vec::with_capacity(32);
    b.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    b.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    B64.encode(b)
}

fn auth_rules(n: &Node) -> ApiResult<Vec<AuthRule>> {
    let mut out = Vec::new();
    for r in n.children_named("AuthorizationRule") {
        let text = |name: &str| r.child(name).map(|c| c.val().to_string()).filter(|s| !s.is_empty());
        let key_name = text("KeyName").ok_or_else(ApiError::bad_request)?;
        let mut rights = Vec::new();
        if let Some(rn) = r.child("Rights") {
            for a in rn.children_named("AccessRights") {
                match a.val() {
                    v @ ("Manage" | "Send" | "Listen") => rights.push(v.to_string()),
                    _ => return Err(ApiError::bad_request()),
                }
            }
        }
        out.push(AuthRule {
            claim_type: text("ClaimType").unwrap_or_else(|| "SharedAccessKey".into()),
            claim_value: text("ClaimValue").unwrap_or_else(|| "None".into()),
            rights,
            key_name,
            primary_key: text("PrimaryKey").unwrap_or_else(gen_key),
            secondary_key: text("SecondaryKey").unwrap_or_else(gen_key),
        });
    }
    Ok(out)
}

/// Build the settings of an entity from a request description. On update the description
/// replaces everything it can carry (an absent element goes back to the default); creation and
/// access times are kept.
pub fn parse(node: &Node, o: &Opts) -> ApiResult<Settings> {
    let kind = o.kind;
    let mut s = match kind {
        Kind::Queue => Settings::queue_default(o.now),
        Kind::Topic => Settings::topic_default(o.now),
        Kind::Subscription => Settings::subscription_default(o.now),
    };
    if let Some(old) = o.existing {
        s.created_ms = old.created_ms;
        s.accessed_ms = old.accessed_ms;
        s.max_message_size_kb = old.max_message_size_kb;
    }
    let not_topic = kind != Kind::Topic;
    let not_sub = kind != Kind::Subscription;
    let mut size: Option<i64> = None;
    let mut ordering_set = false;
    for c in &node.children {
        let v = c.val();
        match c.name.as_str() {
            "LockDuration" if not_topic => s.lock_duration_ms = dur(v)?,
            "MaxSizeInMegabytes" if not_sub => size = Some(int(v)?),
            "RequiresDuplicateDetection" if not_sub => s.requires_duplicate_detection = boolean(v)?,
            "RequiresSession" if not_topic => s.requires_session = boolean(v)?,
            "DefaultMessageTimeToLive" => s.default_ttl_ms = opt_dur(v)?,
            "DeadLetteringOnMessageExpiration" if not_topic => s.dead_letter_on_expiry = boolean(v)?,
            "DeadLetteringOnFilterEvaluationExceptions" if kind == Kind::Subscription => s.dead_letter_on_filter_exception = boolean(v)?,
            "DuplicateDetectionHistoryTimeWindow" if not_sub => s.duplicate_window_ms = dur(v)?,
            "MaxDeliveryCount" if not_topic => {
                let n = int(v)?;
                s.max_delivery_count = u32::try_from(n).ok().filter(|n| *n >= 1).ok_or_else(ApiError::bad_request)?;
            }
            "EnableBatchedOperations" => s.enable_batched_operations = boolean(v)?,
            "IsAnonymousAccessible" if not_sub => s.is_anonymous_accessible = boolean(v)?,
            "AuthorizationRules" if not_sub => s.auth_rules = auth_rules(c)?,
            "Status" => {
                if !matches!(v, "Active" | "Disabled" | "SendDisabled" | "ReceiveDisabled") {
                    return Err(ApiError::bad_request());
                }
                s.status = v.to_string();
            }
            "SupportOrdering" if not_sub => {
                s.support_ordering = boolean(v)?;
                ordering_set = true;
            }
            "AutoDeleteOnIdle" => s.auto_delete_on_idle_ms = opt_dur(v)?,
            "EnablePartitioning" if not_sub => s.enable_partitioning = boolean(v)?,
            "EnableExpress" if not_sub => s.enable_express = boolean(v)?,
            "EnableSubscriptionPartitioning" if kind == Kind::Topic => s.enable_subscription_partitioning = boolean(v)?,
            "FilteringMessagesBeforePublishing" if kind == Kind::Topic => s.filtering_messages_before_publishing = boolean(v)?,
            "ForwardTo" if not_topic => s.forward_to = normalize_forward(v),
            "ForwardDeadLetteredMessagesTo" if not_topic => s.forward_dead_lettered_to = normalize_forward(v),
            "UserMetadata" => s.user_metadata = Some(c.text.clone()).filter(|t| !t.is_empty()),
            "MaxMessageSizeInKilobytes" if not_sub => {
                if o.api < "2021-05" {
                    return Err(ApiError::bad(
                        format!(
                            "The entity description of type '{}' has unsupported properties for api-version '{}' in the request. Try including the correct api-version query string in the request.",
                            kind.desc_name(),
                            o.api
                        ),
                        true,
                    ));
                }
                let kb = int(v)?;
                // Standard tier: 256 KB is the ceiling.
                if !(1..=256).contains(&kb) {
                    return Err(ApiError::bad_request());
                }
                s.max_message_size_kb = kb;
            }
            _ => {}
        }
    }
    if not_sub && !ordering_set {
        s.support_ordering = !s.enable_partitioning;
    }
    if not_sub {
        s.max_size_mb = match (size, o.existing) {
            (Some(sz), Some(old)) if old.enable_partitioning && s.enable_partitioning && sz == old.max_size_mb => sz,
            (Some(sz), _) if sz >= 1 => sz.checked_mul(if s.enable_partitioning { 16 } else { 1 }).ok_or_else(ApiError::bad_request)?,
            (Some(_), _) => return Err(ApiError::bad_request()),
            (None, Some(old)) if old.enable_partitioning == s.enable_partitioning => old.max_size_mb,
            (None, _) => 1024 * if s.enable_partitioning { 16 } else { 1 },
        };
    }
    if not_topic && s.lock_duration_ms > 300_000 {
        return Err(ApiError::bad_param(
            "The supplied lock time exceeds the allowed maximum of '5' minutes.",
            "LockDuration",
            &timespan_text(s.lock_duration_ms),
            kind == Kind::Queue,
            true,
        ));
    }
    if not_sub && !(20_000..=7 * 86_400_000).contains(&s.duplicate_window_ms) {
        return Err(ApiError::bad_param(
            "The value supplied must be between 00:00:20 and 7.00:00:00.",
            "DuplicateDetectionHistoryTimeWindow",
            &timespan_text(s.duplicate_window_ms),
            true,
            false,
        ));
    }
    if matches!(s.auto_delete_on_idle_ms, Some(ms) if ms < 300_000) {
        return Err(ApiError::bad_request());
    }
    Ok(s)
}

/// An update may not change these (the real service answers 400 for each).
pub fn immutable_change(kind: Kind, old: &Settings, new: &Settings) -> Option<&'static str> {
    if kind != Kind::Topic && old.requires_session != new.requires_session {
        return Some("RequiresSession");
    }
    if kind != Kind::Subscription {
        if old.enable_partitioning != new.enable_partitioning {
            return Some("EnablePartitioning");
        }
        if old.requires_duplicate_detection != new.requires_duplicate_detection {
            return Some("RequiresDuplicateDetection");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::xml;

    fn desc(body: &str) -> Node {
        let n = xml::parse(body.as_bytes()).unwrap();
        description(&n).unwrap().clone()
    }

    #[test]
    fn forward_paths() {
        assert_eq!(normalize_forward("sb://h:1/q"), Some("q".into()));
        assert_eq!(normalize_forward("q"), Some("q".into()));
        assert_eq!(normalize_forward("sb://h/t/Subscriptions/s"), Some("t/Subscriptions/s".into()));
        assert_eq!(normalize_forward("  "), None);
        assert_eq!(normalize_forward("sb://h"), None);
    }

    #[test]
    fn partitioned_size_is_sixteen_times() {
        let d = desc("<entry><content><QueueDescription><MaxSizeInMegabytes>3072</MaxSizeInMegabytes><EnablePartitioning>true</EnablePartitioning></QueueDescription></content></entry>");
        let s = parse(&d, &Opts { kind: Kind::Queue, api: "2021-05", existing: None, now: 0 }).unwrap();
        assert_eq!(s.max_size_mb, 49_152);
        assert!(!s.support_ordering);
        // The SDK sends the total back on update: it is not multiplied again.
        let d2 = desc("<entry><content><QueueDescription><MaxSizeInMegabytes>49152</MaxSizeInMegabytes><EnablePartitioning>true</EnablePartitioning></QueueDescription></content></entry>");
        let s2 = parse(&d2, &Opts { kind: Kind::Queue, api: "2021-05", existing: Some(&s), now: 1 }).unwrap();
        assert_eq!(s2.max_size_mb, 49_152);
    }

    #[test]
    fn validations() {
        let o = |kind| Opts { kind, api: "2021-05", existing: None, now: 0 };
        let d = desc("<QueueDescription><LockDuration>P25D</LockDuration></QueueDescription>");
        assert!(parse(&d, &o(Kind::Queue)).is_err());
        let d =
            desc("<TopicDescription><DuplicateDetectionHistoryTimeWindow>P25D</DuplicateDetectionHistoryTimeWindow></TopicDescription>");
        assert!(parse(&d, &o(Kind::Topic)).is_err());
        let d = desc("<QueueDescription><MaxMessageSizeInKilobytes>1024</MaxMessageSizeInKilobytes></QueueDescription>");
        assert!(parse(&d, &o(Kind::Queue)).is_err());
        let d = desc("<QueueDescription><MaxMessageSizeInKilobytes>128</MaxMessageSizeInKilobytes></QueueDescription>");
        assert!(parse(&d, &Opts { api: "2017-04", ..o(Kind::Queue) }).is_err());
        let d = desc("<QueueDescription><Status>Bogus</Status></QueueDescription>");
        assert!(parse(&d, &o(Kind::Queue)).is_err());
        let d = desc("<QueueDescription />");
        let s = parse(&d, &o(Kind::Queue)).unwrap();
        assert_eq!((s.lock_duration_ms, s.max_size_mb, s.max_delivery_count), (60_000, 1024, 10));
        assert!(s.support_ordering);
    }
}
