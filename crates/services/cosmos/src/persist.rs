//! Saving the state to disk and reading it back (`--data-dir`). The format and the reasons are in
//! `docs/services/persistence.md` (section "Cosmos").
//!
//! Layout of the service directory:
//!
//! * `cosmos.state` (kind `cosmos`): every account with its databases, containers (partition key, indexing
//!   and other policies, unique keys, TTL), users, permissions, stored procedures, triggers, UDFs, offers and
//!   the counters that make new resource ids. Small; written again only when its content changed.
//! * `docs-<16 hex digits>.state` (kind `cosmos-docs`): one file per container with its documents and the
//!   container's LSN and document counter, and the "all versions and deletes" history when it was started.
//!   Written again only when the container changed since the last save.
//!
//! A save copies what it writes under the service lock (documents are `Arc`s, so a copy is a pointer and four
//! small strings per document) and encodes and writes after the lock is released. Container files are written
//! before `cosmos.state`, and a file nothing refers to any more is removed last, so a crash at any point leaves
//! a save that loads (at worst a container with fewer documents than `cosmos.state` knows about, or a document
//! file nobody asks for).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::Path;
use std::sync::Arc;

use azemu_core::persist;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::pk::{DocKey, PartitionKeyDef, PkComp};
use crate::state::{Account, ChangeKind, ChangeLog, ChangeRecord, Container, Database, Doc, Meta, Offer, Resource, State};
use crate::CosmosService;

pub const DEFS_FILE: &str = "cosmos.state";
pub const DEFS_KIND: &str = "cosmos";
pub const DOCS_KIND: &str = "cosmos-docs";
pub const VERSION: u32 = 1;

// ---- the shapes in the files --------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct MetaD {
    id: String,
    rid: String,
    rid_bytes: Vec<u8>,
    seq: u64,
    ts: i64,
    etag: String,
}

#[derive(Serialize, Deserialize)]
struct ResD {
    key: String,
    meta: MetaD,
    props: Map<String, Value>,
}

#[derive(Serialize, Deserialize)]
struct OfferD {
    key: String,
    meta: MetaD,
    resource: String,
    resource_rid: String,
    content: Value,
}

#[derive(Serialize, Deserialize)]
struct PkDefD {
    paths: Vec<String>,
    kind: String,
    version: u8,
}

#[derive(Serialize, Deserialize)]
struct ContainerD {
    meta: MetaD,
    props: Map<String, Value>,
    pk: PkDefD,
    unique_keys: Vec<Vec<String>>,
    default_ttl: Option<i64>,
    child_seq: u64,
    sprocs: Vec<ResD>,
    triggers: Vec<ResD>,
    udfs: Vec<ResD>,
    offer: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct UserPermsD {
    user: String,
    permissions: Vec<ResD>,
}

#[derive(Serialize, Deserialize)]
struct DatabaseD {
    meta: MetaD,
    props: Map<String, Value>,
    containers: Vec<ContainerD>,
    users: Vec<ResD>,
    permissions: Vec<UserPermsD>,
    coll_seq: u64,
    user_seq: u64,
    perm_seq: u64,
    offer: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct AccountD {
    name: String,
    databases: Vec<DatabaseD>,
    offers: Vec<OfferD>,
    db_seq: u64,
    offer_seq: u64,
    seq: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct DefsFile {
    accounts: Vec<AccountD>,
}

#[derive(Serialize, Deserialize)]
struct DocD {
    pk: Vec<Value>,
    id: String,
    value: Arc<Value>,
    rid: String,
    epk: String,
    lsn: u64,
    ts: i64,
    etag: String,
}

#[derive(Serialize, Deserialize)]
struct LogRecD {
    lsn: u64,
    pk: Vec<Value>,
    id: String,
    epk: String,
    /// 0 create, 1 replace, 2 delete
    kind: u8,
    ts: i64,
    prev_lsn: Option<u64>,
    ttl_expired: bool,
    current: Option<Arc<Value>>,
    bytes: usize,
}

#[derive(Serialize, Deserialize)]
struct LogD {
    floor: u64,
    records: Vec<LogRecD>,
}

#[derive(Serialize, Deserialize)]
struct DocsFile {
    /// Which container this is (a file is found by name, but what it holds is checked against these).
    account: String,
    db_rid: String,
    coll_rid: String,
    lsn: u64,
    doc_seq: u64,
    docs: Vec<DocD>,
    log: Option<LogD>,
}

// ---- state -> files ---------------------------------------------------------------------------------------------

fn meta_d(m: &Meta) -> MetaD {
    MetaD { id: m.id.clone(), rid: m.rid.clone(), rid_bytes: m.rid_bytes.clone(), seq: m.seq, ts: m.ts, etag: m.etag.clone() }
}

fn res_d(k: &str, r: &Resource) -> ResD {
    ResD { key: k.to_string(), meta: meta_d(&r.meta), props: r.props.clone() }
}

fn res_list(m: &BTreeMap<String, Resource>) -> Vec<ResD> {
    m.iter().map(|(k, r)| res_d(k, r)).collect()
}

fn container_d(c: &Container) -> ContainerD {
    ContainerD {
        meta: meta_d(&c.meta),
        props: c.props.clone(),
        pk: PkDefD { paths: c.pk.paths.clone(), kind: c.pk.kind.clone(), version: c.pk.version },
        unique_keys: c.unique_keys.clone(),
        default_ttl: c.default_ttl,
        child_seq: c.child_seq,
        sprocs: res_list(&c.sprocs),
        triggers: res_list(&c.triggers),
        udfs: res_list(&c.udfs),
        offer: c.offer.clone(),
    }
}

fn database_d(d: &Database) -> DatabaseD {
    DatabaseD {
        meta: meta_d(&d.meta),
        props: d.props.clone(),
        containers: d.containers.values().map(container_d).collect(),
        users: res_list(&d.users),
        permissions: d.permissions.iter().map(|(u, m)| UserPermsD { user: u.clone(), permissions: res_list(m) }).collect(),
        coll_seq: d.coll_seq,
        user_seq: d.user_seq,
        perm_seq: d.perm_seq,
        offer: d.offer.clone(),
    }
}

fn account_d(name: &str, a: &Account) -> AccountD {
    AccountD {
        name: name.to_string(),
        databases: a.databases.values().map(database_d).collect(),
        offers: a
            .offers
            .iter()
            .map(|(k, o)| OfferD {
                key: k.clone(),
                meta: meta_d(&o.meta),
                resource: o.resource.clone(),
                resource_rid: o.resource_rid.clone(),
                content: o.content.clone(),
            })
            .collect(),
        db_seq: a.db_seq,
        offer_seq: a.offer_seq,
        seq: a.seq,
    }
}

fn pk_values(pk: &[PkComp]) -> Vec<Value> {
    pk.iter().map(PkComp::to_json).collect()
}

/// What tells whether a container's file is out of date. Everything a write to the documents changes is in it
/// (every write raises the LSN; a document that expired has left without raising it, so the count is in too; the
/// container's etag tells a container from one of the same name made after a reset).
#[derive(Clone, PartialEq, Eq, Debug)]
struct Fingerprint {
    etag: String,
    lsn: u64,
    doc_seq: u64,
    docs: usize,
    log: Option<(usize, u64, u64)>,
}

fn fingerprint(c: &Container) -> Fingerprint {
    Fingerprint {
        etag: c.meta.etag.clone(),
        lsn: c.lsn,
        doc_seq: c.doc_seq,
        docs: c.docs.len(),
        log: c.log.as_ref().map(|l| (l.records.len(), l.floor, l.records.back().map(|r| r.lsn).unwrap_or(0))),
    }
}

/// What a save found under the lock for one container whose file has to be written.
struct DocsCopy {
    file: String,
    fp: Fingerprint,
    account: String,
    db_rid: String,
    coll_rid: String,
    lsn: u64,
    doc_seq: u64,
    docs: Vec<(DocKey, Doc)>,
    log: Option<LogCopy>,
}

/// A change history entry copied for a save: LSN, key, partition key hash, kind, time, previous LSN, expired by TTL,
/// the version written, size.
type LogRow = (u64, DocKey, String, ChangeKind, i64, Option<u64>, bool, Option<Arc<Value>>, usize);

struct LogCopy {
    floor: u64,
    records: Vec<LogRow>,
}

fn file_name(account: &str, db_rid: &str, coll_rid: &str) -> String {
    // 64 bit FNV-1a over the three names; what the file holds says which container it is, so a collision
    // cannot mix two containers up (the second would be seen as unreadable)
    let mut h: u64 = 0xcbf29ce484222325;
    for part in [account, db_rid, coll_rid] {
        for b in part.bytes().chain(std::iter::once(0xff)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("docs-{h:016x}.state")
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// What the last save (or load) left on disk, so the next one can leave alone what did not change.
#[derive(Default)]
pub struct Saved {
    defs_hash: Option<u64>,
    docs: HashMap<String, Fingerprint>,
}

impl Saved {
    pub fn clear(&mut self) {
        self.defs_hash = None;
        self.docs.clear();
    }
}

impl CosmosService {
    pub(crate) fn mark_changed(&self) {
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Everything a save needs, copied under the lock: the definitions, and the documents of every container whose
    /// file is out of date.
    fn copy_for_save(&self, saved: &Saved) -> (DefsFile, Vec<DocsCopy>, HashSet<String>) {
        let st = self.state.lock();
        let mut defs = DefsFile::default();
        let mut copies = Vec::new();
        let mut keep = HashSet::new();
        let mut names: Vec<&String> = st.accounts.keys().collect();
        names.sort();
        for name in names {
            let a = &st.accounts[name];
            if a.databases.is_empty() && a.offers.is_empty() && a.db_seq == 0 && a.seq == 0 && a.offer_seq == 0 {
                continue;
            }
            defs.accounts.push(account_d(name, a));
            for d in a.databases.values() {
                for c in d.containers.values() {
                    let file = file_name(name, &d.meta.rid, &c.meta.rid);
                    keep.insert(file.clone());
                    let fp = fingerprint(c);
                    if saved.docs.get(&file) == Some(&fp) {
                        continue;
                    }
                    copies.push(DocsCopy {
                        file,
                        fp,
                        account: name.clone(),
                        db_rid: d.meta.rid.clone(),
                        coll_rid: c.meta.rid.clone(),
                        lsn: c.lsn,
                        doc_seq: c.doc_seq,
                        docs: c.docs.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                        log: c.log.as_ref().map(|l| LogCopy {
                            floor: l.floor,
                            records: l
                                .records
                                .iter()
                                .map(|r| {
                                    (
                                        r.lsn,
                                        r.key.clone(),
                                        r.epk.clone(),
                                        r.kind,
                                        r.ts,
                                        r.prev_lsn,
                                        r.ttl_expired,
                                        r.current.clone(),
                                        r.bytes,
                                    )
                                })
                                .collect(),
                        }),
                    });
                }
            }
        }
        (defs, copies, keep)
    }

    pub(crate) fn save_to(&self, dir: &Path) -> io::Result<()> {
        // one save at a time (core promises it; a test or an embedder may not)
        let mut saved = self.saved.lock();
        let (defs, copies, keep) = self.copy_for_save(&saved);
        // -- the lock is released; everything below is encoding and writing --
        for c in copies {
            let file = DocsFile {
                account: c.account,
                db_rid: c.db_rid,
                coll_rid: c.coll_rid,
                lsn: c.lsn,
                doc_seq: c.doc_seq,
                docs: c
                    .docs
                    .into_iter()
                    .map(|(k, d)| DocD {
                        pk: pk_values(&k.pk),
                        id: k.id,
                        value: d.value,
                        rid: d.rid,
                        epk: d.epk,
                        lsn: d.lsn,
                        ts: d.ts,
                        etag: d.etag,
                    })
                    .collect(),
                log: c.log.map(|l| LogD {
                    floor: l.floor,
                    records: l
                        .records
                        .into_iter()
                        .map(|(lsn, key, epk, kind, ts, prev_lsn, ttl_expired, current, bytes)| LogRecD {
                            lsn,
                            pk: pk_values(&key.pk),
                            id: key.id,
                            epk,
                            kind: match kind {
                                ChangeKind::Create => 0,
                                ChangeKind::Replace => 1,
                                ChangeKind::Delete => 2,
                            },
                            ts,
                            prev_lsn,
                            ttl_expired,
                            current,
                            bytes,
                        })
                        .collect(),
                }),
            };
            azemu_statefile::write_stream(&dir.join(&c.file), DOCS_KIND, VERSION, &file)?;
            saved.docs.insert(c.file, c.fp);
        }
        let bytes = persist::encode(DEFS_KIND, VERSION, &defs)?;
        let h = hash_bytes(&bytes);
        if saved.defs_hash != Some(h) {
            persist::write_atomic_private(&dir.join(DEFS_FILE), &bytes)?;
            saved.defs_hash = Some(h);
        }
        // files of containers that are gone
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("docs-") && name.ends_with(".state") && !keep.contains(&name) {
                fs::remove_file(entry.path())?;
            }
        }
        saved.docs.retain(|k, _| keep.contains(k));
        Ok(())
    }

    pub(crate) fn load_from(&self, dir: &Path) -> io::Result<()> {
        let Some(defs) = read_state::<DefsFile>(&dir.join(DEFS_FILE), DEFS_KIND)? else {
            return Ok(());
        };
        let mut state = State::default();
        let mut saved = Saved::default();
        for a in defs.accounts {
            let name = a.name.clone();
            let account = build_account(dir, a, &mut saved)?;
            if state.accounts.insert(name.clone(), account).is_some() {
                return Err(bad(&dir.join(DEFS_FILE), format!("account {name} is listed twice")));
            }
        }
        *self.state.lock() = state;
        *self.saved.lock() = saved;
        Ok(())
    }
}

fn bad(path: &Path, msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {msg}", path.display()))
}

/// Read one state file, `None` when it does not exist. A file over the size limit is refused before it is read.
fn read_state<T: serde::de::DeserializeOwned>(path: &Path, kind: &str) -> io::Result<Option<T>> {
    Ok(azemu_statefile::read_state(path, kind, VERSION)?.map(|(_, v)| v))
}

fn meta_from(m: MetaD) -> Meta {
    Meta { id: m.id, rid: m.rid, rid_bytes: m.rid_bytes, seq: m.seq, ts: m.ts, etag: m.etag }
}

fn res_map(list: Vec<ResD>) -> BTreeMap<String, Resource> {
    list.into_iter().map(|r| (r.key, Resource { meta: meta_from(r.meta), props: r.props })).collect()
}

fn build_account(dir: &Path, a: AccountD, saved: &mut Saved) -> io::Result<Account> {
    let mut account = Account { db_seq: a.db_seq, offer_seq: a.offer_seq, seq: a.seq, ..Default::default() };
    for o in a.offers {
        account
            .offers
            .insert(o.key, Offer { meta: meta_from(o.meta), resource: o.resource, resource_rid: o.resource_rid, content: o.content });
    }
    for d in a.databases {
        let db_rid = d.meta.rid.clone();
        let mut db = Database {
            meta: meta_from(d.meta),
            props: d.props,
            containers: BTreeMap::new(),
            users: res_map(d.users),
            permissions: d.permissions.into_iter().map(|u| (u.user, res_map(u.permissions))).collect(),
            coll_seq: d.coll_seq,
            user_seq: d.user_seq,
            perm_seq: d.perm_seq,
            offer: d.offer,
        };
        for c in d.containers {
            let coll_rid = c.meta.rid.clone();
            let id = c.meta.id.clone();
            let file = file_name(&a.name, &db_rid, &coll_rid);
            let mut container = Container {
                meta: meta_from(c.meta),
                props: c.props,
                pk: PartitionKeyDef { paths: c.pk.paths, kind: c.pk.kind, version: c.pk.version },
                unique_keys: c.unique_keys,
                default_ttl: c.default_ttl,
                docs: BTreeMap::new(),
                by_rid: HashMap::new(),
                changes: BTreeMap::new(),
                lsn: 0,
                doc_seq: 0,
                child_seq: c.child_seq,
                sprocs: res_map(c.sprocs),
                triggers: res_map(c.triggers),
                udfs: res_map(c.udfs),
                offer: c.offer,
                min_expiry: 0,
                log: None,
            };
            let path = dir.join(&file);
            if let Some(f) = read_state::<DocsFile>(&path, DOCS_KIND)? {
                if f.account != a.name || f.db_rid != db_rid || f.coll_rid != coll_rid {
                    return Err(bad(
                        &path,
                        format!("it holds another container's documents (expected {}/{}/{})", a.name, db_rid, coll_rid),
                    ));
                }
                fill_container(&mut container, f).map_err(|m| bad(&path, m))?;
            }
            saved.docs.insert(file, fingerprint(&container));
            if db.containers.insert(id.clone(), container).is_some() {
                return Err(bad(&dir.join(DEFS_FILE), format!("container {id} is listed twice in a database")));
            }
        }
        let name = db.meta.id.clone();
        if account.databases.insert(name.clone(), db).is_some() {
            return Err(bad(&dir.join(DEFS_FILE), format!("database {name} is listed twice")));
        }
    }
    Ok(account)
}

fn pk_from(values: &[Value]) -> Result<Vec<PkComp>, String> {
    values
        .iter()
        .map(|v| PkComp::from_json(v).map_err(|_| "a partition key value is not a string, number, boolean, null or {}".to_string()))
        .collect()
}

fn fill_container(c: &mut Container, f: DocsFile) -> Result<(), String> {
    let mut max_lsn = f.lsn;
    for d in f.docs {
        let key = DocKey { pk: pk_from(&d.pk)?, id: d.id };
        max_lsn = max_lsn.max(d.lsn);
        let doc = Doc { value: d.value, rid: d.rid, epk: d.epk, lsn: d.lsn, ts: d.ts, etag: d.etag };
        c.by_rid.insert(doc.rid.clone(), key.clone());
        c.changes.insert(doc.lsn, key.clone());
        c.docs.insert(key, doc);
    }
    c.lsn = max_lsn;
    c.doc_seq = f.doc_seq;
    if let Some(l) = f.log {
        let mut log = ChangeLog::new(l.floor);
        for r in l.records {
            log.bytes += r.bytes;
            log.records.push_back(ChangeRecord {
                lsn: r.lsn,
                key: DocKey { pk: pk_from(&r.pk)?, id: r.id },
                epk: r.epk,
                kind: match r.kind {
                    0 => ChangeKind::Create,
                    1 => ChangeKind::Replace,
                    2 => ChangeKind::Delete,
                    _ => return Err("a change history entry has an unknown kind".into()),
                },
                ts: r.ts,
                prev_lsn: r.prev_lsn,
                ttl_expired: r.ttl_expired,
                current: r.current,
                bytes: r.bytes,
            });
        }
        c.log = Some(log);
    }
    Ok(())
}
