//! Everything the service remembers. One `Store` per `{store}.azconfig.localhost` host,
//! created on first use.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ops::Bound;
use std::sync::Arc;

use azemu_core::etag;
use azemu_core::time::OffsetDateTime;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::snapshots::Snapshot;
use crate::timefmt::trunc;

/// One key-value as stored: the current state of a (key, label) pair or one revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Setting {
    pub key: String,
    /// `None` is the null label, a different setting from any named label.
    pub label: Option<String>,
    pub value: Option<String>,
    pub content_type: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<(String, String)>,
    pub locked: bool,
    /// Bare etag, no quotes. A new one on every write, lock and unlock.
    pub etag: String,
    pub last_modified: OffsetDateTime,
}

/// A version of a setting in the history. `ended` is when it was replaced or deleted. The setting is shared
/// (`Arc`) with the live entry and with every snapshot that picked it, so a copy costs a pointer.
#[derive(Clone, Debug)]
pub struct Rev {
    /// Position in the store's write order. Orders revisions of the same instant.
    pub seq: u64,
    pub setting: Arc<Setting>,
    pub ended: Option<OffsetDateTime>,
}

pub type Ident = (String, Option<String>);

/// What a store keeps, per store. Azure: revisions are kept 7 days (Free, Developer) or 30 days (Standard, Premium);
/// storage per store is 10 MB (Free), 500 MB (Developer), 1 GB (Standard) or 4 GB (Premium), with the same amount again for
/// snapshots; a snapshot is at most 1 MB. The defaults here are smaller than the paid tiers because the emulator holds
/// everything in memory (settings: `AZEMU_APPCONFIG_*`, see `docs/services/appconfig.md`).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Revisions older than this many seconds (counted from the revision's own time) are dropped.
    pub revision_age_secs: i64,
    /// Most revisions kept per (key, label), the current one included.
    pub revisions_per_key: usize,
    /// Bytes of all settings and their history in one store. The oldest revisions go first; a write that would take the
    /// current settings alone over it is answered 429, as Azure does when the storage quota is exceeded.
    pub store_bytes: usize,
    /// Bytes of all snapshots (active and archived) in one store.
    pub snapshot_bytes: usize,
    /// Most snapshots in one store.
    pub snapshots: usize,
    /// Largest single snapshot (Azure: 1 MB).
    pub snapshot_max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            revision_age_secs: 30 * 86_400,
            revisions_per_key: 1000,
            store_bytes: 100 << 20,
            snapshot_bytes: 50 << 20,
            snapshots: 10_000,
            snapshot_max_bytes: 1_000_000,
        }
    }
}

impl Limits {
    /// The defaults, changed by `AZEMU_APPCONFIG_REVISION_DAYS`, `_REVISIONS_PER_KEY`, `_STORE_MB`, `_SNAPSHOT_MB`, `_SNAPSHOTS`
    /// (read through `azemu_core::settings`, and described there for `docs/SETTINGS.md`).
    pub fn from_env() -> Limits {
        use azemu_core::settings;
        settings::describe(
            "AZEMU_APPCONFIG_REVISION_DAYS",
            "30",
            "days a revision of a setting is kept (counted from the revision's own time)",
        );
        settings::describe(
            "AZEMU_APPCONFIG_REVISIONS_PER_KEY",
            "1000",
            "most revisions kept for one key and label, the current one included",
        );
        settings::describe("AZEMU_APPCONFIG_STORE_MB", "100", "megabytes of settings and history in one store; a write over it gets 429");
        settings::describe("AZEMU_APPCONFIG_SNAPSHOT_MB", "50", "megabytes of snapshots (active and archived) in one store");
        settings::describe("AZEMU_APPCONFIG_SNAPSHOTS", "10000", "most snapshots in one store");
        let d = Limits::default();
        let days = settings::number("AZEMU_APPCONFIG_REVISION_DAYS", (d.revision_age_secs / 86_400) as u64);
        let mb = |name: &str, default: usize| (settings::number(name, (default >> 20) as u64) as usize).saturating_mul(1 << 20);
        Limits {
            revision_age_secs: (days.min(36_500) as i64) * 86_400,
            revisions_per_key: (settings::number("AZEMU_APPCONFIG_REVISIONS_PER_KEY", d.revisions_per_key as u64) as usize).max(1),
            store_bytes: mb("AZEMU_APPCONFIG_STORE_MB", d.store_bytes),
            snapshot_bytes: mb("AZEMU_APPCONFIG_SNAPSHOT_MB", d.snapshot_bytes),
            snapshots: settings::number("AZEMU_APPCONFIG_SNAPSHOTS", d.snapshots as u64) as usize,
            ..d
        }
    }
}

/// A write was refused because the current settings alone would not fit the store's quota.
#[derive(Debug, PartialEq, Eq)]
pub struct QuotaExceeded;

/// Bytes a setting counts against the store's quota: its text, its etag and a fixed amount for what the entry itself takes.
pub fn setting_bytes(s: &Setting) -> usize {
    const FIXED: usize = 192;
    FIXED
        + s.key.len()
        + s.label.as_deref().map_or(0, str::len)
        + s.value.as_deref().map_or(0, str::len)
        + s.content_type.as_deref().map_or(0, str::len)
        + s.description.as_deref().map_or(0, str::len)
        + s.etag.len()
        + s.tags.iter().map(|(k, v)| k.len() + v.len() + 48).sum::<usize>()
}

#[derive(Default)]
pub struct Store {
    pub limits: Limits,
    next_seq: u64,
    /// Every kept version of every setting, by write order.
    revs: BTreeMap<u64, Rev>,
    /// (key, label) of each existing setting -> (seq of its current revision, the setting). The map order is the
    /// listing order: by key, the null label first.
    live: BTreeMap<Ident, (u64, Arc<Setting>)>,
    /// Seqs of the kept revisions of each (key, label), oldest first; the current one is last.
    by_ident: HashMap<Ident, VecDeque<u64>>,
    /// Revisions that were replaced or deleted (the ones retention may drop): by seq, and by (their time in seconds, seq).
    ended: BTreeSet<u64>,
    ended_by_time: BTreeSet<(i64, u64)>,
    /// Bytes of every revision in `revs`, and of the current ones.
    bytes: usize,
    live_bytes: usize,
    /// Labels of the existing settings with how many settings carry each (what `/labels` lists).
    labels: BTreeMap<Option<String>, usize>,
    pub snapshots: BTreeMap<String, Snapshot>,
}

/// What a store writes to disk (`persist.rs`): the revisions and which of them are current, the snapshots. The
/// indexes (by key, by age, labels, byte counts) are worked out again when it is read.
#[derive(Serialize, Deserialize)]
pub struct SavedStore {
    pub next_seq: u64,
    pub revs: Vec<SavedRev>,
    /// (key, label, seq of the current revision) of every existing setting.
    pub live: Vec<(String, Option<String>, u64)>,
    pub snapshots: BTreeMap<String, Snapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedRev {
    pub seq: u64,
    pub setting: Arc<Setting>,
    pub ended: Option<OffsetDateTime>,
}

/// Fields of a write; everything else is filled in by the store.
pub struct NewSetting {
    pub key: String,
    pub label: Option<String>,
    pub value: Option<String>,
    pub content_type: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<(String, String)>,
    pub locked: bool,
}

impl Store {
    pub fn new(limits: Limits) -> Store {
        Store { limits, ..Store::default() }
    }

    pub fn live_get(&self, key: &str, label: Option<&str>) -> Option<&Setting> {
        let id = (key.to_string(), label.map(str::to_string));
        self.live.get(&id).map(|(_, s)| &**s)
    }

    /// The existing settings in listing order after `after` (exclusive), without building a list: a page stops reading
    /// when it is full.
    pub fn live_after(&self, after: Option<&Ident>) -> impl Iterator<Item = &Setting> {
        let lower = match after {
            Some(c) => Bound::Excluded(c.clone()),
            None => Bound::Unbounded,
        };
        self.live.range((lower, Bound::Unbounded)).map(|(_, (_, s))| &**s)
    }

    /// The existing settings, shared (snapshots keep these).
    pub fn live_arcs(&self) -> impl Iterator<Item = &Arc<Setting>> {
        self.live.values().map(|(_, s)| s)
    }

    /// The labels of the existing settings after `after` (exclusive), in order, the null label first.
    pub fn labels_after(&self, after: Option<&Option<String>>) -> impl Iterator<Item = &Option<String>> {
        let lower = match after {
            Some(c) => Bound::Excluded(c.clone()),
            None => Bound::Unbounded,
        };
        self.labels.range((lower, Bound::Unbounded)).map(|(l, _)| l)
    }

    /// Revisions with a seq below `before` (all when `None`), newest first.
    pub fn revs_before(&self, before: Option<u64>) -> impl Iterator<Item = &Rev> {
        let upper = match before {
            Some(b) => Bound::Excluded(b),
            None => Bound::Unbounded,
        };
        self.revs.range((Bound::Unbounded, upper)).rev().map(|(_, r)| r)
    }

    #[cfg(test)]
    fn rev_count(&self) -> usize {
        self.revs.len()
    }

    /// The settings as they were at `t` (second precision, like the service), in listing order. Reads the whole history
    /// (bounded by the store quota).
    pub fn at(&self, t: OffsetDateTime) -> Vec<&Setting> {
        let t = trunc(t);
        let mut found: BTreeMap<(&str, Option<&str>), &Setting> = BTreeMap::new();
        for r in self.revs.values() {
            let s = &*r.setting;
            if trunc(s.last_modified) > t {
                continue;
            }
            if let Some(e) = r.ended {
                if trunc(e) <= t {
                    continue;
                }
            }
            found.insert((s.key.as_str(), s.label.as_deref()), s);
        }
        found.into_values().collect()
    }

    /// The setting at `t`, if one existed then (and its revision was kept).
    pub fn get_at(&self, key: &str, label: Option<&str>, t: OffsetDateTime) -> Option<&Setting> {
        let t = trunc(t);
        let id = (key.to_string(), label.map(str::to_string));
        self.by_ident
            .get(&id)?
            .iter()
            .filter_map(|seq| self.revs.get(seq))
            .filter(|r| trunc(r.setting.last_modified) <= t)
            .filter(|r| r.ended.map(|e| trunc(e) > t).unwrap_or(true))
            .map(|r| &*r.setting)
            .next_back()
    }

    /// Store a new version of a setting (create or overwrite) and return it. `QuotaExceeded` when the current settings
    /// alone (history is dropped to make room) would be over the store's byte limit; nothing changes then.
    pub fn write(&mut self, n: NewSetting, now: OffsetDateTime) -> Result<Arc<Setting>, QuotaExceeded> {
        let id: Ident = (n.key.clone(), n.label.clone());
        let seq = self.take_seq();
        let setting = Arc::new(Setting {
            key: n.key,
            label: n.label,
            value: n.value,
            content_type: n.content_type,
            description: n.description,
            tags: n.tags,
            locked: n.locked,
            etag: new_etag(seq),
            last_modified: now,
        });
        let size = setting_bytes(&setting);
        let old_size = self.live.get(&id).map_or(0, |(_, s)| setting_bytes(s));
        if self.live_bytes - old_size + size > self.limits.store_bytes {
            return Err(QuotaExceeded);
        }
        if let Some((old_seq, old)) = self.live.remove(&id) {
            self.end_rev(old_seq, &old, now);
            self.live_bytes -= old_size;
            self.drop_label(&id.1);
        }
        self.revs.insert(seq, Rev { seq, setting: setting.clone(), ended: None });
        self.by_ident.entry(id.clone()).or_default().push_back(seq);
        *self.labels.entry(id.1.clone()).or_insert(0) += 1;
        self.live.insert(id.clone(), (seq, setting.clone()));
        self.bytes += size;
        self.live_bytes += size;
        // too many revisions of this key: the oldest go
        while self.by_ident.get(&id).is_some_and(|q| q.len() > self.limits.revisions_per_key) {
            let oldest = self.by_ident.get(&id).and_then(|q| q.front().copied());
            match oldest {
                Some(o) => self.remove_rev(o),
                None => break,
            }
        }
        self.enforce(now);
        Ok(setting)
    }

    /// Remove a setting from the live set; its revisions stay (until retention drops them).
    pub fn delete(&mut self, key: &str, label: Option<&str>, now: OffsetDateTime) -> Option<Arc<Setting>> {
        let id: Ident = (key.to_string(), label.map(str::to_string));
        let (seq, old) = self.live.remove(&id)?;
        self.live_bytes -= setting_bytes(&old);
        self.drop_label(&id.1);
        self.end_rev(seq, &old, now);
        self.enforce(now);
        Some(old)
    }

    fn drop_label(&mut self, label: &Option<String>) {
        if let Some(n) = self.labels.get_mut(label) {
            *n -= 1;
            if *n == 0 {
                self.labels.remove(label);
            }
        }
    }

    /// Mark a revision as replaced or deleted: from now on retention may drop it.
    fn end_rev(&mut self, seq: u64, setting: &Setting, now: OffsetDateTime) {
        if let Some(r) = self.revs.get_mut(&seq) {
            r.ended = Some(now);
            self.ended.insert(seq);
            self.ended_by_time.insert((setting.last_modified.unix_timestamp(), seq));
        }
    }

    /// Drop a revision (never a current one) from every index.
    fn remove_rev(&mut self, seq: u64) {
        let Some(r) = self.revs.remove(&seq) else { return };
        self.bytes -= setting_bytes(&r.setting);
        self.ended.remove(&seq);
        self.ended_by_time.remove(&(r.setting.last_modified.unix_timestamp(), seq));
        let id: Ident = (r.setting.key.clone(), r.setting.label.clone());
        if let Some(q) = self.by_ident.get_mut(&id) {
            if q.front() == Some(&seq) {
                q.pop_front();
            } else {
                q.retain(|s| *s != seq);
            }
            if q.is_empty() {
                self.by_ident.remove(&id);
            }
        }
    }

    /// Retention: drop revisions older than the age limit, then the oldest ones while the store is over its byte limit.
    /// Called after every write and at the start of every request, so it costs what it removes.
    pub fn enforce(&mut self, now: OffsetDateTime) {
        let cutoff = now.unix_timestamp().saturating_sub(self.limits.revision_age_secs);
        while let Some(&(t, seq)) = self.ended_by_time.first() {
            if t >= cutoff {
                break;
            }
            self.remove_rev(seq);
        }
        while self.bytes > self.limits.store_bytes {
            match self.ended.first().copied() {
                Some(seq) => self.remove_rev(seq),
                None => break,
            }
        }
    }

    /// What `persist.rs` writes. Cheap: the settings are shared, not copied.
    pub fn to_saved(&self) -> SavedStore {
        SavedStore {
            next_seq: self.next_seq,
            revs: self.revs.values().map(|r| SavedRev { seq: r.seq, setting: r.setting.clone(), ended: r.ended }).collect(),
            live: self.live.iter().map(|((k, l), (seq, _))| (k.clone(), l.clone(), *seq)).collect(),
            snapshots: self.snapshots.clone(),
        }
    }

    /// A store made from what `to_saved` produced, with the given limits. `Err` says what does not fit together.
    pub fn from_saved(limits: Limits, saved: SavedStore) -> Result<Store, String> {
        let mut s = Store::new(limits);
        s.next_seq = saved.next_seq;
        let mut revs = saved.revs;
        revs.sort_by_key(|r| r.seq);
        for r in revs {
            if r.seq == 0 || r.seq > saved.next_seq {
                return Err(format!("revision number {} is outside 1..={}", r.seq, saved.next_seq));
            }
            let id: Ident = (r.setting.key.clone(), r.setting.label.clone());
            if s.revs.contains_key(&r.seq) {
                return Err(format!("revision number {} appears twice", r.seq));
            }
            s.bytes += setting_bytes(&r.setting);
            s.by_ident.entry(id).or_default().push_back(r.seq);
            if r.ended.is_some() {
                s.ended.insert(r.seq);
                s.ended_by_time.insert((r.setting.last_modified.unix_timestamp(), r.seq));
            }
            s.revs.insert(r.seq, Rev { seq: r.seq, setting: r.setting, ended: r.ended });
        }
        // by_ident lists revisions oldest first, which is by number (they were pushed in map order)
        for (key, label, seq) in saved.live {
            let rev = s.revs.get(&seq).ok_or_else(|| format!("the current revision {seq} of '{key}' is missing"))?;
            if rev.setting.key != key || rev.setting.label != label || rev.ended.is_some() {
                return Err(format!("the current revision {seq} of '{key}' does not match it"));
            }
            let setting = rev.setting.clone();
            s.live_bytes += setting_bytes(&setting);
            *s.labels.entry(label.clone()).or_insert(0) += 1;
            if s.live.insert((key.clone(), label), (seq, setting)).is_some() {
                return Err(format!("'{key}' has two current revisions"));
            }
        }
        // a snapshot shares the settings it holds with the live ones and with other snapshots
        let mut known: HashMap<String, Arc<Setting>> = s.revs.values().map(|r| (r.setting.etag.clone(), r.setting.clone())).collect();
        for (name, snap) in saved.snapshots {
            let mut snap = snap;
            for item in snap.items.iter_mut() {
                match known.get(&item.etag) {
                    Some(shared) if **shared == **item => *item = shared.clone(),
                    Some(_) => {}
                    None => {
                        known.insert(item.etag.clone(), item.clone());
                    }
                }
            }
            s.snapshots.insert(name, snap);
        }
        Ok(s)
    }

    fn take_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// A fresh etag for things that are not key-values (snapshots).
    pub fn fresh_etag(&mut self) -> String {
        let seq = self.take_seq();
        new_etag(seq)
    }
}

/// 43 characters of base64url, like the real service's etags. Unique per call.
pub fn new_etag(seq: u64) -> String {
    let unique = etag::new_etag();
    token(&format!("{unique}#{seq}"))
}

/// base64url (no padding) of the SHA-256 of `seed`: 43 characters.
pub fn token(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    URL_SAFE_NO_PAD.encode(digest.as_slice())
}

/// Every store of the service, by store name.
#[derive(Default)]
pub struct State {
    pub limits: Limits,
    pub stores: HashMap<String, Store>,
}

impl State {
    pub fn new(limits: Limits) -> State {
        State { limits, stores: HashMap::new() }
    }

    pub fn store(&mut self, name: &str) -> &mut Store {
        let limits = self.limits;
        self.stores.entry(name.to_string()).or_insert_with(|| Store::new(limits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_core::time::Duration;

    fn new(key: &str, label: Option<&str>, value: &str) -> NewSetting {
        NewSetting {
            key: key.into(),
            label: label.map(Into::into),
            value: Some(value.into()),
            content_type: None,
            description: None,
            tags: vec![],
            locked: false,
        }
    }

    #[test]
    fn etags_are_43_chars_and_unique() {
        let a = new_etag(1);
        let b = new_etag(1);
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
    }

    #[test]
    fn point_in_time_reads_follow_the_history() {
        let mut s = Store::default();
        let t0 = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        s.write(new("a", None, "old"), t0).unwrap();
        s.write(new("b", None, "b1"), t0).unwrap();
        let t1 = t0 + Duration::seconds(10);
        s.write(new("a", None, "new"), t1).unwrap();
        s.delete("b", None, t1);
        let t2 = t0 + Duration::seconds(5);
        let at: Vec<_> = s.at(t2).iter().map(|x| (x.key.clone(), x.value.clone().unwrap())).collect();
        assert_eq!(at, vec![("a".to_string(), "old".to_string()), ("b".to_string(), "b1".to_string())]);
        let now: Vec<_> = s.at(t1 + Duration::seconds(1)).iter().map(|x| x.value.clone().unwrap()).collect();
        assert_eq!(now, vec!["new".to_string()]);
        assert!(s.at(t0 - Duration::seconds(1)).is_empty());
        assert_eq!(s.get_at("a", None, t2).unwrap().value.as_deref(), Some("old"));
        assert!(s.get_at("b", None, t1).is_none());
    }

    #[test]
    fn same_instant_writes_keep_their_order() {
        let mut s = Store::default();
        let t = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        for v in ["1", "2", "3"] {
            s.write(new("k", None, v), t).unwrap();
        }
        let seqs: Vec<u64> = s.revs.values().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(s.live_get("k", None).unwrap().value.as_deref(), Some("3"));
        // the same instant: the latest write is the one a point-in-time read sees
        assert_eq!(s.at(t)[0].value.as_deref(), Some("3"));
    }

    #[test]
    fn null_label_sorts_first() {
        let mut s = Store::default();
        let t = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        s.write(new("k", Some("b"), "x"), t).unwrap();
        s.write(new("k", None, "x"), t).unwrap();
        s.write(new("k", Some("a"), "x"), t).unwrap();
        let labels: Vec<_> = s.live_after(None).map(|x| x.label.clone()).collect();
        assert_eq!(labels, vec![None, Some("a".to_string()), Some("b".to_string())]);
    }

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + secs).unwrap()
    }

    #[test]
    fn revisions_older_than_the_age_limit_are_dropped_but_never_the_current_setting() {
        let mut s = Store::new(Limits { revision_age_secs: 100, ..Limits::default() });
        s.write(new("a", None, "1"), at(0)).unwrap();
        s.write(new("a", None, "2"), at(50)).unwrap();
        s.write(new("b", None, "old but current"), at(0)).unwrap();
        assert_eq!(s.rev_count(), 3);
        s.enforce(at(120)); // the first revision of a is 120 s old
        assert_eq!(s.rev_count(), 2);
        s.enforce(at(10_000)); // a:2 is current and stays; b is current
        assert_eq!(s.rev_count(), 2);
        assert_eq!(s.live_get("a", None).unwrap().value.as_deref(), Some("2"));
        // a deleted setting's last revision goes when it is old, too
        s.delete("a", None, at(10_001));
        s.enforce(at(20_000));
        assert_eq!(s.rev_count(), 1);
    }

    #[test]
    fn a_key_keeps_at_most_the_configured_number_of_revisions() {
        let mut s = Store::new(Limits { revisions_per_key: 3, ..Limits::default() });
        for i in 0..50 {
            s.write(new("k", None, &i.to_string()), at(i)).unwrap();
        }
        assert_eq!(s.rev_count(), 3);
        let kept: Vec<_> = s.revs_before(None).map(|r| r.setting.value.clone().unwrap()).collect();
        assert_eq!(kept, ["49", "48", "47"]);
        assert_eq!(s.get_at("k", None, at(48)).unwrap().value.as_deref(), Some("48"));
    }

    #[test]
    fn history_is_trimmed_by_bytes_oldest_first_and_current_settings_that_do_not_fit_are_refused() {
        let big = "x".repeat(10_000);
        let mut s = Store::new(Limits { store_bytes: 100_000, ..Limits::default() });
        for i in 0..200 {
            s.write(new("k", None, &big), at(i)).unwrap();
        }
        assert!(s.bytes <= 100_000, "{} bytes kept", s.bytes);
        assert!(s.rev_count() >= 5 && s.rev_count() <= 10, "{}", s.rev_count());
        assert_eq!(s.revs_before(None).next().unwrap().seq, 200, "the newest revision is kept");
        // current settings alone: filling the store with different keys
        let mut s = Store::new(Limits { store_bytes: 100_000, ..Limits::default() });
        let mut refused = 0;
        for i in 0..20 {
            if s.write(new(&format!("k{i}"), None, &big), at(i)).is_err() {
                refused += 1;
            }
        }
        assert!(refused >= 9, "{refused} refused");
        assert!(s.bytes <= 100_000);
        // an overwrite of an existing key still fits (it replaces its own bytes)
        assert!(s.write(new("k0", None, &big), at(100)).is_ok());
    }

    #[test]
    fn labels_are_listed_from_their_own_index() {
        let mut s = Store::default();
        s.write(new("a", Some("x"), "1"), at(0)).unwrap();
        s.write(new("b", Some("x"), "1"), at(0)).unwrap();
        s.write(new("b", None, "1"), at(0)).unwrap();
        let l: Vec<_> = s.labels_after(None).cloned().collect();
        assert_eq!(l, vec![None, Some("x".to_string())]);
        s.delete("a", Some("x"), at(1));
        s.delete("b", Some("x"), at(1));
        let l: Vec<_> = s.labels_after(None).cloned().collect();
        assert_eq!(l, vec![None]);
    }
}
