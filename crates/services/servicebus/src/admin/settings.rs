//! Request description (`QueueDescription` / `TopicDescription` / `SubscriptionDescription`)
//! to [`Settings`], with the validations the real service applies.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use super::error::{ApiError, ApiResult, ErrKind};
use super::util::timespan_text;
use super::xml::Node;
use crate::duration::{self, TIMESPAN_MAX_MS};
use crate::model::{AuthRule, Settings};
use crate::state::Namespace;

/// The api-versions the service takes (Azure's own list in the answer to a request with another one).
pub const API_VERSIONS: &[&str] = &[
    "2012-03",
    "2012-08",
    "2013-04",
    "2013-07",
    "2013-08",
    "2013-10",
    "2014-01",
    "2014-05",
    "2014-08",
    "2014-09",
    "2015-01",
    "2015-04",
    "2015-08",
    "2016-03",
    "2016-07",
    "2017-04",
    "2020-04",
    "2020-09-01-preview",
    "2021-05",
    "2021-10",
    "2022-10",
    "2023-07-01",
    "2024-05",
];

/// Largest `MaxSizeInMegabytes` (Premium: 80 GB; Standard stops at 5 GB, see the page of the service).
const MAX_SIZE_MB: i64 = 81_920;
/// Largest `MaxMessageSizeInKilobytes` (Premium: 100 MB; Standard takes 256 only).
const MAX_MESSAGE_KB: i64 = 102_400;

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
    /// The namespace the entity is in, to check that the targets of a forward exist (`None`: not checked).
    pub namespace: Option<&'a Namespace>,
    /// How the service names the entity in its error texts: `<namespace>:Queue:<name>`, `<namespace>:Topic:<topic>|<subscription>`.
    pub id: &'a str,
    /// `AZEMU_SERVICEBUS_TIER=standard`: refuse what only the Standard tier refuses (see [`standard_tier`]).
    pub standard: bool,
}

/// Is the emulator set to behave as a Standard namespace (`AZEMU_SERVICEBUS_TIER=standard`)? Standard takes 256 KB as the one message size
/// and at most 5 GB per queue or topic; by default the emulator takes what Premium takes too.
pub fn standard_tier() -> bool {
    static STANDARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STANDARD.get_or_init(|| {
        azemu_core::settings::text("AZEMU_SERVICEBUS_TIER").map(|v| v.trim().eq_ignore_ascii_case("standard")).unwrap_or(false)
    })
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
        "true" | "True" | "1" => Ok(true),
        "false" | "False" | "0" => Ok(false),
        _ => Err(ApiError::bad_request()),
    }
}

fn int(v: &str) -> ApiResult<i64> {
    v.parse::<i64>().map_err(|_| ApiError::bad_request())
}

fn dur(v: &str) -> ApiResult<i64> {
    // a negative duration (`-PT5S`) is a duration: the check of the property says it is not allowed
    match v.trim().strip_prefix('-') {
        Some(rest) => duration::parse(rest).map(|ms| -ms).ok_or_else(ApiError::bad_request),
        None => duration::parse(v).ok_or_else(ApiError::bad_request),
    }
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

/// Check the value of `ForwardTo` / `ForwardDeadLetteredMessagesTo`: an address (`sb://host/path`) of a queue or a topic that exists. The
/// texts are the service's (a subscription is answered differently from a queue, and an update differently from a create).
fn check_forward(v: &str, param: &str, o: &Opts) -> ApiResult<()> {
    let v = v.trim();
    if v.is_empty() {
        return Ok(());
    }
    if !v.contains("://") {
        return Err(match o.kind {
            Kind::Subscription => ApiError::bad_raw(format!("BR0012The specified value is in an invalid format.\r\nParameter name: {param}.")),
            _ => ApiError::bad_raw(format!(
                "SubCode=40000. Absolute URI must be provided.\r\nParameter name: {param} ExceptionDescription: Absolute URI must be provided.\r\nParameter name: {param}."
            )),
        });
    }
    let Some(ns) = o.namespace else { return Ok(()) };
    let path = normalize_forward(v).unwrap_or_default();
    if ns.queue(&path).is_some() || ns.topic(&path).is_some() {
        return Ok(());
    }
    Err(match (o.kind, o.existing.is_some()) {
        (Kind::Subscription, _) => {
            ApiError::bad_raw(format!("The auto forwarding destination \"{v}\" on entity \"{}\" does not exist. .", o.id))
        }
        (_, true) => {
            ApiError::bad_raw(format!("SubCode=40400. The auto forwarding destination \"{v}\" on entity \"{}\" does not exist. .", o.id))
        }
        (_, false) => ApiError::bad_raw(format!("SubCode=40400. The auto forwarding destination entity {v} does not exist.")),
    })
}

/// The text a value of `Status` that the entity cannot have is refused with.
fn status_error(kind: Kind, status: &str, id: &str) -> Option<ApiError> {
    match (kind, status) {
        (Kind::Topic, "ReceiveDisabled") => {
            Some(ApiError::bad_raw(format!("SubCode=40000. Entity 'Status' cannot be set to 'ReceiveDisabled' for '{id}'. ExceptionDescription: Entity 'Status' cannot be set to 'ReceiveDisabled' for '{id}'.")))
        }
        (Kind::Subscription, "SendDisabled") => {
            Some(ApiError::bad_raw(format!("BR0012Entity 'Status' cannot be set to 'SendDisabled' for '{id}'.")))
        }
        _ => None,
    }
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
            "LockDuration" if not_topic => {
                s.lock_duration_ms = dur(v)?;
                if s.lock_duration_ms <= 0 {
                    return Err(ApiError::bad_param(
                        &format!(
                            "Argument LockDuration must be a positive timeout value. The provided value was {}.",
                            timespan_text(s.lock_duration_ms)
                        ),
                        "LockDuration",
                        &timespan_text(s.lock_duration_ms),
                        kind == Kind::Queue,
                        false,
                    ));
                }
            }
            "MaxSizeInMegabytes" if not_sub => size = Some(int(v)?),
            "RequiresDuplicateDetection" if not_sub => s.requires_duplicate_detection = boolean(v)?,
            "RequiresSession" if not_topic => s.requires_session = boolean(v)?,
            "DefaultMessageTimeToLive" => {
                s.default_ttl_ms = opt_dur(v)?;
                if matches!(s.default_ttl_ms, Some(ms) if ms < 1000) {
                    return Err(ApiError::bad_param(
                        "The value supplied must be between 00:00:01 and 10675199.02:48:05.4775807.",
                        "DefaultMessageTimeToLive",
                        &timespan_text(s.default_ttl_ms.unwrap_or(0)),
                        kind != Kind::Subscription,
                        false,
                    ));
                }
            }
            "DeadLetteringOnMessageExpiration" if not_topic => s.dead_letter_on_expiry = boolean(v)?,
            "DeadLetteringOnFilterEvaluationExceptions" if kind == Kind::Subscription => s.dead_letter_on_filter_exception = boolean(v)?,
            "DuplicateDetectionHistoryTimeWindow" if not_sub => {
                s.duplicate_window_ms = dur(v)?;
                if s.duplicate_window_ms <= 0 {
                    return Err(ApiError::bad_param(
                        &format!(
                            "Argument DuplicateDetectionHistoryTimeWindow must be a positive timeout value. The provided value was {}.",
                            timespan_text(s.duplicate_window_ms)
                        ),
                        "DuplicateDetectionHistoryTimeWindow",
                        &timespan_text(s.duplicate_window_ms),
                        true,
                        false,
                    ));
                }
            }
            "MaxDeliveryCount" if not_topic => {
                let n = int(v)?;
                if n > i64::from(i32::MAX) {
                    return Err(ApiError::new(ErrKind::Internal));
                }
                s.max_delivery_count = match u32::try_from(n).ok().filter(|n| *n >= 1) {
                    Some(n) => n,
                    None if kind == Kind::Subscription => {
                        return Err(ApiError::bad_raw(format!(
                            "BR0012The value supplied must be between 1 and 2147483647.\r\nParameter name: value\r\nActual value was {n}."
                        )))
                    }
                    None => {
                        return Err(ApiError::bad_param(
                            "The value supplied must be between 1 and 2147483647.",
                            "MaxDeliveryCount",
                            &n.to_string(),
                            true,
                            false,
                        ))
                    }
                };
            }
            "EnableBatchedOperations" => s.enable_batched_operations = boolean(v)?,
            "IsAnonymousAccessible" if not_sub => s.is_anonymous_accessible = boolean(v)?,
            "AuthorizationRules" if not_sub => s.auth_rules = auth_rules(c)?,
            "Status" => {
                if !matches!(v, "Active" | "Disabled" | "SendDisabled" | "ReceiveDisabled" | "Creating") {
                    return Err(ApiError::bad_request());
                }
                if let Some(e) = status_error(kind, v, o.id) {
                    return Err(e);
                }
                s.status = v.to_string();
            }
            "SupportOrdering" if not_sub => {
                s.support_ordering = boolean(v)?;
                ordering_set = true;
            }
            "AutoDeleteOnIdle" => {
                s.auto_delete_on_idle_ms = opt_dur(v)?;
                if let Some(ms) = s.auto_delete_on_idle_ms.filter(|ms| *ms < 300_000) {
                    return Err(ApiError::bad_param(
                        "The value supplied must be between 00:05:00 and 10675199.02:48:05.4775807.",
                        "AutoDeleteOnIdle",
                        &timespan_text(ms),
                        kind != Kind::Subscription,
                        false,
                    ));
                }
            }
            "EnablePartitioning" if not_sub => s.enable_partitioning = boolean(v)?,
            "EnableExpress" if not_sub => s.enable_express = boolean(v)?,
            "EnableSubscriptionPartitioning" if kind == Kind::Topic => s.enable_subscription_partitioning = boolean(v)?,
            "FilteringMessagesBeforePublishing" if kind == Kind::Topic => s.filtering_messages_before_publishing = boolean(v)?,
            "ForwardTo" if not_topic => {
                check_forward(v, "ForwardTo", o)?;
                s.forward_to = normalize_forward(v);
            }
            "ForwardDeadLetteredMessagesTo" if not_topic => {
                check_forward(v, "ForwardDeadLetteredMessagesTo", o)?;
                s.forward_dead_lettered_to = normalize_forward(v);
            }
            "UserMetadata" => {
                if c.text.chars().count() > 1024 {
                    return Err(ApiError::bad_param(
                        "The value supplied must be between 0 and 1024.",
                        "UserMetadata",
                        &c.text.chars().count().to_string(),
                        kind != Kind::Subscription,
                        false,
                    ));
                }
                s.user_metadata = Some(c.text.clone()).filter(|t| !t.is_empty());
            }
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
                // Standard takes 256 and nothing else; Premium 1,024 to 102,400. The service of the emulator is every tier, so the Premium
                // values are taken (what only Standard refuses is not).
                if kb != 256 && (o.standard || !(1024..=MAX_MESSAGE_KB).contains(&kb)) {
                    if o.standard {
                        // the answer of the recordings (a Standard namespace)
                        return Err(ApiError::bad_request());
                    }
                    let allowed = format!("256 or from 1024 to {MAX_MESSAGE_KB}");
                    return Err(ApiError::bad_raw(format!(
                        "SubCode=40000. Bad Request. To know more visit https://aka.ms/sbResourceMgrExceptions.  ExceptionDescription: The specified value MaxMessageSizeInKilobytes is invalid. The property MaxMessageSizeInKilobytes, must be {allowed}. To know more visit https://aka.ms/sbResourceMgrExceptions\r\nParameter name: MaxMessageSizeInKilobytes\r\nActual value was {kb}."
                    )));
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
        // The sizes of an entity that is not partitioned are whole gigabytes: Azure refuses the others (and 0 is the default size).
        let size_error = |sz: i64| {
            let most = if o.standard { 5120 } else { MAX_SIZE_MB };
            let allowed: Vec<String> = (1..=most / 1024).map(|g| (g * 1024).to_string()).collect();
            ApiError::bad_raw(format!(
                "SubCode=40000. The specified value {sz} is invalid. The property MaxSizeInMegabytes, must be one of the following values: {}. To know more visit https://aka.ms/sbResourceMgrExceptions.",
                allowed.join(";")
            ))
        };
        if let Some(sz) = size.filter(|_| !s.enable_partitioning && !matches!(o.existing, Some(old) if old.enable_partitioning)) {
            let most = if o.standard { 5120 } else { MAX_SIZE_MB };
            if sz != 0 && (sz % 1024 != 0 || !(1024..=most).contains(&sz)) {
                return Err(size_error(sz));
            }
        }
        s.max_size_mb = match (size, o.existing) {
            (Some(sz), Some(old)) if old.enable_partitioning && s.enable_partitioning && sz == old.max_size_mb => sz,
            (Some(0), _) => 1024 * if s.enable_partitioning { 16 } else { 1 },
            (Some(sz), _) if sz >= 1 => sz.checked_mul(if s.enable_partitioning { 16 } else { 1 }).ok_or_else(ApiError::bad_request)?,
            (Some(sz), _) => return Err(size_error(sz)),
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
    // Microsoft Learn says 20 seconds is the least, but Azure takes any positive window up to 7 days (checked in November 2026)
    if not_sub && s.duplicate_window_ms > 7 * 86_400_000 {
        return Err(ApiError::bad_param(
            "The value supplied must be between 00:00:20 and 7.00:00:00.",
            "DuplicateDetectionHistoryTimeWindow",
            &timespan_text(s.duplicate_window_ms),
            true,
            false,
        ));
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
        let s = parse(
            &d,
            &Opts { kind: Kind::Queue, api: "2021-05", existing: None, now: 0, namespace: None, id: "ns:Queue:q", standard: false },
        )
        .unwrap();
        assert_eq!(s.max_size_mb, 49_152);
        assert!(!s.support_ordering);
        // The SDK sends the total back on update: it is not multiplied again.
        let d2 = desc("<entry><content><QueueDescription><MaxSizeInMegabytes>49152</MaxSizeInMegabytes><EnablePartitioning>true</EnablePartitioning></QueueDescription></content></entry>");
        let s2 = parse(
            &d2,
            &Opts { kind: Kind::Queue, api: "2021-05", existing: Some(&s), now: 1, namespace: None, id: "ns:Queue:q", standard: false },
        )
        .unwrap();
        assert_eq!(s2.max_size_mb, 49_152);
    }

    #[test]
    fn the_standard_tier_refuses_what_only_it_refuses() {
        let o = |standard| Opts { kind: Kind::Queue, api: "2021-05", existing: None, now: 0, namespace: None, id: "ns:Queue:q", standard };
        for (inner, ok_any) in [
            ("<MaxMessageSizeInKilobytes>1024</MaxMessageSizeInKilobytes>", true),
            ("<MaxSizeInMegabytes>81920</MaxSizeInMegabytes>", true),
            ("<MaxMessageSizeInKilobytes>256</MaxMessageSizeInKilobytes>", false),
        ] {
            let d = desc(&format!("<QueueDescription>{inner}</QueueDescription>"));
            assert!(parse(&d, &o(false)).is_ok(), "{inner}");
            assert_eq!(parse(&d, &o(true)).is_err(), ok_any, "{inner}");
        }
    }

    #[test]
    fn validations() {
        let o = |kind| Opts { kind, api: "2021-05", existing: None, now: 0, namespace: None, id: "ns:Queue:q", standard: false };
        let d = desc("<QueueDescription><LockDuration>P25D</LockDuration></QueueDescription>");
        assert!(parse(&d, &o(Kind::Queue)).is_err());
        let d =
            desc("<TopicDescription><DuplicateDetectionHistoryTimeWindow>P25D</DuplicateDetectionHistoryTimeWindow></TopicDescription>");
        assert!(parse(&d, &o(Kind::Topic)).is_err());
        let d = desc("<QueueDescription><MaxMessageSizeInKilobytes>255</MaxMessageSizeInKilobytes></QueueDescription>");
        assert!(parse(&d, &o(Kind::Queue)).is_err());
        let d = desc("<QueueDescription><MaxMessageSizeInKilobytes>1024</MaxMessageSizeInKilobytes></QueueDescription>");
        assert!(parse(&d, &o(Kind::Queue)).is_ok());
        let d = desc("<QueueDescription><MaxMessageSizeInKilobytes>256</MaxMessageSizeInKilobytes></QueueDescription>");
        assert!(parse(&d, &Opts { api: "2017-04", ..o(Kind::Queue) }).is_err());
        let d = desc("<QueueDescription><Status>Bogus</Status></QueueDescription>");
        assert!(parse(&d, &o(Kind::Queue)).is_err());
        let d = desc("<QueueDescription />");
        let s = parse(&d, &o(Kind::Queue)).unwrap();
        assert_eq!((s.lock_duration_ms, s.max_size_mb, s.max_delivery_count), (60_000, 1024, 10));
        assert!(s.support_ordering);
    }
}
