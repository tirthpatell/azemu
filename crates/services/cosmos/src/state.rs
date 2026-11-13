//! Everything the service stores. One `State` for the whole service, keyed by account.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use azemu_core::etag;
use serde_json::{json, Map, Value};

use crate::errors::{CosmosError, R};
use crate::pk::{DocKey, PartitionKeyDef};
use crate::rid;

#[derive(Default)]
pub struct State {
    pub accounts: HashMap<String, Account>,
}

/// One Cosmos DB account (the `{account}` label of the host).
#[derive(Default)]
pub struct Account {
    pub databases: BTreeMap<String, Database>,
    pub offers: BTreeMap<String, Offer>,
    pub db_seq: u64,
    pub offer_seq: u64,
    pub seq: u64,
}

impl Account {
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Name of the database addressed by `key` (a name or a rid).
    pub fn db_name(&self, key: &str) -> Option<String> {
        if self.databases.contains_key(key) {
            return Some(key.to_string());
        }
        self.databases.values().find(|d| rid::matches(&d.meta.rid, key)).map(|d| d.meta.id.clone())
    }

    pub fn db(&mut self, key: &str) -> R<&mut Database> {
        let name = self.db_name(key).ok_or_else(CosmosError::not_found_default)?;
        self.databases.get_mut(&name).ok_or_else(CosmosError::not_found_default)
    }
}

/// Fields every resource has.
#[derive(Clone)]
pub struct Meta {
    pub id: String,
    pub rid: String,
    pub rid_bytes: Vec<u8>,
    pub seq: u64,
    pub ts: i64,
    pub etag: String,
}

impl Meta {
    pub fn new(id: &str, rid_bytes: Vec<u8>, seq: u64, ts: i64) -> Meta {
        Meta { id: id.to_string(), rid: rid::encode(&rid_bytes), rid_bytes, seq, ts, etag: etag::new_etag() }
    }

    pub fn touch(&mut self, ts: i64) {
        self.ts = ts;
        self.etag = etag::new_etag();
    }

    /// `{"id", <props>, "_rid", "_self", "_etag", <extras>, "_ts"}`.
    pub fn json(&self, props: &Map<String, Value>, link: &str, extras: &[(&str, &str)]) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), Value::String(self.id.clone()));
        for (k, v) in props {
            if k != "id" {
                m.insert(k.clone(), v.clone());
            }
        }
        m.insert("_rid".into(), Value::String(self.rid.clone()));
        m.insert("_self".into(), Value::String(link.to_string()));
        m.insert("_etag".into(), Value::String(self.etag.clone()));
        for (k, v) in extras {
            m.insert((*k).into(), Value::String((*v).into()));
        }
        m.insert("_ts".into(), json!(self.ts));
        Value::Object(m)
    }
}

/// A resource that is just a definition: user, permission, stored procedure, trigger, UDF.
pub struct Resource {
    pub meta: Meta,
    pub props: Map<String, Value>,
}

pub struct Database {
    pub meta: Meta,
    pub props: Map<String, Value>,
    pub containers: BTreeMap<String, Container>,
    pub users: BTreeMap<String, Resource>,
    /// Permissions by user id.
    pub permissions: BTreeMap<String, BTreeMap<String, Resource>>,
    pub coll_seq: u64,
    pub user_seq: u64,
    pub perm_seq: u64,
    /// Offer rid of the database throughput, when it has shared throughput.
    pub offer: Option<String>,
}

impl Database {
    pub fn link(&self) -> String {
        format!("dbs/{}/", self.meta.rid)
    }

    pub fn to_json(&self) -> Value {
        self.meta.json(&self.props, &self.link(), &[("_colls", "colls/"), ("_users", "users/")])
    }

    pub fn container_name(&self, key: &str) -> Option<String> {
        if self.containers.contains_key(key) {
            return Some(key.to_string());
        }
        self.containers.values().find(|c| rid::matches(&c.meta.rid, key)).map(|c| c.meta.id.clone())
    }

    pub fn container(&mut self, key: &str) -> R<&mut Container> {
        let name = self.container_name(key).ok_or_else(CosmosError::not_found_default)?;
        self.containers.get_mut(&name).ok_or_else(CosmosError::not_found_default)
    }
}

pub struct Offer {
    pub meta: Meta,
    /// `_self` of the resource the offer belongs to, and its `_rid`.
    pub resource: String,
    pub resource_rid: String,
    pub content: Value,
}

impl Offer {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("offerVersion".into(), json!("V2"));
        m.insert("offerType".into(), json!("Invalid"));
        m.insert("content".into(), self.content.clone());
        m.insert("resource".into(), json!(self.resource));
        m.insert("offerResourceId".into(), json!(self.resource_rid));
        m.insert("id".into(), json!(self.meta.rid));
        m.insert("_rid".into(), json!(self.meta.rid));
        m.insert("_self".into(), json!(format!("offers/{}/", self.meta.rid)));
        m.insert("_etag".into(), json!(self.meta.etag));
        m.insert("_ts".into(), json!(self.meta.ts));
        Value::Object(m)
    }
}

/// A stored document.
#[derive(Clone)]
pub struct Doc {
    /// Shared so that a query can work on a snapshot of the documents after the service lock is released.
    pub value: Arc<Value>,
    pub rid: String,
    pub epk: String,
    pub lsn: u64,
    pub ts: i64,
    pub etag: String,
}

pub struct Container {
    pub meta: Meta,
    pub props: Map<String, Value>,
    pub pk: PartitionKeyDef,
    pub unique_keys: Vec<Vec<String>>,
    pub default_ttl: Option<i64>,
    pub docs: BTreeMap<DocKey, Doc>,
    pub by_rid: HashMap<String, DocKey>,
    pub changes: BTreeMap<u64, DocKey>,
    pub lsn: u64,
    pub doc_seq: u64,
    pub child_seq: u64,
    pub sprocs: BTreeMap<String, Resource>,
    pub triggers: BTreeMap<String, Resource>,
    pub udfs: BTreeMap<String, Resource>,
    pub offer: Option<String>,
    /// Lower bound of the earliest expiry time of any document (0 = unknown, scan).
    pub min_expiry: i64,
    /// Started by the first all versions and deletes read.
    pub log: Option<ChangeLog>,
}

/// What happened to a document, for the "all versions and deletes" change feed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    Create,
    Replace,
    Delete,
}

/// One entry of the all versions and deletes change feed.
pub struct ChangeRecord {
    pub lsn: u64,
    pub key: DocKey,
    pub epk: String,
    pub kind: ChangeKind,
    /// Time of the change, unix seconds (`crts`).
    pub ts: i64,
    /// LSN of the version this change replaced or deleted.
    pub prev_lsn: Option<u64>,
    pub ttl_expired: bool,
    /// The version written (not for deletes). Shares the allocation with the stored document.
    pub current: Option<Arc<Value>>,
    pub bytes: usize,
}

/// Longest history kept: Azure keeps it for the continuous backup period (7 or 30 days);
/// here it is also bounded in size, so a loop of writes cannot fill the memory.
pub const LOG_MAX_AGE_SECS: i64 = 7 * 24 * 3600;
pub const LOG_MAX_RECORDS: usize = 100_000;
pub const LOG_MAX_BYTES: usize = 128 * 1024 * 1024;

/// The record limit in force (a variable so that a test can make the history short; see `set_change_log_record_limit`).
static LOG_RECORD_LIMIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(LOG_MAX_RECORDS);

pub fn set_record_limit(n: usize) {
    LOG_RECORD_LIMIT.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// History of every change since the first "all versions and deletes" read of the container
/// (which starts it, so containers nobody reads this way pay nothing).
pub struct ChangeLog {
    pub records: std::collections::VecDeque<ChangeRecord>,
    pub bytes: usize,
    /// Changes up to this LSN are not in the log (before it started, or dropped to stay in bounds).
    pub floor: u64,
}

impl ChangeLog {
    pub fn new(floor: u64) -> ChangeLog {
        ChangeLog { records: Default::default(), bytes: 0, floor }
    }

    fn push(&mut self, r: ChangeRecord, now: i64) {
        self.bytes += r.bytes;
        self.records.push_back(r);
        while let Some(front) = self.records.front() {
            let too_old = now - front.ts > LOG_MAX_AGE_SECS;
            let limit = LOG_RECORD_LIMIT.load(std::sync::atomic::Ordering::Relaxed);
            if !(too_old || self.records.len() > limit || self.bytes > LOG_MAX_BYTES) {
                break;
            }
            if let Some(old) = self.records.pop_front() {
                self.bytes -= old.bytes;
                self.floor = old.lsn;
            }
        }
    }

    /// Forget the changes after `lsn` (a batch that was undone).
    fn truncate_after(&mut self, lsn: u64) {
        while self.records.back().map(|r| r.lsn > lsn).unwrap_or(false) {
            if let Some(r) = self.records.pop_back() {
                self.bytes -= r.bytes;
            }
        }
    }
}

/// What a write changed, so a batch can undo it.
pub struct Undo {
    pub entries: Vec<(DocKey, Option<Doc>)>,
    pub lsn: u64,
    pub doc_seq: u64,
}

/// A copy of one logical partition for a script to work on (see `scripts.rs`): the script's reads and writes go to
/// `scratch`, and `expect` is what the partition looked like, so the writes can be applied only if nothing else changed it.
pub struct Snapshot {
    pub scratch: Container,
    /// Every document of the partition and its LSN at the time of the copy.
    pub expect: Vec<(DocKey, u64)>,
    pub pk: crate::pk::PkValue,
}

/// The partition changed while the script ran.
pub struct Conflict;

impl State {
    /// The container `coll` (its resource id must still be `rid`) of database `db`.
    pub fn container_by_rid(&mut self, account: &str, db: &str, coll: &str, rid: &str) -> Option<&mut Container> {
        let d = self.accounts.get_mut(account)?.databases.get_mut(db)?;
        d.containers.get_mut(coll).filter(|c| c.meta.rid == rid)
    }
}

impl Container {
    /// Copies the documents of the logical partition `pk` (the documents are shared, not copied). `reserve` document
    /// counters are set aside in this container for the documents the script may create, so their resource ids stay
    /// unique when the script's work is applied later.
    pub fn snapshot(&mut self, pk: &crate::pk::PkValue, reserve: u64) -> Snapshot {
        let mut docs = BTreeMap::new();
        let mut by_rid = HashMap::new();
        let mut changes = BTreeMap::new();
        let mut expect = Vec::new();
        for (k, d) in self.partition(pk) {
            if k.pk != *pk {
                continue;
            }
            by_rid.insert(d.rid.clone(), k.clone());
            changes.insert(d.lsn, k.clone());
            expect.push((k.clone(), d.lsn));
            docs.insert(k.clone(), d.clone());
        }
        let scratch = Container {
            meta: self.meta.clone(),
            props: self.props.clone(),
            pk: self.pk.clone(),
            unique_keys: self.unique_keys.clone(),
            default_ttl: self.default_ttl,
            docs,
            by_rid,
            changes,
            lsn: self.lsn,
            doc_seq: self.doc_seq,
            child_seq: 0,
            sprocs: BTreeMap::new(),
            triggers: BTreeMap::new(),
            udfs: BTreeMap::new(),
            offer: None,
            min_expiry: self.min_expiry,
            log: None,
        };
        self.doc_seq = self.doc_seq.saturating_add(reserve);
        Snapshot { scratch, expect, pk: pk.clone() }
    }

    /// Applies what a script did to its copy of the partition, if the partition is as it was when the copy was made.
    /// Returns the container's LSN after the writes.
    pub fn apply_snapshot(&mut self, snap: &Snapshot, now: i64) -> Result<u64, Conflict> {
        let mut now_docs: Vec<(&DocKey, u64)> = Vec::new();
        for (k, d) in self.partition(&snap.pk) {
            if k.pk == snap.pk {
                now_docs.push((k, d.lsn));
            }
        }
        if now_docs.len() != snap.expect.len() || now_docs.iter().zip(&snap.expect).any(|((k, l), (ek, el))| *k != ek || l != el) {
            return Err(Conflict);
        }
        let before: BTreeMap<&DocKey, u64> = snap.expect.iter().map(|(k, l)| (k, *l)).collect();
        let mut undo = self.begin();
        for (k, _) in &snap.expect {
            if !snap.scratch.docs.contains_key(k) {
                self.delete(k, &mut undo, now);
            }
        }
        let mut puts: Vec<(&DocKey, &Doc)> = snap.scratch.docs.iter().filter(|(k, d)| before.get(k) != Some(&d.lsn)).collect();
        puts.sort_by_key(|(_, d)| d.lsn);
        for (k, d) in puts {
            self.put(k.clone(), d.clone(), &mut undo);
        }
        Ok(self.lsn)
    }

    pub fn link(&self, db_rid: &str) -> String {
        format!("dbs/{}/colls/{}/", db_rid, self.meta.rid)
    }

    pub fn to_json(&self, db_rid: &str) -> Value {
        self.meta.json(
            &self.props,
            &self.link(db_rid),
            &[("_docs", "docs/"), ("_sprocs", "sprocs/"), ("_triggers", "triggers/"), ("_udfs", "udfs/"), ("_conflicts", "conflicts/")],
        )
    }

    pub fn begin(&self) -> Undo {
        Undo { entries: Vec::new(), lsn: self.lsn, doc_seq: self.doc_seq }
    }

    pub fn rollback(&mut self, undo: Undo) {
        for (key, old) in undo.entries.into_iter().rev() {
            self.remove_raw(&key);
            if let Some(d) = old {
                self.insert_raw(key, d);
            }
        }
        self.lsn = undo.lsn;
        self.doc_seq = undo.doc_seq;
        if let Some(l) = &mut self.log {
            l.truncate_after(undo.lsn);
        }
    }

    fn log_change(&mut self, key: &DocKey, doc: Option<&Doc>, old: Option<&Doc>, ts: i64, ttl_expired: bool) {
        let lsn = self.lsn;
        let Some(log) = &mut self.log else { return };
        let (kind, epk, current) = match (doc, old) {
            (Some(d), Some(_)) => (ChangeKind::Replace, d.epk.clone(), Some(d.value.clone())),
            (Some(d), None) => (ChangeKind::Create, d.epk.clone(), Some(d.value.clone())),
            (None, Some(o)) => (ChangeKind::Delete, o.epk.clone(), None),
            (None, None) => return,
        };
        let bytes = 200 + key.id.len() + current.as_ref().map(|v| crate::paging::approx_size(v)).unwrap_or(0);
        log.push(ChangeRecord { lsn, key: key.clone(), epk, kind, ts, prev_lsn: old.map(|o| o.lsn), ttl_expired, current, bytes }, ts);
    }

    fn remove_raw(&mut self, key: &DocKey) -> Option<Doc> {
        let old = self.docs.remove(key)?;
        self.by_rid.remove(&old.rid);
        self.changes.remove(&old.lsn);
        Some(old)
    }

    fn insert_raw(&mut self, key: DocKey, doc: Doc) {
        self.by_rid.insert(doc.rid.clone(), key.clone());
        self.changes.insert(doc.lsn, key.clone());
        self.docs.insert(key, doc);
    }

    /// Store `doc` under `key` (replacing any document there); bumps the LSN.
    pub fn put(&mut self, key: DocKey, mut doc: Doc, undo: &mut Undo) {
        self.lsn += 1;
        doc.lsn = self.lsn;
        if let Some(ttl_at) = self.expiry_of(&doc) {
            if self.min_expiry == 0 || ttl_at < self.min_expiry {
                self.min_expiry = ttl_at;
            }
        }
        let old = self.remove_raw(&key);
        undo.entries.push((key.clone(), old.clone()));
        self.log_change(&key, Some(&doc), old.as_ref(), doc.ts, false);
        self.insert_raw(key, doc);
    }

    /// Remove the document at `key`; bumps the LSN so session tokens advance.
    pub fn delete(&mut self, key: &DocKey, undo: &mut Undo, now: i64) -> Option<Doc> {
        let old = self.remove_raw(key)?;
        self.lsn += 1;
        self.log_change(key, None, Some(&old), now, false);
        undo.entries.push((key.clone(), Some(old.clone())));
        Some(old)
    }

    pub fn next_doc_rid(&mut self) -> (Vec<u8>, String) {
        let (bytes, used) = rid::child(&self.meta.rid_bytes, self.doc_seq + 1);
        self.doc_seq = used;
        let s = rid::encode(&bytes);
        (bytes, s)
    }

    /// Time (unix seconds) at which `doc` expires, or None.
    pub fn expiry_of(&self, doc: &Doc) -> Option<i64> {
        let default = self.default_ttl?;
        let ttl = match doc.value.get("ttl") {
            Some(v) => v.as_i64().unwrap_or(default),
            None => default,
        };
        if ttl > 0 {
            Some(doc.ts + ttl)
        } else {
            None
        }
    }

    /// Remove documents whose time to live has passed. Cheap when nothing can have expired.
    pub fn purge_expired(&mut self, now: i64) {
        if self.default_ttl.is_none() || (self.min_expiry != 0 && now < self.min_expiry) {
            return;
        }
        let mut min = 0i64;
        let mut dead = Vec::new();
        for (k, d) in &self.docs {
            if let Some(at) = self.expiry_of(d) {
                if at <= now {
                    dead.push(k.clone());
                } else if min == 0 || at < min {
                    min = at;
                }
            }
        }
        for k in dead {
            if let Some(old) = self.remove_raw(&k) {
                if self.log.is_some() {
                    self.lsn += 1;
                    self.log_change(&k, None, Some(&old), now, true);
                }
            }
        }
        // nothing left that can expire: next possible expiry is far away
        self.min_expiry = if min == 0 { i64::MAX } else { min };
    }

    /// Documents of one logical partition (or all partitions whose key starts with
    /// `prefix` for hierarchical keys).
    pub fn partition<'a>(&'a self, prefix: &'a [crate::pk::PkComp]) -> impl Iterator<Item = (&'a DocKey, &'a Doc)> + 'a {
        let start = DocKey { pk: prefix.to_vec(), id: String::new() };
        self.docs.range(start..).take_while(move |(k, _)| k.pk.len() >= prefix.len() && k.pk[..prefix.len()] == *prefix)
    }

    /// Unique key check for a document about to be stored under `key`.
    pub fn check_unique(&self, key: &DocKey, value: &Value) -> R<()> {
        for paths in &self.unique_keys {
            let mine = unique_tuple(paths, value);
            for (k, d) in self.partition(&key.pk) {
                if k.pk != key.pk || k.id == key.id {
                    continue;
                }
                if unique_tuple(paths, &d.value) == mine {
                    return Err(CosmosError::conflict());
                }
            }
        }
        Ok(())
    }
}

fn unique_tuple(paths: &[String], doc: &Value) -> Vec<Option<Value>> {
    paths
        .iter()
        .map(|p| {
            let mut cur = doc;
            for seg in p.trim_start_matches('/').split('/') {
                match cur.get(seg) {
                    Some(v) => cur = v,
                    None => return None,
                }
            }
            Some(cur.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(lsn: u64, ts: i64, bytes: usize) -> ChangeRecord {
        ChangeRecord {
            lsn,
            key: DocKey { pk: vec![], id: lsn.to_string() },
            epk: String::new(),
            kind: ChangeKind::Create,
            ts,
            prev_lsn: None,
            ttl_expired: false,
            current: None,
            bytes,
        }
    }

    #[test]
    fn change_log_is_bounded_by_count_size_and_age() {
        let mut l = ChangeLog::new(0);
        for i in 1..=(LOG_MAX_RECORDS as u64 + 10) {
            l.push(record(i, 1000, 10), 1000);
        }
        assert_eq!(l.records.len(), LOG_MAX_RECORDS);
        assert_eq!(l.floor, 10, "what was dropped is reported through the floor");

        let mut l = ChangeLog::new(0);
        for i in 1..=5u64 {
            l.push(record(i, 1000, LOG_MAX_BYTES / 2), 1000);
        }
        assert_eq!(l.records.len(), 2);
        assert!(l.bytes <= LOG_MAX_BYTES);

        let mut l = ChangeLog::new(0);
        l.push(record(1, 1000, 10), 1000);
        l.push(record(2, 1000 + LOG_MAX_AGE_SECS + 1, 10), 1000 + LOG_MAX_AGE_SECS + 1);
        assert_eq!(l.records.len(), 1);
        assert_eq!(l.floor, 1);

        l.truncate_after(1);
        assert!(l.records.is_empty());
        assert_eq!(l.bytes, 0);
    }
}
