//! What the service remembers: one `Vault` per vault name, each with a collection of
//! secrets, keys and certificates. All of it sits behind one lock in the service;
//! the lock is never held across an `.await`.

use std::collections::{BTreeMap, HashMap};

use crate::common::RECOVERABLE_DAYS;

/// One stored version of an object.
pub trait Versioned {
    fn version(&self) -> &str;
}

/// When an object was soft-deleted and when it will be purged for good.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Deleted {
    pub deleted_date: i64,
    pub scheduled_purge_date: i64,
}

impl Deleted {
    pub fn at(now: i64) -> Deleted {
        Deleted { deleted_date: now, scheduled_purge_date: now + RECOVERABLE_DAYS * 86_400 }
    }
}

/// A named object with all its versions (oldest first; the last one is "latest").
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Item<V, X> {
    /// The name as it was first written (lookups ignore case).
    pub name: String,
    pub versions: Vec<V>,
    pub deleted: Option<Deleted>,
    /// Per-object data that is not per-version (rotation policy, ...).
    pub extra: X,
}

impl<V: Versioned, X> Item<V, X> {
    pub fn latest(&self) -> Option<&V> {
        self.versions.last()
    }

    pub fn find(&self, version: &str) -> Option<&V> {
        self.versions.iter().find(|v| v.version() == version)
    }

    pub fn find_mut(&mut self, version: &str) -> Option<&mut V> {
        self.versions.iter_mut().find(|v| v.version() == version)
    }

    /// The version a request names; `None` or "" means latest.
    pub fn pick(&self, version: Option<&str>) -> Option<&V> {
        match version {
            None | Some("") => self.latest(),
            Some(v) => self.find(v),
        }
    }
}

/// All objects of one kind in one vault, keyed by lower-cased name.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Collection<V, X> {
    pub items: BTreeMap<String, Item<V, X>>,
}

impl<V, X> Default for Collection<V, X> {
    fn default() -> Self {
        Collection { items: BTreeMap::new() }
    }
}

pub fn norm(name: &str) -> String {
    name.to_ascii_lowercase()
}

impl<V: Versioned, X> Collection<V, X> {
    pub fn active(&self, name: &str) -> Option<&Item<V, X>> {
        self.items.get(&norm(name)).filter(|i| i.deleted.is_none())
    }

    pub fn active_mut(&mut self, name: &str) -> Option<&mut Item<V, X>> {
        self.items.get_mut(&norm(name)).filter(|i| i.deleted.is_none())
    }

    pub fn deleted(&self, name: &str) -> Option<&Item<V, X>> {
        self.items.get(&norm(name)).filter(|i| i.deleted.is_some())
    }

    pub fn exists(&self, name: &str) -> bool {
        self.items.contains_key(&norm(name))
    }

    pub fn is_deleted(&self, name: &str) -> bool {
        self.deleted(name).is_some()
    }

    /// Drop soft-deleted objects whose purge date has passed.
    pub fn sweep(&mut self, now: i64) {
        self.items.retain(|_, i| i.deleted.map(|d| d.scheduled_purge_date > now).unwrap_or(true));
    }

    /// Items in the given state (`deleted == true` for the deleted list), sorted by name.
    pub fn listing(&self, deleted: bool) -> Vec<(String, &Item<V, X>)> {
        self.items.iter().filter(|(_, i)| i.deleted.is_some() == deleted).map(|(k, i)| (k.clone(), i)).collect()
    }
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Vault {
    pub secrets: Collection<crate::secrets::SecretVersion, ()>,
    pub keys: Collection<crate::keys::KeyVersion, crate::keys::KeyExtra>,
    pub certs: Collection<crate::certificates::CertVersion, crate::certificates::CertExtra>,
    /// Certificate issuers by lower-cased name.
    pub issuers: BTreeMap<String, crate::cert_policy::Issuer>,
    /// Certificate contacts (the list from the last `PUT /certificates/contacts`).
    pub contacts: Option<Vec<serde_json::Value>>,
}

#[derive(Default)]
pub struct State {
    /// Vault name (lower case) -> vault. Created on first use.
    pub vaults: HashMap<String, Vault>,
}

impl State {
    pub fn vault(&mut self, name: &str, now: i64) -> &mut Vault {
        let v = self.vaults.entry(name.to_ascii_lowercase()).or_default();
        v.secrets.sweep(now);
        v.keys.sweep(now);
        v.certs.sweep(now);
        v
    }
}
