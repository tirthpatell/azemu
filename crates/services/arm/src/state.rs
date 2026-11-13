//! What ARM remembers: resource groups and resources.

use std::collections::{BTreeMap, HashMap};

use azemu_core::time::OffsetDateTime;
use serde_json::{Map, Value};

pub struct Group {
    pub sub: String,
    pub name: String,
    pub location: String,
    pub tags: Map<String, Value>,
    pub managed_by: Option<String>,
}

/// One stored resource: the JSON the client sees (`body`) plus provider-private
/// state (`extra`, for example key generations) that is never sent out.
#[derive(Clone)]
pub struct Stored {
    pub id: String,
    pub sub: String,
    pub rg: String,
    pub ns: String,
    /// Canonical type segments: `["storageAccounts", "blobServices", "containers"]`.
    pub types: Vec<String>,
    pub names: Vec<String>,
    pub body: Value,
    pub extra: Value,
    pub created: OffsetDateTime,
}

impl Stored {
    pub fn name(&self) -> &str {
        self.names.last().map(String::as_str).unwrap_or("")
    }

    /// `storageAccounts/blobServices/containers`
    pub fn type_path(&self) -> String {
        self.types.join("/")
    }

    /// `Microsoft.Storage/storageAccounts/blobServices/containers`
    pub fn type_string(&self) -> String {
        format!("{}/{}", self.ns, self.types.join("/"))
    }

    pub fn name_at(&self, depth: usize) -> &str {
        self.names.get(depth).map(String::as_str).unwrap_or("")
    }

    /// A string property of `body.properties`.
    pub fn prop_str(&self, name: &str) -> Option<&str> {
        self.body.get("properties")?.get(name)?.as_str()
    }
}

pub fn key(id: &str) -> String {
    id.to_ascii_lowercase()
}

pub fn group_key(sub: &str, rg: &str) -> String {
    key(&crate::ids::group_id(sub, rg))
}

static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A number nobody has had before in this process. Every change to something that is saved to disk takes one (see
/// `persist.rs`): the highest number handed out is the service's `persist_generation`, and a scope (a resource group, or a
/// subscription for what lives outside groups) whose number differs from the one at the last save has changed since.
pub fn next_revision() -> u64 {
    REVISION.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

pub fn current_revision() -> u64 {
    REVISION.load(std::sync::atomic::Ordering::SeqCst)
}

/// The scope a resource key belongs to: its resource group (`/subscriptions/{s}/resourcegroups/{g}`), or its subscription.
pub fn scope_of(key: &str) -> &str {
    let mut at = 0;
    let mut slashes = 0;
    let wanted = if key.split('/').nth(3).map(|s| s.eq_ignore_ascii_case("resourcegroups")).unwrap_or(false) { 5 } else { 3 };
    for (i, c) in key.char_indices() {
        if c == '/' {
            slashes += 1;
            if slashes > wanted {
                break;
            }
            at = i;
        }
    }
    // `at` is the slash that ends the scope; a key shorter than the scope is its own scope
    if slashes >= wanted {
        &key[..at]
    } else {
        key
    }
}

/// The stored resources, with a revision per scope. Reads go through `Deref`; every change goes through a method that
/// takes a new revision for the scope it touches, so the next save knows which scopes to write again.
#[derive(Default)]
pub struct Resources {
    map: BTreeMap<String, Stored>,
    revs: HashMap<String, u64>,
}

impl std::ops::Deref for Resources {
    type Target = BTreeMap<String, Stored>;
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl Resources {
    fn touch(&mut self, key: &str) {
        self.revs.insert(scope_of(key).to_string(), next_revision());
    }

    pub fn insert(&mut self, key: String, s: Stored) -> Option<Stored> {
        self.touch(&key);
        self.map.insert(key, s)
    }

    pub fn remove(&mut self, key: &str) -> Option<Stored> {
        let old = self.map.remove(key);
        if old.is_some() {
            self.touch(key);
        }
        old
    }

    /// For changing a resource in place. Counts as a change whether or not the caller changes anything.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Stored> {
        if self.map.contains_key(key) {
            self.touch(key);
        }
        self.map.get_mut(key)
    }

    /// Store `s` unless there is a resource under `key` already.
    pub fn insert_if_absent(&mut self, key: String, s: Stored) {
        if !self.map.contains_key(&key) {
            self.insert(key, s);
        }
    }

    pub fn rev_of(&self, scope: &str) -> u64 {
        self.revs.get(scope).copied().unwrap_or(0)
    }
}

#[derive(Default)]
pub struct State {
    pub groups: BTreeMap<String, Group>,
    pub resources: Resources,
    /// Tags set through `.../providers/Microsoft.Resources/tags/default`, by lower-case scope id.
    pub scope_tags: HashMap<String, Map<String, Value>>,
    /// Subscription-wide tag names and their values.
    pub tag_names: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// Subscriptions that have been used.
    pub subs: Vec<String>,
    /// Writes into each group that are running (by group key), and the groups whose delete has started. See `writers.rs`.
    pub writers: HashMap<String, usize>,
    pub deleting: std::collections::HashSet<String>,
}

impl State {
    pub fn touch_sub(&mut self, sub: &str) {
        if !self.subs.iter().any(|s| s.eq_ignore_ascii_case(sub)) {
            self.subs.push(sub.to_string());
        }
    }

    pub fn group(&self, sub: &str, rg: &str) -> Option<&Group> {
        self.groups.get(&group_key(sub, rg))
    }

    /// Every resource of the group, deepest first (children before parents).
    pub fn in_group_deepest_first(&self, sub: &str, rg: &str) -> Vec<Stored> {
        let prefix = format!("{}/", group_key(sub, rg));
        let mut v: Vec<Stored> =
            self.resources.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).map(|(_, s)| s.clone()).collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.types.len()));
        v
    }

    /// Resources below `parent_id` (any depth), deepest first.
    pub fn descendants(&self, parent_id: &str) -> Vec<Stored> {
        let prefix = format!("{}/", key(parent_id));
        let mut v: Vec<Stored> =
            self.resources.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).map(|(_, s)| s.clone()).collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.types.len()));
        v
    }
}

#[cfg(test)]
mod scope_tests {
    use super::scope_of;

    #[test]
    fn a_resource_belongs_to_its_group_or_its_subscription() {
        assert_eq!(scope_of("/subscriptions/s/resourcegroups/g/providers/microsoft.x/y/z"), "/subscriptions/s/resourcegroups/g");
        assert_eq!(scope_of("/subscriptions/s/resourcegroups/g/y"), "/subscriptions/s/resourcegroups/g");
        assert_eq!(scope_of("/subscriptions/s/resourcegroups/g"), "/subscriptions/s/resourcegroups/g");
        assert_eq!(scope_of("/subscriptions/s/providers/microsoft.resources/deployments/d"), "/subscriptions/s");
        assert_eq!(scope_of("/subscriptions/s"), "/subscriptions/s");
        assert_eq!(scope_of("odd"), "odd");
    }
}
