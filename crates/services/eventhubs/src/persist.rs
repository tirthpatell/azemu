//! Saving the state to disk and reading it back (`--data-dir`). The reasons are in `docs/services/persistence.md`
//! (section "Event Hubs and Kafka").
//!
//! Event Hubs and Kafka share one state (a Kafka topic is an event hub), so they share these files, and the `kafka`
//! service object keeps nothing of its own.
//!
//! Layout of the service directory:
//!
//! * `eventhubs.state` (kind `eventhubs`): the namespaces with their event hubs (partition count, retention,
//!   consumer groups, the round-robin counter), the committed offsets of the Kafka consumer groups and the next
//!   Kafka producer id. Small; written again only when its content changed.
//! * `part-<hash>.state` (kind `eventhubs-partition`): one file per partition of every event hub, with its events
//!   (sequence number, offset, enqueue time, partition key, the message exactly as it was sent), the counters that
//!   make the next sequence number and offset, and what the partition remembers of Kafka idempotent producers.
//!   Written again only when that partition changed since the last save.
//!
//! A save copies what it writes under the lock (an event is an `Arc`, so a copy is a pointer per event) and encodes
//! and writes after the lock is released. Partition files are written before `eventhubs.state`; files nothing refers
//! to are removed last.
//!
//! Not saved: receivers, epochs, Kafka consumer group members and generations (after a restart a group is empty and
//! its members join again, which is a rebalance; committed offsets stay).

use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use azemu_amqp::message::Message;
use azemu_core::persist;
use azemu_statefile::{hashed_name, read_state, remove_stale, write_stream};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::kafka::groups::Committed;
use crate::kafka::store::ProducerSeq;
use crate::model::{
    current_revision, key, Event, Hub, Partition, EVENT_OVERHEAD, MAX_CONSUMER_GROUPS, MAX_HUBS_PER_NAMESPACE, MAX_PARTITIONS,
};
use crate::state::{raise_hub_ids, Namespace};
use crate::{EventHubsService, Shared};

pub const DEFS_FILE: &str = "eventhubs.state";
pub const DEFS_KIND: &str = "eventhubs";
pub const PART_KIND: &str = "eventhubs-partition";
pub const VERSION: u32 = 1;

// ---- the shapes in the files --------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct HubD {
    name: String,
    id: u64,
    created_ms: i64,
    retention_ms: i64,
    partitions: u32,
    consumer_groups: Vec<(String, String)>,
    autocreated: bool,
    round_robin: u64,
}

#[derive(Serialize, Deserialize)]
struct NsD {
    name: String,
    hubs: Vec<HubD>,
}

#[derive(Serialize, Deserialize)]
struct OffsetD {
    topic: String,
    partition: i32,
    offset: i64,
    metadata: Option<String>,
    leader_epoch: i32,
}

#[derive(Serialize, Deserialize)]
struct GroupD {
    ns: String,
    group: String,
    offsets: Vec<OffsetD>,
}

#[derive(Serialize, Deserialize, Default)]
struct DefsFile {
    default_alias: Option<String>,
    namespaces: Vec<NsD>,
    kafka_groups: Vec<GroupD>,
    next_producer_id: i64,
}

#[derive(Serialize, Deserialize)]
struct EventD {
    seq: i64,
    offset: i64,
    enqueued_ms: i64,
    partition_key: Option<String>,
    /// The AMQP encoding of the message as it was stored (without the annotations only the broker sets).
    message: Bytes,
    size: usize,
}

struct EventsOut(Vec<Arc<Event>>);

impl Serialize for EventsOut {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for e in &self.0 {
            seq.serialize_element(&EventD {
                seq: e.seq,
                offset: e.offset,
                enqueued_ms: e.enqueued_ms,
                partition_key: e.partition_key.clone(),
                message: Bytes::from(e.message.encode()),
                size: e.size,
            })?;
        }
        seq.end()
    }
}

#[derive(Serialize, Deserialize)]
struct ProducerD {
    id: i64,
    epoch: i16,
    next_seq: i32,
    recent: Vec<(i32, i32, i64)>,
}

#[derive(Serialize)]
struct PartOut {
    /// which partition this is: namespace and hub (lower case), the hub's id, the partition number
    ns: String,
    hub: String,
    hub_id: u64,
    pid: u32,
    next_seq: i64,
    next_offset: i64,
    last_offset: Option<i64>,
    last_ms: Option<i64>,
    total_events: u64,
    max_bytes: usize,
    producers: Vec<ProducerD>,
    events: EventsOut,
}

#[derive(Deserialize)]
struct PartIn {
    ns: String,
    hub: String,
    hub_id: u64,
    pid: u32,
    next_seq: i64,
    next_offset: i64,
    last_offset: Option<i64>,
    last_ms: Option<i64>,
    total_events: u64,
    max_bytes: usize,
    producers: Vec<ProducerD>,
    events: Vec<EventD>,
}

// ---- what a save copies under the lock ------------------------------------------------------------------------

struct PartCopy {
    file: String,
    rev: u64,
    out: PartOut,
}

/// What the last save (or load) left on disk.
#[derive(Default)]
pub struct Saved {
    defs_hash: Option<u64>,
    parts: std::collections::HashMap<String, u64>,
}

impl Saved {
    pub fn forget(&mut self) {
        self.defs_hash = None;
        self.parts.clear();
    }
}

/// What the service keeps between calls of `save`.
#[derive(Default)]
pub struct SaveMemory(pub parking_lot::Mutex<Saved>);

fn part_file(ns: &str, hub: &str, pid: usize) -> String {
    hashed_name("part", &[ns, hub, &pid.to_string()])
}

fn hash_bytes(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

impl Shared {
    fn copy_for_save(&self, saved: &Saved) -> (DefsFile, Vec<PartCopy>, HashSet<String>) {
        let st = self.state.lock();
        let mut defs = DefsFile {
            default_alias: st.default_alias.clone(),
            next_producer_id: self.next_producer_id.load(Ordering::Relaxed),
            ..Default::default()
        };
        let mut copies = Vec::new();
        let mut keep = HashSet::new();
        for (ns_name, ns) in &st.namespaces {
            let mut nd = NsD { name: ns_name.clone(), hubs: vec![] };
            for hub in ns.hubs.values() {
                let hub_key = key(&hub.name);
                nd.hubs.push(HubD {
                    name: hub.name.clone(),
                    id: hub.id,
                    created_ms: hub.created_ms,
                    retention_ms: hub.retention_ms,
                    partitions: hub.partitions.len() as u32,
                    consumer_groups: hub.consumer_groups.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    autocreated: hub.autocreated,
                    round_robin: hub.round_robin() as u64,
                });
                for (pid, p) in hub.partitions.iter().enumerate() {
                    let file = part_file(ns_name, &hub_key, pid);
                    keep.insert(file.clone());
                    if saved.parts.get(&file) == Some(&p.rev) {
                        continue;
                    }
                    let mut producers: Vec<ProducerD> = p
                        .producers
                        .iter()
                        .map(|(id, s)| ProducerD {
                            id: *id,
                            epoch: s.epoch,
                            next_seq: s.next_seq,
                            recent: s.recent.iter().copied().collect(),
                        })
                        .collect();
                    producers.sort_by_key(|p| p.id);
                    copies.push(PartCopy {
                        file,
                        rev: p.rev,
                        out: PartOut {
                            ns: ns_name.clone(),
                            hub: hub_key.clone(),
                            hub_id: hub.id,
                            pid: pid as u32,
                            next_seq: p.next_seq,
                            next_offset: p.next_offset,
                            last_offset: p.last_offset,
                            last_ms: p.last_ms,
                            total_events: p.total_events,
                            max_bytes: p.max_bytes,
                            producers,
                            events: EventsOut(p.events.iter().cloned().collect()),
                        },
                    });
                }
            }
            defs.namespaces.push(nd);
        }
        defs.kafka_groups = self
            .kafka
            .export_offsets()
            .into_iter()
            .map(|(ns, group, offsets)| GroupD {
                ns,
                group,
                offsets: offsets
                    .into_iter()
                    .map(|(topic, partition, c)| OffsetD {
                        topic,
                        partition,
                        offset: c.offset,
                        metadata: c.metadata,
                        leader_epoch: c.leader_epoch,
                    })
                    .collect(),
            })
            .collect();
        (defs, copies, keep)
    }

    pub fn save_to(&self, dir: &Path, saved: &mut Saved) -> io::Result<()> {
        let (defs, copies, keep) = self.copy_for_save(saved);
        // -- the lock is released; everything below is encoding and writing --
        for c in copies {
            write_stream(&dir.join(&c.file), PART_KIND, VERSION, &c.out)?;
            saved.parts.insert(c.file, c.rev);
        }
        let bytes = persist::encode(DEFS_KIND, VERSION, &defs)?;
        let h = hash_bytes(&bytes);
        if saved.defs_hash != Some(h) {
            persist::write_atomic_private(&dir.join(DEFS_FILE), &bytes)?;
            saved.defs_hash = Some(h);
        }
        remove_stale(dir, "part", &keep)?;
        saved.parts.retain(|k, _| keep.contains(k));
        Ok(())
    }

    pub fn load_from(&self, dir: &Path, saved: &mut Saved) -> io::Result<()> {
        let defs_path = dir.join(DEFS_FILE);
        let Some((_, defs)) = read_state::<DefsFile>(&defs_path, DEFS_KIND, VERSION)? else {
            return Ok(());
        };
        let bad = |p: &Path, m: String| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {m}", p.display()));
        let default_alias = defs.default_alias;
        let mut namespaces = std::collections::BTreeMap::new();
        let mut max_id = 0;
        for nd in defs.namespaces {
            let mut ns = Namespace::default();
            if nd.hubs.len() > MAX_HUBS_PER_NAMESPACE {
                return Err(bad(
                    &defs_path,
                    format!("namespace {} lists {} event hubs, more than the {MAX_HUBS_PER_NAMESPACE} allowed", nd.name, nd.hubs.len()),
                ));
            }
            for hd in nd.hubs {
                if hd.partitions == 0 || hd.partitions > MAX_PARTITIONS {
                    return Err(bad(
                        &defs_path,
                        format!("event hub {} has {} partitions (1 to {MAX_PARTITIONS} are possible)", hd.name, hd.partitions),
                    ));
                }
                if hd.consumer_groups.len() > MAX_CONSUMER_GROUPS + 1 {
                    return Err(bad(&defs_path, format!("event hub {} has {} consumer groups", hd.name, hd.consumer_groups.len())));
                }
                let hub_key = key(&hd.name);
                let mut hub = Hub::new(&hd.name, hd.id, hd.partitions, hd.retention_ms, hd.created_ms);
                hub.consumer_groups = hd.consumer_groups.into_iter().collect();
                hub.autocreated = hd.autocreated;
                hub.set_round_robin(hd.round_robin as usize);
                max_id = max_id.max(hd.id);
                for pid in 0..hd.partitions as usize {
                    let file = part_file(&nd.name, &hub_key, pid);
                    let p = dir.join(&file);
                    if let Some((_, f)) = read_state::<PartIn>(&p, PART_KIND, VERSION)? {
                        if f.ns != nd.name || f.hub != hub_key || f.pid as usize != pid {
                            return Err(bad(
                                &p,
                                format!(
                                    "it holds partition {} of event hub {} in {}, expected partition {pid} of {hub_key} in {}",
                                    f.pid, f.hub, f.ns, nd.name
                                ),
                            ));
                        }
                        if f.hub_id != hd.id {
                            // left over from an event hub of the same name that was deleted and made again
                            tracing::warn!("{}: belongs to an earlier event hub of the same name; ignored", p.display());
                        } else {
                            fill_partition(&mut hub.partitions[pid], f).map_err(|m| bad(&p, m))?;
                        }
                    }
                    saved.parts.insert(file, hub.partitions[pid].rev);
                }
                if ns.hubs.insert(hub_key, hub).is_some() {
                    return Err(bad(&defs_path, format!("event hub {} is listed twice in namespace {}", hd.name, nd.name)));
                }
            }
            if namespaces.insert(nd.name.clone(), ns).is_some() {
                return Err(bad(&defs_path, format!("namespace {} is listed twice", nd.name)));
            }
        }
        let groups = defs
            .kafka_groups
            .into_iter()
            .map(|g| {
                let offsets = g
                    .offsets
                    .into_iter()
                    .map(|o| (o.topic, o.partition, Committed { offset: o.offset, metadata: o.metadata, leader_epoch: o.leader_epoch }))
                    .collect();
                (g.ns, g.group, offsets)
            })
            .collect();
        raise_hub_ids(max_id);
        self.next_producer_id.store(defs.next_producer_id.max(1), Ordering::Relaxed);
        {
            let mut st = self.state.lock();
            st.namespaces = namespaces;
            st.default_alias = default_alias;
        }
        self.kafka.import_offsets(groups);
        Ok(())
    }
}

fn fill_partition(p: &mut Partition, f: PartIn) -> Result<(), String> {
    let mut events = VecDeque::with_capacity(f.events.len().min(1 << 16));
    let mut bytes = 0usize;
    let mut expect: Option<i64> = None;
    for e in f.events {
        if let Some(want) = expect {
            if e.seq != want {
                return Err(format!("the events are not in sequence: {} follows {}", e.seq, want - 1));
            }
        }
        expect = Some(e.seq.saturating_add(1));
        let message = Message::decode(&e.message).map_err(|err| format!("event {} cannot be read: {err}", e.seq))?;
        bytes = bytes.saturating_add(e.size.saturating_add(EVENT_OVERHEAD));
        events.push_back(Arc::new(Event {
            seq: e.seq,
            offset: e.offset,
            enqueued_ms: e.enqueued_ms,
            partition_key: e.partition_key,
            message,
            size: e.size,
        }));
    }
    let next_seq = events.back().map(|e| e.seq + 1).unwrap_or(0).max(f.next_seq);
    let next_offset = f.next_offset.max(events.back().map(|e| e.offset.saturating_add(1)).unwrap_or(0));
    p.events = events;
    p.next_seq = next_seq;
    p.next_offset = next_offset;
    p.last_offset = f.last_offset;
    p.last_ms = f.last_ms;
    p.total_events = f.total_events;
    p.bytes_held = bytes;
    p.max_bytes = f.max_bytes;
    p.producers = f
        .producers
        .into_iter()
        .map(|d| (d.id, ProducerSeq { epoch: d.epoch, next_seq: d.next_seq, recent: d.recent.into_iter().collect() }))
        .collect();
    p.rev = crate::model::next_revision();
    Ok(())
}

// ---- the service ------------------------------------------------------------------------------------------------

impl EventHubsService {
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
