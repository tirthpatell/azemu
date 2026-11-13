//! Link addresses of the Event Hubs data plane.
//!
//! The Python SDK sends full URLs (`amqps://host:port/hub/ConsumerGroups/$Default/Partitions/0`);
//! other SDKs send bare paths. Both are accepted, scheme and authority are dropped.

#[derive(Debug, Clone, PartialEq)]
pub enum Address {
    Cbs,
    /// The namespace level `$management` node (what the SDKs attach to).
    Management,
    /// `{hub}/$management`
    HubManagement(String),
    /// A producer address: the hub itself.
    Hub(String),
    /// `{hub}/Partitions/{n}`
    Partition {
        hub: String,
        pid: String,
    },
    /// `{hub}/ConsumerGroups/{cg}/Partitions/{n}`
    Consumer {
        hub: String,
        cg: String,
        pid: String,
    },
}

impl Address {
    pub fn parse(raw: &str) -> Option<Address> {
        let mut s = raw.trim();
        if let Some(i) = s.find("://") {
            let rest = &s[i + 3..];
            s = rest.split_once('/').map(|(_, p)| p).unwrap_or("");
        }
        let s = s.trim_matches('/');
        let segs: Vec<&str> = s.split('/').collect();
        let is = |i: usize, w: &str| segs[i].eq_ignore_ascii_case(w);
        if segs.iter().any(|p| p.is_empty()) {
            return None;
        }
        match segs.len() {
            1 if is(0, "$cbs") => Some(Address::Cbs),
            1 if is(0, "$management") => Some(Address::Management),
            1 => Some(Address::Hub(segs[0].to_string())),
            2 if is(1, "$management") => Some(Address::HubManagement(segs[0].to_string())),
            3 if is(1, "partitions") => Some(Address::Partition { hub: segs[0].to_string(), pid: segs[2].to_string() }),
            5 if is(1, "consumergroups") && is(3, "partitions") => {
                Some(Address::Consumer { hub: segs[0].to_string(), cg: segs[2].to_string(), pid: segs[4].to_string() })
            }
            _ => None,
        }
    }

    /// An address only Event Hubs uses: a Service Bus entity never has this shape.
    pub fn is_unmistakably_eventhubs(&self) -> bool {
        matches!(self, Address::Partition { .. } | Address::Consumer { .. } | Address::Management)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sdk_urls_and_bare_paths() {
        assert_eq!(Address::parse("$cbs"), Some(Address::Cbs));
        assert_eq!(Address::parse("$management"), Some(Address::Management));
        assert_eq!(Address::parse("amqps://localhost:5672/hub"), Some(Address::Hub("hub".into())));
        assert_eq!(
            Address::parse("amqps://h:1/hub/ConsumerGroups/$Default/Partitions/3"),
            Some(Address::Consumer { hub: "hub".into(), cg: "$Default".into(), pid: "3".into() })
        );
        assert_eq!(Address::parse("hub/partitions/1"), Some(Address::Partition { hub: "hub".into(), pid: "1".into() }));
        assert_eq!(Address::parse("hub/$management"), Some(Address::HubManagement("hub".into())));
        assert_eq!(Address::parse("a/b"), None);
        assert!(Address::parse("h/Partitions/0").unwrap().is_unmistakably_eventhubs());
        assert!(!Address::parse("h").unwrap().is_unmistakably_eventhubs());
    }
}
