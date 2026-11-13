//! Entity settings: what the HTTP management API creates and the message engine reads.

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuthRule {
    pub claim_type: String,
    pub claim_value: String,
    pub rights: Vec<String>,
    pub key_name: String,
    pub primary_key: String,
    pub secondary_key: String,
}

/// Every property of a queue, topic or subscription description. Each kind uses the subset it
/// has in the ATOM API; the message engine reads only the first group.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    // ---- used by the engine ----
    pub lock_duration_ms: i64,
    pub max_delivery_count: u32,
    /// `None` = never expires (the API shows TimeSpan.MaxValue).
    pub default_ttl_ms: Option<i64>,
    pub dead_letter_on_expiry: bool,
    pub requires_session: bool,
    pub requires_duplicate_detection: bool,
    pub duplicate_window_ms: i64,
    /// Entity path (not a URL) after normalising `sb://host/path`.
    pub forward_to: Option<String>,
    pub forward_dead_lettered_to: Option<String>,
    pub max_size_mb: i64,
    pub max_message_size_kb: i64,
    pub status: String,
    // ---- stored and echoed by the API ----
    pub enable_batched_operations: bool,
    pub auto_delete_on_idle_ms: Option<i64>,
    pub user_metadata: Option<String>,
    pub enable_partitioning: bool,
    pub enable_express: bool,
    pub support_ordering: bool,
    pub dead_letter_on_filter_exception: bool,
    pub filtering_messages_before_publishing: bool,
    pub enable_subscription_partitioning: bool,
    pub is_anonymous_accessible: bool,
    pub auth_rules: Vec<AuthRule>,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub accessed_ms: i64,
}

impl Settings {
    pub fn queue_default(now: i64) -> Settings {
        Settings {
            lock_duration_ms: 60_000,
            max_delivery_count: 10,
            default_ttl_ms: None,
            dead_letter_on_expiry: false,
            requires_session: false,
            requires_duplicate_detection: false,
            duplicate_window_ms: 600_000,
            forward_to: None,
            forward_dead_lettered_to: None,
            max_size_mb: 1024,
            max_message_size_kb: 256,
            status: "Active".into(),
            enable_batched_operations: true,
            auto_delete_on_idle_ms: None,
            user_metadata: None,
            enable_partitioning: false,
            enable_express: false,
            support_ordering: false,
            dead_letter_on_filter_exception: false,
            filtering_messages_before_publishing: false,
            enable_subscription_partitioning: false,
            is_anonymous_accessible: false,
            auth_rules: vec![],
            created_ms: now,
            updated_ms: now,
            accessed_ms: 0,
        }
    }

    pub fn topic_default(now: i64) -> Settings {
        Settings { support_ordering: true, ..Settings::queue_default(now) }
    }

    pub fn subscription_default(now: i64) -> Settings {
        Settings { dead_letter_on_filter_exception: true, ..Settings::queue_default(now) }
    }

    pub fn max_message_size_bytes(&self) -> u64 {
        (self.max_message_size_kb.max(1) as u64) * 1024
    }

    pub fn max_size_bytes(&self) -> u64 {
        (self.max_size_mb.max(1) as u64) * 1024 * 1024
    }
}

/// Which sub-queue of an entity a link or operation addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubQueue {
    Main,
    DeadLetter,
    TransferDeadLetter,
}

/// What an address in an attach frame points to, after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// `$cbs`
    Cbs,
    /// `<entity>/$management` (entity is a queue path or `topic/Subscriptions/sub`).
    Management(String),
    Queue {
        name: String,
        sub: SubQueue,
    },
    Topic {
        name: String,
    },
    Subscription {
        topic: String,
        name: String,
        sub: SubQueue,
    },
}

impl Address {
    /// Parse an attach address: `queue`, `queue/$DeadLetterQueue`, `topic`,
    /// `topic/Subscriptions/sub[/$DeadLetterQueue]`, `x/$management`, `$cbs`; any of them
    /// may be prefixed with `scheme://host[:port]/`. Returns `None` for shapes that are no
    /// entity address. Whether `name` is a queue or a topic is decided later (`Queue` is
    /// returned for a single segment).
    pub fn parse(raw: &str) -> Option<Address> {
        let mut s = raw.trim();
        if let Some(i) = s.find("://") {
            let rest = &s[i + 3..];
            s = match rest.find('/') {
                Some(j) => &rest[j + 1..],
                None => "",
            };
        }
        let s = s.trim_matches('/');
        if s.is_empty() {
            return None;
        }
        if s.eq_ignore_ascii_case("$cbs") {
            return Some(Address::Cbs);
        }
        let segs: Vec<&str> = s.split('/').collect();
        let last = segs.last().copied().unwrap_or("");
        if last.eq_ignore_ascii_case("$management") {
            if segs.len() < 2 {
                return None;
            }
            return Some(Address::Management(segs[..segs.len() - 1].join("/")));
        }
        let (segs, sub) = match segs.as_slice() {
            [rest @ .., t, d] if t.eq_ignore_ascii_case("$Transfer") && d.eq_ignore_ascii_case("$DeadLetterQueue") => {
                (rest.to_vec(), SubQueue::TransferDeadLetter)
            }
            [rest @ .., d] if d.eq_ignore_ascii_case("$DeadLetterQueue") => (rest.to_vec(), SubQueue::DeadLetter),
            _ => (segs.clone(), SubQueue::Main),
        };
        match segs.as_slice() {
            [q] if !q.is_empty() => Some(Address::Queue { name: (*q).to_string(), sub }),
            [t, subs, s] if subs.eq_ignore_ascii_case("Subscriptions") && !t.is_empty() && !s.is_empty() => {
                Some(Address::Subscription { topic: (*t).to_string(), name: (*s).to_string(), sub })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_forms() {
        assert_eq!(Address::parse("$cbs"), Some(Address::Cbs));
        assert_eq!(Address::parse("q"), Some(Address::Queue { name: "q".into(), sub: SubQueue::Main }));
        assert_eq!(Address::parse("amqps://localhost:5671/q"), Some(Address::Queue { name: "q".into(), sub: SubQueue::Main }));
        assert_eq!(
            Address::parse("sb://ns.servicebus.windows.net/Q/$DeadLetterQueue"),
            Some(Address::Queue { name: "Q".into(), sub: SubQueue::DeadLetter })
        );
        assert_eq!(
            Address::parse("q/$Transfer/$DeadLetterQueue"),
            Some(Address::Queue { name: "q".into(), sub: SubQueue::TransferDeadLetter })
        );
        assert_eq!(Address::parse("q/$management"), Some(Address::Management("q".into())));
        assert_eq!(Address::parse("t/Subscriptions/s/$management"), Some(Address::Management("t/Subscriptions/s".into())));
        assert_eq!(
            Address::parse("amqps://h/t/subscriptions/s/$deadletterqueue"),
            Some(Address::Subscription { topic: "t".into(), name: "s".into(), sub: SubQueue::DeadLetter })
        );
        assert_eq!(Address::parse(""), None);
        assert_eq!(Address::parse("a/b"), None);
        assert_eq!(Address::parse("$management"), None);
    }
}
