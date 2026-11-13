//! Event sources of other emulated services: "the events of storage account X".
//!
//! On Azure, a storage account raises events into a *system topic* (an ARM resource,
//! `Microsoft.EventGrid/systemTopics`) and event subscriptions hang off that topic. Here a
//! source is named by the resource it belongs to (a [`SourceRef`]) and subscriptions hang off
//! the source directly. The ARM control plane maps system topics and their event subscriptions
//! onto the functions in this file.
//!
//! * [`EventGridService::put_source_subscription`] and friends manage subscriptions;
//! * [`EventGridService::raise_source_event`] is what a service calls when something happens;
//! * [`EventGridService::wants_source`] is the cheap "does anyone listen?" check.
//!
//! A source is stored like a topic (an entry in the state with kind `source`), so the filters,
//! both delivery schemas, webhook handshakes, retries, dead letters and the delivery log work as
//! for any other subscription. The admin API reaches the same data under
//! `/_azemu/eventgrid/sources/…` (see `admin.rs`).

use std::sync::Arc;

use azemu_core::time::{iso8601_7, OffsetDateTime};
use serde_json::{json, Value};

use crate::admin::Scope;
use crate::events::{Event, Schema, FAKE_SUBSCRIPTION};
use crate::model::{InputSchema, SubscriptionConfig, TopicKind};
use crate::pull::Now;
use crate::state::{key, Host, State};
use crate::{admin_subs, publish, Shared};

/// Prefix of the state key of a source, so it can never collide with a topic name (host names
/// have no `:`).
const KEY_PREFIX: &str = "src:";

/// Names an event source. Build one with [`SourceRef::storage_account`] or
/// [`SourceRef::from_resource_id`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRef {
    key: String,
    resource_id: String,
    explicit: bool,
}

impl SourceRef {
    fn named(provider: &str, kind: &str, name: &str) -> SourceRef {
        SourceRef {
            key: format!("{KEY_PREFIX}{}/{}/{}", provider.to_ascii_lowercase(), kind.to_ascii_lowercase(), name.to_ascii_lowercase()),
            resource_id: format!("/subscriptions/{FAKE_SUBSCRIPTION}/resourceGroups/azemu/providers/{provider}/{kind}/{name}"),
            explicit: false,
        }
    }

    /// The events of a storage account. The `topic` of its events is
    /// `/subscriptions/00000000-…/resourceGroups/azemu/providers/Microsoft.Storage/storageAccounts/{account}`
    /// unless a subscription was made with a real resource id ([`SourceRef::from_resource_id`]).
    pub fn storage_account(account: &str) -> SourceRef {
        SourceRef::named("Microsoft.Storage", "storageAccounts", account)
    }

    /// The events of a key vault (`Microsoft.KeyVault/vaults/{vault}`).
    pub fn key_vault(vault: &str) -> SourceRef {
        SourceRef::named("Microsoft.KeyVault", "vaults", vault)
    }

    /// The events of an App Configuration store (`Microsoft.AppConfiguration/configurationStores/{store}`).
    pub fn app_configuration_store(store: &str) -> SourceRef {
        SourceRef::named("Microsoft.AppConfiguration", "configurationStores", store)
    }

    /// The events of the resource with this ARM resource id
    /// (`/subscriptions/{s}/resourceGroups/{g}/providers/{Namespace}/{type}/{name}`). Two ids that
    /// differ only in the subscription, resource group or letter case name the same source,
    /// which is what lets the storage side find the source without knowing the ids.
    pub fn from_resource_id(id: &str) -> Result<SourceRef, String> {
        let segs: Vec<&str> = id.trim_matches('/').split('/').collect();
        let at = segs.iter().position(|s| s.eq_ignore_ascii_case("providers"));
        match at {
            Some(i) if segs.len() >= i + 4 && !segs[i + 3].is_empty() => Ok(SourceRef {
                key: format!(
                    "{KEY_PREFIX}{}/{}/{}",
                    segs[i + 1].to_ascii_lowercase(),
                    segs[i + 2].to_ascii_lowercase(),
                    segs[i + 3].to_ascii_lowercase()
                ),
                resource_id: format!("/{}", segs.join("/")),
                explicit: true,
            }),
            _ => Err(format!("'{id}' is not a resource id (expected /subscriptions/…/providers/<Namespace>/<type>/<name>).")),
        }
    }

    /// The resource id events carry as `topic` (Event Grid schema) or `source` (CloudEvents).
    pub fn resource_id(&self) -> &str {
        &self.resource_id
    }

    /// The key the source has in the admin API: `src:microsoft.storage/storageaccounts/{name}`.
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// One event raised by a service. The service fills `topic` (the source's resource id) and
/// `metadataVersion`.
#[derive(Clone, Debug)]
pub struct SourceEvent {
    pub subject: String,
    /// `Microsoft.Storage.BlobCreated` and so on.
    pub event_type: String,
    pub data: Value,
    /// `dataVersion` of the Event Grid schema. May be empty.
    pub data_version: String,
    /// Unique id; a new one is made when `None`.
    pub id: Option<String>,
    /// Event time; now when `None`.
    pub time: Option<OffsetDateTime>,
}

/// Why a subscription could not be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionError {
    /// The HTTP status the admin API answers with: 400 invalid, 404 not found, 501 not built.
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for SubscriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.message)
    }
}

impl std::error::Error for SubscriptionError {}

/// A subscription that was made or changed.
#[derive(Clone, Debug)]
pub struct SubscriptionOutcome {
    /// False when an existing subscription of that name was replaced.
    pub created: bool,
    /// The subscription as the admin API shows it (`name`, `deliveryMode`, `validation`,
    /// `stats`, and the configuration).
    pub view: Value,
}

/// Create the source entry when it is missing and keep a real resource id once one was given.
pub(crate) fn ensure(st: &mut State, src: &SourceRef) {
    let h = st.hosts.entry(src.key.clone()).or_insert_with(|| {
        let mut h = Host::new(&src.key, TopicKind::Source, InputSchema::EventGrid);
        h.resource_id = Some(src.resource_id.clone());
        h
    });
    if src.explicit {
        h.resource_id = Some(src.resource_id.clone());
    }
}

impl crate::EventGridService {
    /// `wants_source` for the events of a storage account, without building a `SourceRef`.
    pub fn wants_storage_account(&self, account: &str) -> bool {
        self.wants_named("microsoft.storage/storageaccounts", account)
    }

    /// `wants_source` for the events of a key vault.
    pub fn wants_key_vault(&self, vault: &str) -> bool {
        self.wants_named("microsoft.keyvault/vaults", vault)
    }

    /// `wants_source` for the events of an App Configuration store.
    pub fn wants_app_configuration_store(&self, store: &str) -> bool {
        self.wants_named("microsoft.appconfiguration/configurationstores", store)
    }

    fn wants_named(&self, type_path: &str, name: &str) -> bool {
        let k = format!("{KEY_PREFIX}{type_path}/{}", name.to_ascii_lowercase());
        let st = self.shared.state.lock();
        st.hosts.get(&k).map(|h| !h.subs.is_empty()).unwrap_or(false)
    }

    /// Is any subscription listening to this source? Cheap: one lock, one map lookup.
    pub fn wants_source(&self, source: &SourceRef) -> bool {
        let st = self.shared.state.lock();
        st.hosts.get(&source.key).map(|h| !h.subs.is_empty()).unwrap_or(false)
    }

    /// Raise an event of a source. Every subscription whose filter matches gets it, in the
    /// delivery schema it asked for (Event Grid schema by default). Returns the number of
    /// subscriptions that matched. Must be called inside the tokio runtime; never blocks.
    pub fn raise_source_event(&self, source: &SourceRef, event: SourceEvent) -> usize {
        raise(&self.shared, source, event)
    }

    /// Create or replace the subscription `name` on `source`.
    ///
    /// For a webhook destination the validation handshake runs when `validation_base` is the
    /// base URL of this emulator (for example `req.ctx.endpoint.base_url()`; the webhook can
    /// then call `{base}/_azemu/eventgrid/validate/{code}` by hand); with `None` the handshake
    /// is skipped and the subscription is marked `Skipped`.
    pub async fn put_source_subscription(
        &self,
        source: &SourceRef,
        name: &str,
        cfg: SubscriptionConfig,
        validation_base: Option<&str>,
    ) -> Result<SubscriptionOutcome, SubscriptionError> {
        {
            let mut st = self.shared.state.lock();
            ensure(&mut st, source);
        }
        let scope = Scope { host: source.key.clone(), topic: None, kind: TopicKind::Source };
        let r = admin_subs::create(&self.shared, scope, &source.key, name, cfg, validation_base).await;
        crate::state::next_revision();
        match r {
            Ok((created, view)) => Ok(SubscriptionOutcome { created, view }),
            Err((status, message)) => Err(SubscriptionError { status, message }),
        }
    }

    /// One subscription of a source, as the admin API shows it.
    pub fn get_source_subscription(&self, source: &SourceRef, name: &str) -> Option<Value> {
        let st = self.shared.state.lock();
        let sub = st.hosts.get(&source.key)?.subs.get(&key(name))?;
        Some(admin_subs::sub_view(sub, &Now::now()))
    }

    /// Every subscription of a source (empty when the source is unknown).
    pub fn list_source_subscriptions(&self, source: &SourceRef) -> Vec<Value> {
        let st = self.shared.state.lock();
        let now = Now::now();
        st.hosts.get(&source.key).map(|h| h.subs.values().map(|s| admin_subs::sub_view(s, &now)).collect()).unwrap_or_default()
    }

    /// Delete a subscription; false when there was none. Deliveries still waiting for it stop.
    pub fn delete_source_subscription(&self, source: &SourceRef, name: &str) -> bool {
        let removed = {
            let mut st = self.shared.state.lock();
            st.hosts.get_mut(&source.key).and_then(|h| h.subs.remove(&key(name))).is_some()
        };
        crate::state::next_revision();
        self.shared.notify.notify_waiters();
        removed
    }

    /// Delete a source with all its subscriptions; false when it was unknown.
    pub fn delete_source(&self, source: &SourceRef) -> bool {
        let removed = self.shared.state.lock().hosts.remove(&source.key).is_some();
        crate::state::next_revision();
        self.shared.notify.notify_waiters();
        removed
    }
}

pub(crate) fn raise(shared: &Arc<Shared>, source: &SourceRef, event: SourceEvent) -> usize {
    let now = Now::now();
    let time = event.time.unwrap_or(now.wall);
    let body = json!({
        "id": event.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        "subject": event.subject,
        "data": event.data,
        "eventType": event.event_type,
        "dataVersion": event.data_version,
        "eventTime": iso8601_7(time),
    });
    let ev = Event::new(Schema::EventGrid, body);
    let mut jobs = Vec::new();
    let matched = {
        let mut st = shared.state.lock();
        if !st.hosts.get(&source.key).map(|h| !h.subs.is_empty()).unwrap_or(false) {
            return 0;
        }
        st.fan_out(&source.key, None, &ev, &now, &mut jobs)
    };
    publish::finish_publish(shared, jobs);
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_account_and_resource_id_name_the_same_source() {
        let a = SourceRef::storage_account("MyAcct");
        let b =
            SourceRef::from_resource_id("/subscriptions/abc/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/myacct").unwrap();
        assert_eq!(a.key(), b.key());
        assert!(b.resource_id().contains("/subscriptions/abc/"));
        assert!(a.resource_id().contains("storageAccounts/MyAcct"));
    }

    #[test]
    fn resource_id_needs_providers() {
        assert!(SourceRef::from_resource_id("/subscriptions/abc").is_err());
        assert!(SourceRef::from_resource_id("/providers/Microsoft.Storage/storageAccounts").is_err());
    }
}
