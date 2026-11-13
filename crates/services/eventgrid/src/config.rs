//! The optional JSON config file (`AZEMU_EVENTGRID_CONFIG`): topics and subscriptions
//! a project wants to exist whenever the emulator starts (and after every reset).
//!
//! ```json
//! {
//!   "topics": [
//!     { "name": "orders", "inputSchema": "EventGridSchema",
//!       "subscriptions": [ { "name": "to-hook", "webhookUrl": "http://localhost:7071/hook",
//!                            "filter": { "includedEventTypes": ["Order.Created"] } } ] }
//!   ],
//!   "domains": [
//!     { "name": "tenants", "subscriptions": [],
//!       "topics": [ { "name": "acme", "subscriptions": [ { "name": "s", "webhookUrl": "http://localhost:7071/acme" } ] } ] }
//!   ],
//!   "namespaces": [
//!     { "name": "default",
//!       "topics": [ { "name": "jobs", "subscriptions": [ { "name": "workers", "lockDurationSeconds": 30, "maxDeliveryCount": 5 } ] } ] }
//!   ]
//! }
//! ```
//!
//! Webhook handshakes are not run for subscriptions that come from the file (the endpoint
//! is usually not up yet); they are marked `Skipped`.

use serde::Deserialize;

use crate::model::{InputSchema, SubscriptionConfig, TopicKind};
use crate::pull::Now;
use crate::state::{key, Host, State, Sub, Topic};
use crate::Options;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFile {
    #[serde(default)]
    pub topics: Vec<TopicEntry>,
    #[serde(default)]
    pub domains: Vec<DomainEntry>,
    #[serde(default)]
    pub namespaces: Vec<NamespaceEntry>,
    /// Subscriptions to the events of storage accounts (Blob and Data Lake events).
    #[serde(default)]
    pub storage_accounts: Vec<StorageAccountEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageAccountEntry {
    /// The storage account name.
    pub name: String,
    /// The ARM resource id events carry as `topic`; made up when missing.
    #[serde(default)]
    pub resource_id: Option<String>,
    #[serde(default)]
    pub subscriptions: Vec<SubEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubEntry {
    pub name: String,
    #[serde(flatten)]
    pub cfg: SubscriptionConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicEntry {
    pub name: String,
    #[serde(default)]
    pub input_schema: InputSchema,
    #[serde(default)]
    pub subscriptions: Vec<SubEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainEntry {
    pub name: String,
    #[serde(default)]
    pub input_schema: InputSchema,
    #[serde(default)]
    pub subscriptions: Vec<SubEntry>,
    #[serde(default)]
    pub topics: Vec<DomainTopicEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainTopicEntry {
    pub name: String,
    #[serde(default)]
    pub subscriptions: Vec<SubEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceEntry {
    pub name: String,
    #[serde(default)]
    pub topics: Vec<DomainTopicEntry>,
}

/// Load the config file named in the options into `st`. Problems are logged; whatever
/// is valid is still loaded.
pub fn load_into(opts: &Options, st: &mut State) {
    let Some(path) = &opts.config_path else { return };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("eventgrid: cannot read config file {}: {e}", path.display());
            return;
        }
    };
    match parse(&text) {
        Ok(cfg) => apply(&cfg, st, &opts.webhooks),
        Err(e) => tracing::error!("eventgrid: config file {} is not valid: {e}", path.display()),
    }
}

pub fn parse(text: &str) -> Result<ConfigFile, String> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}

fn add_subs(
    subs: &mut std::collections::BTreeMap<String, Sub>,
    entries: &[SubEntry],
    kind: TopicKind,
    now: &Now,
    policy: &crate::WebhookPolicy,
) {
    for e in entries {
        let mut cfg = e.cfg.clone();
        if let Err(msg) = cfg.normalize().and_then(|_| cfg.validate_with(kind, policy)) {
            tracing::error!("eventgrid: config: subscription '{}' skipped: {msg}", e.name);
            continue;
        }
        subs.insert(key(&e.name), Sub::new(&e.name, cfg, now.wall));
    }
}

pub fn apply(cfg: &ConfigFile, st: &mut State, policy: &crate::WebhookPolicy) {
    let now = Now::now();
    for t in &cfg.topics {
        let mut h = Host::new(&t.name, TopicKind::Custom, t.input_schema);
        add_subs(&mut h.subs, &t.subscriptions, TopicKind::Custom, &now, policy);
        st.hosts.insert(key(&t.name), h);
    }
    for d in &cfg.domains {
        let mut h = Host::new(&d.name, TopicKind::Domain, d.input_schema);
        add_subs(&mut h.subs, &d.subscriptions, TopicKind::Domain, &now, policy);
        for t in &d.topics {
            let mut topic = Topic { name: t.name.clone(), ..Topic::default() };
            add_subs(&mut topic.subs, &t.subscriptions, TopicKind::Domain, &now, policy);
            h.topics.insert(key(&t.name), topic);
        }
        st.hosts.insert(key(&d.name), h);
    }
    for a in &cfg.storage_accounts {
        let src = match a.resource_id.as_deref().map(crate::SourceRef::from_resource_id) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                tracing::error!("eventgrid: config: storage account '{}' skipped: {e}", a.name);
                continue;
            }
            None => crate::SourceRef::storage_account(&a.name),
        };
        crate::sources::ensure(st, &src);
        if let Some(h) = st.hosts.get_mut(src.key()) {
            add_subs(&mut h.subs, &a.subscriptions, TopicKind::Source, &now, policy);
        }
    }
    for n in &cfg.namespaces {
        let mut h = Host::new(&n.name, TopicKind::Namespace, InputSchema::CloudEvents);
        for t in &n.topics {
            let mut topic = Topic { name: t.name.clone(), input_schema: InputSchema::CloudEvents, ..Topic::default() };
            add_subs(&mut topic.subs, &t.subscriptions, TopicKind::Namespace, &now, policy);
            h.topics.insert(key(&t.name), topic);
        }
        st.hosts.insert(key(&n.name), h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_all_three_kinds() {
        let text = r#"{
          "topics":[{"name":"Orders","inputSchema":"EventGridSchema",
                     "subscriptions":[{"name":"hook","webhookUrl":"http://127.0.0.1:9/h","filter":{"includedEventTypes":["A"]}}]}],
          "domains":[{"name":"dom","topics":[{"name":"t1","subscriptions":[{"name":"s","webhookUrl":"http://127.0.0.1:9/x"}]}]}],
          "namespaces":[{"name":"ns","topics":[{"name":"jobs","subscriptions":[{"name":"w","lockDurationSeconds":5,"maxDeliveryCount":3}]}]}]
        }"#;
        let cfg = parse(text).unwrap();
        let mut st = State::default();
        apply(&cfg, &mut st, &crate::WebhookPolicy::allow_all());
        let orders = st.host("orders").unwrap();
        assert_eq!(orders.kind, TopicKind::Custom);
        assert_eq!(orders.input_schema, InputSchema::EventGrid);
        assert_eq!(orders.subs["hook"].cfg.filter.included_event_types, vec!["A"]);
        assert_eq!(st.host("dom").unwrap().topics["t1"].subs.len(), 1);
        let ns = st.host("ns").unwrap();
        assert_eq!(ns.kind, TopicKind::Namespace);
        let w = &ns.topics["jobs"].subs["w"];
        assert_eq!(w.cfg.pull_params().max_delivery, 3);
    }

    #[test]
    fn invalid_subscription_is_skipped_not_fatal() {
        // push needs a URL; pull is not allowed on a custom topic
        let cfg = parse(
            r#"{"topics":[{"name":"t","subscriptions":[{"name":"bad","deliveryMode":"pull"},{"name":"ok","webhookUrl":"http://h/x"}]}]}"#,
        )
        .unwrap();
        let mut st = State::default();
        apply(&cfg, &mut st, &crate::WebhookPolicy::allow_all());
        let subs = &st.host("t").unwrap().subs;
        assert!(subs.contains_key("ok") && !subs.contains_key("bad"));
    }

    #[test]
    fn bad_json_is_an_error() {
        assert!(parse("{").is_err());
    }
}
