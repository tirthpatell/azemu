//! Saving the state to disk and reading it back (`--data-dir`). The reasons are in `docs/services/persistence.md`
//! (section "Event Grid").
//!
//! Layout of the service directory:
//!
//! * `eventgrid.state` (kind `eventgrid`): every topic, domain, domain topic, namespace, namespace topic and event
//!   source with its subscriptions (the configuration of each, as the API takes it, its validation state and creation
//!   time) and the counter that numbers push deliveries. Small; written again only when its content changed.
//! * `sub-<hash>.state` (kind `eventgrid-sub`): what happens to the events of one subscription: the pull queue
//!   (events, delivery counts, locks, release delays, dead letters), the delivery log, the counters, and the push
//!   deliveries that were not finished (the event, where it goes, how many attempts were made, when the first was
//!   made). Written again only when that subscription changed since the last save.
//!
//! Times are saved on the wall clock and come back as moments of the monotonic clock the service works with, so
//! locks, release delays and time to live mean the same after a restart. A lock stays good after a restart until it
//! ends (a receiver of the HTTP API is not tied to a connection). Push deliveries that were waiting start again half a
//! second after the start with the attempts they had made; the wait between two attempts is not kept, the next
//! attempt is made at once.
//!
//! Not kept: the tokens of locks that already ended (a token that was used or lost before the restart is answered
//! `BadToken` instead of `TokenLost`), the contents of the webhook handshake in progress.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::persist;
use azemu_core::time::OffsetDateTime;
use azemu_statefile as statefile;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;

use crate::destination::{DestinationKind, DestinationTarget};
use crate::events::Schema;
use crate::model::{InputSchema, SubscriptionConfig, TopicKind};
use crate::pull::{Now, Queue, QueueSnap};
use crate::state::{current_revision, next_revision, DeliveryAttempt, Dest, Host, Pending, PushJob, State, Sub, SubKey, Topic, Validation};
use crate::{EventGridService, Shared};

pub const DEFS_FILE: &str = "eventgrid.state";
pub const DEFS_KIND: &str = "eventgrid";
pub const SUB_KIND: &str = "eventgrid-sub";
pub const VERSION: u32 = 1;

// ---- the shapes in the files --------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct ValidationD {
    status: String,
    detail: String,
    code: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SubD {
    /// the key in the map (lower case) and the name as it was written
    key: String,
    name: String,
    /// The configuration in the form the API takes it.
    cfg: Value,
    created_ns: i64,
    validation: ValidationD,
}

#[derive(Serialize, Deserialize)]
struct TopicD {
    key: String,
    name: String,
    input_schema: InputSchema,
    published: u64,
    subs: Vec<SubD>,
}

#[derive(Serialize, Deserialize)]
struct HostD {
    key: String,
    name: String,
    kind: TopicKind,
    input_schema: InputSchema,
    published: u64,
    resource_id: Option<String>,
    subs: Vec<SubD>,
    topics: Vec<TopicD>,
}

#[derive(Serialize, Deserialize, Default)]
struct DefsFile {
    hosts: Vec<HostD>,
    next_push_id: u64,
}

#[derive(Serialize, Deserialize)]
struct DestD {
    kind: String,
    namespace: String,
    entity: String,
    ttl_seconds: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct PendingD {
    id: u64,
    bytes: u64,
    event_id: String,
    attempts: u32,
    started_ms: i64,
    sub_name: String,
    url: String,
    schema: Schema,
    body: Arc<Value>,
    data_version: String,
    max_attempts: u32,
    ttl_ms: u64,
    intervals_ms: Option<Vec<u64>>,
    dest: Option<DestD>,
}

#[derive(Serialize, Deserialize)]
struct SubFile {
    /// which subscription this is: host key, topic key (domain topics, namespace topics), subscription key
    host: String,
    topic: Option<String>,
    sub: String,
    queue: QueueSnap,
    log: Vec<DeliveryAttempt>,
    delivered: u64,
    failed_attempts: u64,
    shed: u64,
    pending: Vec<PendingD>,
}

fn kind_name(k: DestinationKind) -> String {
    k.endpoint_type().to_string()
}

// ---- state -> files ---------------------------------------------------------------------------------------------

fn sub_d(key: &str, s: &Sub) -> SubD {
    SubD {
        key: key.to_string(),
        name: s.name.clone(),
        cfg: serde_json::to_value(&s.cfg).unwrap_or(Value::Null),
        created_ns: s.created.unix_timestamp_nanos().clamp(i64::MIN as i128, i64::MAX as i128) as i64,
        validation: ValidationD {
            status: s.validation.status.clone(),
            detail: s.validation.detail.clone(),
            code: s.validation.code.clone(),
        },
    }
}

fn sub_file_name(host: &str, topic: Option<&str>, sub: &str) -> String {
    statefile::hashed_name("sub", &[host, topic.unwrap_or(""), if topic.is_some() { "t" } else { "h" }, sub])
}

struct SubCopy {
    file: String,
    fp: (u64, u64),
    out: SubFile,
}

/// What the last save (or load) left on disk.
#[derive(Default)]
pub struct Saved {
    defs_hash: Option<u64>,
    subs: std::collections::HashMap<String, (u64, u64)>,
}

impl Saved {
    pub fn forget(&mut self) {
        self.defs_hash = None;
        self.subs.clear();
    }
}

/// What the service keeps between calls of `save`.
#[derive(Default)]
pub struct SaveMemory(pub parking_lot::Mutex<Saved>);

fn hash_bytes(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

fn copy_sub(host: &str, topic: Option<&str>, key: &str, s: &Sub, now: &Now) -> SubCopy {
    SubCopy {
        file: sub_file_name(host, topic, key),
        fp: (s.rev, s.queue.rev),
        out: SubFile {
            host: host.to_string(),
            topic: topic.map(str::to_string),
            sub: key.to_string(),
            queue: s.queue.export(now),
            log: s.log.iter().cloned().collect(),
            delivered: s.delivered,
            failed_attempts: s.failed_attempts,
            shed: s.shed,
            pending: s
                .pending
                .values()
                .map(|p| PendingD {
                    id: p.job.id,
                    bytes: p.bytes,
                    event_id: p.event_id.clone(),
                    attempts: p.attempts,
                    started_ms: p.started_ms,
                    sub_name: p.job.sub_name.clone(),
                    url: p.job.url.clone(),
                    schema: p.job.schema,
                    body: p.body.clone(),
                    data_version: p.job.data_version.clone(),
                    max_attempts: p.job.max_attempts,
                    ttl_ms: p.job.ttl.as_millis().min(u64::MAX as u128) as u64,
                    intervals_ms: p.job.intervals_ms.clone(),
                    dest: p.job.dest.as_ref().map(|d| DestD {
                        kind: kind_name(d.kind),
                        namespace: d.target.namespace.clone(),
                        entity: d.target.entity.clone(),
                        ttl_seconds: d.ttl_seconds,
                    }),
                })
                .collect(),
        },
    }
}

impl Shared {
    fn copy_for_save(&self, saved: &Saved) -> (DefsFile, Vec<SubCopy>, HashSet<String>) {
        let now = Now::now();
        let st = self.state.lock();
        let mut defs = DefsFile { next_push_id: st.next_push_id, ..Default::default() };
        let mut copies = Vec::new();
        let mut keep = HashSet::new();
        let mut consider = |host: &str, topic: Option<&str>, key: &str, s: &Sub, copies: &mut Vec<SubCopy>| {
            let file = sub_file_name(host, topic, key);
            keep.insert(file.clone());
            if saved.subs.get(&file) != Some(&(s.rev, s.queue.rev)) {
                copies.push(copy_sub(host, topic, key, s, &now));
            }
        };
        for (hk, h) in &st.hosts {
            let mut hd = HostD {
                key: hk.clone(),
                name: h.name.clone(),
                kind: h.kind,
                input_schema: h.input_schema,
                published: h.published,
                resource_id: h.resource_id.clone(),
                subs: vec![],
                topics: vec![],
            };
            for (sk, s) in &h.subs {
                hd.subs.push(sub_d(sk, s));
                consider(hk, None, sk, s, &mut copies);
            }
            for (tk, t) in &h.topics {
                let mut td =
                    TopicD { key: tk.clone(), name: t.name.clone(), input_schema: t.input_schema, published: t.published, subs: vec![] };
                for (sk, s) in &t.subs {
                    td.subs.push(sub_d(sk, s));
                    consider(hk, Some(tk), sk, s, &mut copies);
                }
                hd.topics.push(td);
            }
            defs.hosts.push(hd);
        }
        (defs, copies, keep)
    }

    pub fn save_to(&self, dir: &Path, saved: &mut Saved) -> io::Result<()> {
        let (defs, copies, keep) = self.copy_for_save(saved);
        // -- the lock is released; everything below is encoding and writing --
        for c in copies {
            statefile::write_stream(&dir.join(&c.file), SUB_KIND, VERSION, &c.out)?;
            saved.subs.insert(c.file, c.fp);
        }
        let bytes = persist::encode(DEFS_KIND, VERSION, &defs)?;
        let h = hash_bytes(&bytes);
        if saved.defs_hash != Some(h) {
            persist::write_atomic_private(&dir.join(DEFS_FILE), &bytes)?;
            saved.defs_hash = Some(h);
        }
        statefile::remove_stale(dir, "sub", &keep)?;
        saved.subs.retain(|k, _| keep.contains(k));
        Ok(())
    }

    pub fn load_from(self: &Arc<Self>, dir: &Path, saved: &mut Saved) -> io::Result<()> {
        let defs_path = dir.join(DEFS_FILE);
        let Some((_, defs)) = statefile::read_state::<DefsFile>(&defs_path, DEFS_KIND, VERSION)? else {
            return Ok(());
        };
        let bad = |p: &Path, m: String| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {m}", p.display()));
        let now = Now::now();
        let mut state = State::new(self.opts.push_limits);
        state.next_push_id = defs.next_push_id;
        let mut resume: Vec<(PushJob, u32, Instant)> = Vec::new();
        let mut files_seen = HashSet::new();
        for hd in defs.hosts {
            let mut host = Host::new(&hd.name, hd.kind, hd.input_schema);
            host.published = hd.published;
            host.resource_id = hd.resource_id;
            let mut load_sub = |topic: Option<&str>, sd: SubD, saved: &mut Saved| -> io::Result<Sub> {
                let cfg: SubscriptionConfig = serde_json::from_value(sd.cfg)
                    .map_err(|e| bad(&defs_path, format!("subscription {}: the configuration cannot be read: {e}", sd.name)))?;
                let created = OffsetDateTime::from_unix_timestamp_nanos(sd.created_ns as i128).unwrap_or(OffsetDateTime::UNIX_EPOCH);
                let mut sub = Sub::new(&sd.name, cfg, created);
                sub.validation = Validation { status: sd.validation.status, detail: sd.validation.detail, code: sd.validation.code };
                let file = sub_file_name(&hd.key, topic, &sd.key);
                if !files_seen.insert(file.clone()) {
                    return Err(bad(&defs_path, format!("subscription {} is listed twice", sd.name)));
                }
                let p = dir.join(&file);
                if let Some((_, f)) = statefile::read_state::<SubFile>(&p, SUB_KIND, VERSION)? {
                    if f.host != hd.key || f.topic.as_deref() != topic || f.sub != sd.key {
                        return Err(bad(&p, format!("it holds subscription {} of {}, expected {} of {}", f.sub, f.host, sd.key, hd.key)));
                    }
                    sub.queue = Queue::import(f.queue, &now).map_err(|m| bad(&p, m))?;
                    sub.log = VecDeque::from(f.log);
                    sub.delivered = f.delivered;
                    sub.failed_attempts = f.failed_attempts;
                    sub.shed = f.shed;
                    let key = SubKey { host: hd.key.clone(), topic: topic.map(str::to_string), sub: sd.key.clone() };
                    for pd in f.pending {
                        let dest = match pd.dest {
                            None => None,
                            Some(d) => {
                                let kind = DestinationKind::parse(&d.kind)
                                    .ok_or_else(|| bad(&p, format!("a delivery goes to an unknown kind of destination: {}", d.kind)))?;
                                Some(Dest {
                                    kind,
                                    target: DestinationTarget { namespace: d.namespace, entity: d.entity },
                                    ttl_seconds: d.ttl_seconds,
                                })
                            }
                        };
                        let cancel = Arc::new(Notify::new());
                        let job = PushJob {
                            key: key.clone(),
                            id: pd.id,
                            cancel: cancel.clone(),
                            sub_name: pd.sub_name,
                            url: pd.url,
                            schema: pd.schema,
                            body: pd.body.clone(),
                            event_id: pd.event_id.clone(),
                            data_version: pd.data_version,
                            max_attempts: pd.max_attempts,
                            ttl: Duration::from_millis(pd.ttl_ms),
                            intervals_ms: pd.intervals_ms,
                            dest,
                        };
                        let started = {
                            let age = (now.wall.unix_timestamp_nanos() / 1_000_000) as i64 - pd.started_ms;
                            now.mono.checked_sub(Duration::from_millis(age.max(0) as u64)).unwrap_or(now.mono)
                        };
                        sub.pending_bytes += pd.bytes;
                        sub.pending.insert(
                            pd.id,
                            Pending {
                                bytes: pd.bytes,
                                body: pd.body,
                                event_id: pd.event_id,
                                cancel,
                                job: job.clone(),
                                attempts: pd.attempts,
                                started_ms: pd.started_ms,
                            },
                        );
                        resume.push((job, pd.attempts, started));
                    }
                }
                saved.subs.insert(file, (sub.rev, sub.queue.rev));
                Ok(sub)
            };
            for sd in hd.subs {
                let k = sd.key.clone();
                let s = load_sub(None, sd, saved)?;
                host.subs.insert(k, s);
            }
            for td in hd.topics {
                let mut topic = Topic { name: td.name, input_schema: td.input_schema, subs: BTreeMap::new(), published: td.published };
                for sd in td.subs {
                    let k = sd.key.clone();
                    let s = load_sub(Some(&td.key), sd, saved)?;
                    topic.subs.insert(k, s);
                }
                host.topics.insert(td.key, topic);
            }
            if state.hosts.insert(hd.key.clone(), host).is_some() {
                return Err(bad(&defs_path, format!("{} is listed twice", hd.key)));
            }
        }
        *self.state.lock() = state;
        crate::push::resume_jobs(self, resume);
        next_revision();
        Ok(())
    }
}

impl EventGridService {
    pub(crate) fn save_dir(&self, dir: &Path) -> io::Result<()> {
        let mut saved = self.saved.0.lock();
        self.shared.save_to(dir, &mut saved)
    }

    pub(crate) fn load_dir(&self, dir: &Path) -> io::Result<()> {
        let mut saved = self.saved.0.lock();
        self.shared.load_from(dir, &mut saved)
    }

    pub(crate) fn generation(&self) -> u64 {
        current_revision()
    }
}
