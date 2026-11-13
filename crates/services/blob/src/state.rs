//! In-memory state: accounts, containers, blobs and everything stored on them.

use std::collections::{BTreeMap, HashMap};

use azemu_core::bytes::Bytes;
use azemu_core::time::OffsetDateTime;
use serde::{Deserialize, Serialize};

pub use crate::dfs::model::{DeletedPath, DfsAttrs};

pub type Time = OffsetDateTime;

/// Settings that are read once when the service starts. See `docs/services/blob.md` and `docs/SETTINGS.md`.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// `AZEMU_BLOB_VERSIONING`: versioning of accounts that appear on first use. Off as on Azure; on with
    /// `AZEMU_LENIENT=1` or when the variable itself says so.
    pub versioning: bool,
    /// `AZEMU_BLOB_VERSION_BYTES`: bytes the old versions of one account may hold.
    pub version_bytes: u64,
    /// `AZEMU_BLOB_SOFT_DELETE_BYTES`: bytes the soft-deleted snapshots of one account may hold.
    pub soft_snapshot_bytes: u64,
    /// Limits of Query Blob Contents (`AZEMU_BLOB_QUERY_*`, see `docs/services/blob-query.md`).
    pub query: crate::query::Limits,
    /// `AZEMU_BLOB_LOG_DELAY`: how long Storage Analytics log entries wait before they are written to `$logs`.
    pub log_delay: std::time::Duration,
    /// `AZEMU_BLOB_LOG_MAX_BLOBS`: log blobs one account keeps in `$logs` (the oldest go first).
    pub log_max_blobs: u64,
    /// `AZEMU_BLOB_LOG_MAX_BYTES`: bytes of log blobs one account keeps in `$logs`.
    pub log_max_bytes: u64,
}

/// Default for both byte limits: 256 MiB per account.
pub const DEFAULT_OLD_BYTES: u64 = 256 * 1024 * 1024;

impl Default for Config {
    /// The values used when nothing is set: versioning off, as on a new Azure storage account.
    fn default() -> Config {
        Config {
            versioning: false,
            version_bytes: DEFAULT_OLD_BYTES,
            soft_snapshot_bytes: DEFAULT_OLD_BYTES,
            query: crate::query::Limits::default(),
            log_delay: std::time::Duration::from_secs(3),
            log_max_blobs: 2000,
            log_max_bytes: 64 * 1024 * 1024,
        }
    }
}

impl Config {
    /// Read the settings (through `azemu_core::settings`) and describe them for `docs/SETTINGS.md`.
    pub fn from_env() -> Config {
        use azemu_core::settings;
        settings::describe(
            "AZEMU_BLOB_VERSIONING",
            "off (on with AZEMU_LENIENT)",
            "blob versioning for storage accounts that appear on first use; an account made through ARM follows its own setting",
        );
        settings::describe(
            "AZEMU_BLOB_VERSION_BYTES",
            "256M",
            "bytes of old blob versions one account keeps; older ones are dropped first (0 keeps none)",
        );
        settings::describe("AZEMU_BLOB_SOFT_DELETE_BYTES", "256M", "bytes of soft-deleted snapshots one account keeps (0 keeps none)");
        settings::describe(
            "AZEMU_BLOB_LOG_DELAY",
            "3s",
            "how long Storage Analytics log entries (Blob logging switched on with Set Blob Service Properties) wait before they are written to the `$logs` container; Azure takes up to an hour",
        );
        settings::describe("AZEMU_BLOB_LOG_MAX_BLOBS", "2000", "log blobs one account keeps in `$logs`; the oldest are removed first");
        settings::describe(
            "AZEMU_BLOB_LOG_MAX_BYTES",
            "64M",
            "bytes of log blobs one account keeps in `$logs`; the oldest are removed first",
        );
        let d = Config::default();
        Config {
            versioning: settings::lenient_flag("AZEMU_BLOB_VERSIONING"),
            version_bytes: settings::bytes("AZEMU_BLOB_VERSION_BYTES", d.version_bytes),
            soft_snapshot_bytes: settings::bytes("AZEMU_BLOB_SOFT_DELETE_BYTES", d.soft_snapshot_bytes),
            query: crate::query::Limits::from_env(),
            log_delay: settings::duration("AZEMU_BLOB_LOG_DELAY", d.log_delay),
            log_max_blobs: settings::number("AZEMU_BLOB_LOG_MAX_BLOBS", d.log_max_blobs),
            log_max_bytes: settings::bytes("AZEMU_BLOB_LOG_MAX_BYTES", d.log_max_bytes),
        }
    }
}

/// Everything the service stores. `reset()` replaces it with a fresh one (same settings).
pub struct State {
    pub config: Config,
    pub accounts: HashMap<String, Account>,
    /// Set by the service for the request being handled: does anyone listen to this
    /// account's events? When false, no event is built (see `events.rs`).
    pub events_wanted: bool,
    /// Events built while handling the request; the service takes them out after the lock
    /// is released.
    pub events: Vec<crate::events::StorageEvent>,
    /// Set by a handler that needs the data of a source blob put together and hashed outside
    /// the lock; the service does that and runs the request again (see `pre.rs`).
    pub wait_source: Option<SourceWait>,
    /// Set by Get Blob: the body to assemble (and hash) after the lock is released.
    pub out_body: Option<OutBody>,
    /// Set by Query Blob Contents: the query to run after the lock is released.
    pub query_job: Option<crate::query::QueryJob>,
    /// The request being handled changed something a save would write (set for every request that is not a
    /// read, and by the reads that do change state). Taken by `run_with_events`.
    pub changed: bool,
}

/// The data of a source blob (from-URL operations) in pieces, not yet joined into one buffer.
pub struct SourceWait {
    pub pieces: Vec<Piece>,
    pub blob: Blob,
    pub url: String,
}

/// A response body in pieces. Put together after the lock is released; `md5` and `crc64` ask for
/// the matching header of the finished body.
pub struct OutBody {
    pub pieces: Vec<Piece>,
    pub md5: bool,
    pub crc64: bool,
}

/// A part of a blob's content: stored bytes, or a run of zeros (page blob ranges nothing was
/// written to).
#[derive(Clone)]
pub enum Piece {
    Data(Bytes),
    Zeros(usize),
}

impl Piece {
    pub fn len(&self) -> usize {
        match self {
            Piece::Data(d) => d.len(),
            Piece::Zeros(n) => *n,
        }
    }
}

/// Join pieces into one buffer. One stored piece is returned as it is (no copy).
pub fn join(mut pieces: Vec<Piece>) -> Bytes {
    if pieces.len() == 1 && matches!(pieces[0], Piece::Data(_)) {
        if let Some(Piece::Data(d)) = pieces.pop() {
            return d;
        }
    }
    let total: usize = pieces.iter().map(Piece::len).sum();
    let mut out: Vec<u8> = Vec::with_capacity(total);
    for p in &pieces {
        match p {
            Piece::Data(d) => out.extend_from_slice(d),
            Piece::Zeros(n) => out.resize(out.len() + n, 0),
        }
    }
    Bytes::from(out)
}

impl Default for State {
    fn default() -> State {
        State::with_config(Config::from_env())
    }
}

impl State {
    pub fn with_config(config: Config) -> State {
        State {
            config,
            accounts: HashMap::new(),
            events_wanted: false,
            events: vec![],
            wait_source: None,
            out_body: None,
            query_job: None,
            changed: false,
        }
    }

    /// The account's state; created on first use.
    pub fn account(&mut self, name: &str) -> &mut Account {
        let config = self.config;
        self.accounts.entry(name.to_ascii_lowercase()).or_insert_with(|| Account::new(&config))
    }
}

/// Old copies of one kind that an account keeps (previous versions, or soft-deleted snapshots)
/// and the byte limit for them.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Pool {
    pub limit: u64,
    /// Upper bound of the bytes held since the last count (a copy that shares its buffers
    /// with another is counted in full). Only when it passes `limit` is the exact figure worked
    /// out (`store::trim`).
    pub est: u64,
}

/// What a write operation needs to know about the account's policy.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// Days soft-deleted items are kept, `None` when soft delete is off.
    pub retention: Option<u32>,
    /// New writes get a version id and the old state is kept as a version.
    pub versioning: bool,
    /// Old versions may be kept at all (the byte limit is not 0).
    pub keep_versions: bool,
    /// Soft-deleted snapshots may be kept at all.
    pub keep_snapshots: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    /// `isVersioningEnabled`: see `BlobService::set_versioning_enabled`.
    pub versioning: bool,
    pub version_pool: Pool,
    pub snapshot_pool: Pool,
    pub containers: BTreeMap<String, Container>,
    /// Soft-deleted containers (kept while the delete retention policy is on).
    pub deleted_containers: Vec<Container>,
    pub props: ServiceProps,
    /// Last timestamp handed out as a snapshot / version id.
    pub last_ts: Option<Time>,
    /// Storage Analytics logging has been switched on at some time: `$logs` cannot be deleted any more.
    #[serde(default)]
    pub logs_pinned: bool,
    /// The storage account allows public access to containers (`allowBlobPublicAccess` in the management API). Accounts
    /// that appear on first use allow it; one made through Resource Manager follows its own setting, `false` unless the
    /// request says otherwise, as on Azure. Where it is `false` a container cannot be made public and an anonymous
    /// request is refused with 409 `PublicAccessNotPermitted`.
    #[serde(default = "public_access_allowed_by_default")]
    pub public_access_allowed: bool,
}

fn public_access_allowed_by_default() -> bool {
    true
}

impl Default for Account {
    fn default() -> Account {
        Account::new(&Config::default())
    }
}

/// What a write needs to change versions and soft-deleted snapshots, split off an `Account` so
/// that its containers can be borrowed at the same time (see `Account::split`).
pub struct Ctl<'a> {
    pub last_ts: &'a mut Option<Time>,
    pub version_pool: &'a mut Pool,
    pub snapshot_pool: &'a mut Pool,
    pub policy: Policy,
}

impl Account {
    /// The containers and the version bookkeeping, borrowed side by side.
    pub fn split(&mut self) -> (&mut BTreeMap<String, Container>, Ctl<'_>) {
        let policy = self.policy();
        (
            &mut self.containers,
            Ctl { last_ts: &mut self.last_ts, version_pool: &mut self.version_pool, snapshot_pool: &mut self.snapshot_pool, policy },
        )
    }

    pub fn new(c: &Config) -> Account {
        Account {
            versioning: c.versioning,
            version_pool: Pool { limit: c.version_bytes, est: 0 },
            snapshot_pool: Pool { limit: c.soft_snapshot_bytes, est: 0 },
            containers: BTreeMap::new(),
            deleted_containers: vec![],
            props: ServiceProps::default(),
            last_ts: None,
            logs_pinned: false,
            public_access_allowed: true,
        }
    }

    pub fn policy(&self) -> Policy {
        Policy {
            retention: self.props.soft_delete_days(),
            versioning: self.versioning,
            keep_versions: self.version_pool.limit > 0,
            keep_snapshots: self.snapshot_pool.limit > 0,
        }
    }
}

/// A timestamp for snapshot / version ids that is strictly increasing in an account.
pub fn unique_time(slot: &mut Option<Time>, now: Time) -> Time {
    let mut t = now;
    if let Some(last) = *slot {
        if t <= last {
            t = last + azemu_core::time::Duration::nanoseconds(100);
        }
    }
    *slot = Some(t);
    t
}

// ---------------------------------------------------------------------------
// Service properties
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Retention {
    pub enabled: bool,
    pub days: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Logging {
    pub version: String,
    pub read: bool,
    pub write: bool,
    pub delete: bool,
    pub retention: Retention,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metrics {
    pub version: String,
    pub enabled: bool,
    pub include_apis: Option<bool>,
    pub retention: Retention,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CorsRule {
    pub allowed_origins: String,
    pub allowed_methods: String,
    pub allowed_headers: String,
    pub exposed_headers: String,
    pub max_age: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeleteRetention {
    pub enabled: bool,
    pub days: Option<u32>,
    pub allow_permanent_delete: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StaticWebsite {
    pub enabled: bool,
    pub index_document: Option<String>,
    pub error_document_404: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceProps {
    pub logging: Logging,
    pub hour_metrics: Metrics,
    pub minute_metrics: Metrics,
    pub cors: Vec<CorsRule>,
    pub default_service_version: Option<String>,
    pub delete_retention: DeleteRetention,
    pub static_website: StaticWebsite,
}

impl Default for ServiceProps {
    fn default() -> Self {
        ServiceProps {
            logging: Logging {
                version: "1.0".into(),
                read: false,
                write: false,
                delete: false,
                retention: Retention { enabled: false, days: None },
            },
            hour_metrics: Metrics {
                version: "1.0".into(),
                enabled: true,
                include_apis: Some(true),
                retention: Retention { enabled: true, days: Some(7) },
            },
            minute_metrics: Metrics {
                version: "1.0".into(),
                enabled: false,
                include_apis: None,
                retention: Retention { enabled: false, days: None },
            },
            cors: vec![],
            default_service_version: None,
            delete_retention: DeleteRetention::default(),
            static_website: StaticWebsite::default(),
        }
    }
}

impl ServiceProps {
    /// Days soft-deleted items are kept, `None` when soft delete is off.
    pub fn soft_delete_days(&self) -> Option<u32> {
        if self.delete_retention.enabled {
            Some(self.delete_retention.days.unwrap_or(7))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Leases
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseState {
    Available,
    Leased,
    Expired,
    Breaking,
    Broken,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lease {
    pub id: Option<String>,
    pub state: LeaseState,
    pub infinite: bool,
    /// Fixed lease length in seconds (renew starts it again).
    pub secs: u32,
    /// Fixed lease: when it expires. Breaking: when it is broken.
    pub expires: Option<Time>,
}

impl Default for Lease {
    fn default() -> Self {
        Lease { id: None, state: LeaseState::Available, infinite: false, secs: 0, expires: None }
    }
}

impl Lease {
    /// Apply the passage of time.
    pub fn refresh(&mut self, now: Time) {
        match self.state {
            LeaseState::Leased if !self.infinite => {
                if let Some(e) = self.expires {
                    if e <= now {
                        self.state = LeaseState::Expired;
                        self.expires = None;
                    }
                }
            }
            LeaseState::Breaking => {
                if let Some(e) = self.expires {
                    if e <= now {
                        self.state = LeaseState::Broken;
                        self.expires = None;
                    }
                }
            }
            _ => {}
        }
    }

    /// True while the lease locks the resource (leased or breaking).
    pub fn locked(&self) -> bool {
        matches!(self.state, LeaseState::Leased | LeaseState::Breaking)
    }

    pub fn state_str(&self) -> &'static str {
        match self.state {
            LeaseState::Available => "available",
            LeaseState::Leased => "leased",
            LeaseState::Expired => "expired",
            LeaseState::Breaking => "breaking",
            LeaseState::Broken => "broken",
        }
    }

    pub fn status_str(&self) -> &'static str {
        if self.locked() {
            "locked"
        } else {
            "unlocked"
        }
    }

    /// `infinite` / `fixed` while leased.
    pub fn duration_str(&self) -> Option<&'static str> {
        if self.state == LeaseState::Leased {
            Some(if self.infinite { "infinite" } else { "fixed" })
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedId {
    pub id: String,
    pub start: Option<String>,
    pub expiry: Option<String>,
    pub permission: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Container {
    pub name: String,
    pub etag: String,
    pub last_modified: Time,
    pub metadata: Vec<(String, String)>,
    /// `blob` or `container`.
    pub public_access: Option<String>,
    pub policies: Vec<SignedId>,
    pub lease: Lease,
    pub blobs: BTreeMap<String, BlobEntry>,
    pub default_scope: String,
    pub deny_scope_override: bool,
    /// Set while the container is soft deleted: (deleted at, version id).
    pub deleted: Option<(Time, String)>,
    /// Hierarchical namespace (Data Lake Gen2): switched on the first time the container is
    /// touched through the `dfs` endpoint. See `dfs/model.rs`.
    pub hns: bool,
    /// Something was written through the blob endpoint that is not known to the Data Lake
    /// side yet: every blob has to be looked at (`dirty`) or just the names in `touched`.
    pub dirty: bool,
    pub touched: Vec<String>,
    /// Attributes (owner, ACL, ...) of the root directory of a hierarchical container.
    pub root: Option<Box<DfsAttrs>>,
    /// Paths deleted through the `dfs` endpoint while soft delete is on.
    pub deleted_paths: Vec<DeletedPath>,
}

impl Container {
    pub fn new(name: &str, now: Time, etag: String) -> Container {
        Container {
            name: name.to_string(),
            etag,
            last_modified: now,
            metadata: vec![],
            public_access: None,
            policies: vec![],
            lease: Lease::default(),
            blobs: BTreeMap::new(),
            default_scope: "$account-encryption-key".into(),
            deny_scope_override: false,
            deleted: None,
            hns: false,
            dirty: false,
            touched: vec![],
            root: None,
            deleted_paths: vec![],
        }
    }

    /// Drop the entry for `name` when nothing is left in it.
    pub fn prune(&mut self, name: &str) {
        if let Some(e) = self.blobs.get(name) {
            if e.is_empty() {
                self.blobs.remove(name);
            }
        }
    }

    pub fn touch(&mut self, now: Time) {
        self.etag = azemu_core::etag::new_etag();
        self.last_modified = now;
    }
}

// ---------------------------------------------------------------------------
// Blobs
// ---------------------------------------------------------------------------

/// Everything stored under one blob name.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct BlobEntry {
    /// The base blob (may be soft deleted).
    pub current: Option<Blob>,
    /// Snapshots by id. Ids sort in time order.
    pub snapshots: BTreeMap<String, Blob>,
    /// Previous versions, oldest first.
    pub versions: Vec<Blob>,
    /// Staged blocks that are not committed yet, by block id.
    #[serde(with = "crate::disk::piece_map")]
    pub uncommitted: BTreeMap<String, Bytes>,
}

impl BlobEntry {
    pub fn is_empty(&self) -> bool {
        self.current.is_none() && self.snapshots.is_empty() && self.versions.is_empty() && self.uncommitted.is_empty()
    }

    /// The base blob unless it does not exist or is soft deleted.
    pub fn live(&self) -> Option<&Blob> {
        self.current.as_ref().filter(|b| b.deleted.is_none())
    }

    pub fn live_mut(&mut self) -> Option<&mut Blob> {
        self.current.as_mut().filter(|b| b.deleted.is_none())
    }

    /// Look up the instance a request addresses: a snapshot, a version or the base blob.
    pub fn find(&self, snapshot: Option<&str>, version: Option<&str>) -> Option<&Blob> {
        if let Some(s) = snapshot {
            return self.snapshots.get(s).filter(|b| b.deleted.is_none());
        }
        if let Some(v) = version {
            if let Some(c) = self.live() {
                if c.version_id.as_deref() == Some(v) {
                    return Some(c);
                }
            }
            return self.versions.iter().find(|b| b.version_id.as_deref() == Some(v) && b.deleted.is_none());
        }
        self.live()
    }

    pub fn find_mut(&mut self, snapshot: Option<&str>, version: Option<&str>) -> Option<&mut Blob> {
        if let Some(s) = snapshot {
            return self.snapshots.get_mut(s).filter(|b| b.deleted.is_none());
        }
        if let Some(v) = version {
            if self.live().and_then(|c| c.version_id.as_deref()) == Some(v) {
                return self.current.as_mut();
            }
            return self.versions.iter_mut().find(|b| b.version_id.as_deref() == Some(v) && b.deleted.is_none());
        }
        self.live_mut()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobKind {
    Block,
    Append,
    Page,
}

impl BlobKind {
    pub fn name(self) -> &'static str {
        match self {
            BlobKind::Block => "BlockBlob",
            BlobKind::Append => "AppendBlob",
            BlobKind::Page => "PageBlob",
        }
    }
}

/// A block of a block or append blob.
#[derive(Clone, Serialize, Deserialize)]
pub struct Seg {
    /// Block id (block blobs); `None` for data written by Put Blob and for append blocks.
    pub id: Option<String>,
    #[serde(with = "crate::disk::piece")]
    pub data: Bytes,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Props {
    pub content_type: String,
    pub content_encoding: Option<String>,
    pub content_language: Option<String>,
    pub content_disposition: Option<String>,
    pub cache_control: Option<String>,
    pub content_md5: Option<Vec<u8>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CopyInfo {
    pub id: String,
    pub source: String,
    pub status: String,
    pub progress: String,
    pub completed: Time,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Immutability {
    pub until: Time,
    pub mode: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Blob {
    pub kind: BlobKind,
    /// Block and append blob content.
    pub segs: Vec<Seg>,
    /// Page blob content.
    pub pages: PageStore,
    /// Content length (declared size for page blobs).
    pub size: u64,
    pub etag: String,
    pub last_modified: Time,
    pub creation_time: Time,
    pub props: Props,
    pub crc64: Option<u64>,
    pub metadata: Vec<(String, String)>,
    pub tags: BTreeMap<String, String>,
    /// Explicitly set access tier (`Hot`, `Cool`, `Cold`, `Archive`, `P10`, ...).
    pub tier: Option<String>,
    pub tier_changed: Option<Time>,
    /// `rehydrate-pending-to-hot` / `rehydrate-pending-to-cool`.
    pub archive_status: Option<String>,
    pub rehydrate_priority: Option<String>,
    pub lease: Lease,
    pub copy: Option<CopyInfo>,
    pub sequence: u64,
    pub sealed: bool,
    pub snapshot: Option<String>,
    pub version_id: Option<String>,
    pub deleted: Option<Time>,
    pub legal_hold: bool,
    pub immutability: Option<Immutability>,
    pub encryption_scope: Option<String>,
    /// SHA-256 of the customer-provided key the blob was written with.
    pub cpk_sha256: Option<String>,
    /// Hierarchical namespace attributes; set on every blob of a hierarchical container.
    pub dfs: Option<Box<DfsAttrs>>,
}

impl Blob {
    pub fn new(kind: BlobKind, now: Time) -> Blob {
        Blob {
            kind,
            segs: vec![],
            pages: PageStore::default(),
            size: 0,
            etag: azemu_core::etag::new_etag(),
            last_modified: now,
            creation_time: now,
            props: Props { content_type: "application/octet-stream".into(), ..Props::default() },
            crc64: None,
            metadata: vec![],
            tags: BTreeMap::new(),
            tier: None,
            tier_changed: None,
            archive_status: None,
            rehydrate_priority: None,
            lease: Lease::default(),
            copy: None,
            sequence: 0,
            sealed: false,
            snapshot: None,
            version_id: None,
            deleted: None,
            legal_hold: false,
            immutability: None,
            encryption_scope: None,
            cpk_sha256: None,
            dfs: None,
        }
    }

    pub fn touch(&mut self, now: Time) {
        self.etag = azemu_core::etag::new_etag();
        self.last_modified = now;
    }

    /// `(tier, inferred)` as reported to clients; `None` for blobs without a tier.
    pub fn tier_info(&self) -> Option<(String, bool)> {
        match (&self.tier, self.kind) {
            (Some(t), _) => Some((t.clone(), false)),
            (None, BlobKind::Block) => Some(("Hot".to_string(), true)),
            _ => None,
        }
    }

    /// Number of committed blocks (append blobs: appended blocks).
    pub fn block_count(&self) -> usize {
        self.segs.len()
    }

    /// Read `len` bytes at `off` (already validated to be inside the blob).
    pub fn read(&self, off: u64, len: u64) -> Bytes {
        join(self.read_pieces(off, len))
    }

    /// The same bytes as `read`, not joined: the stored buffers (no copy) and runs of zeros. Use it
    /// when a lock is held and join after releasing it.
    pub fn read_pieces(&self, off: u64, len: u64) -> Vec<Piece> {
        if len == 0 {
            return vec![];
        }
        match self.kind {
            BlobKind::Page => self.pages.read_pieces(off, len),
            _ => {
                let mut out = vec![];
                let mut pos = 0u64;
                let end = off + len;
                for s in &self.segs {
                    let (a, b) = (pos, pos + s.data.len() as u64);
                    pos = b;
                    if b <= off {
                        continue;
                    }
                    if a >= end {
                        break;
                    }
                    let from = off.max(a) - a;
                    let to = end.min(b) - a;
                    out.push(Piece::Data(s.data.slice(from as usize..to as usize)));
                }
                out
            }
        }
    }

    /// Every stored buffer of the content as (address, length). Two blobs that were cloned from
    /// one another share their buffers (`Bytes` is reference counted), and report the same pairs.
    pub fn buffers(&self) -> Vec<(usize, usize)> {
        let mut v: Vec<(usize, usize)> = self.segs.iter().map(|s| (s.data.as_ptr() as usize, s.data.len())).collect();
        v.extend(self.pages.buffers());
        v.retain(|b| b.1 > 0);
        v
    }

    /// Bytes of content held, counting shared buffers in full.
    pub fn stored_bytes(&self) -> u64 {
        self.segs.iter().map(|s| s.data.len() as u64).sum::<u64>() + self.pages.stored_bytes()
    }

    /// Checks used when state is read from disk: the content agrees with the declared size.
    pub fn check_consistent(&self) -> Result<(), String> {
        match self.kind {
            BlobKind::Page => self.pages.check(self.size),
            _ => {
                let total: u64 = self.segs.iter().map(|s| s.data.len() as u64).sum();
                if total == self.size {
                    Ok(())
                } else {
                    Err(format!("its blocks hold {total} bytes but its size is {}", self.size))
                }
            }
        }
    }

    pub fn read_all(&self) -> Bytes {
        self.read(0, self.size)
    }

    /// Replace the content of a block / append blob with one unnamed block.
    pub fn set_content(&mut self, data: Bytes) {
        self.size = data.len() as u64;
        self.segs = if data.is_empty() { vec![] } else { vec![Seg { id: None, data }] };
    }
}

// ---------------------------------------------------------------------------
// Page blob content
// ---------------------------------------------------------------------------

/// Sparse page blob content: non-overlapping extents keyed by start offset.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct PageStore {
    #[serde(with = "crate::disk::piece_map")]
    ext: BTreeMap<u64, Bytes>,
}

impl PageStore {
    pub fn buffers(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.ext.values().map(|v| (v.as_ptr() as usize, v.len()))
    }

    pub fn stored_bytes(&self) -> u64 {
        self.ext.values().map(|v| v.len() as u64).sum()
    }

    /// Every extent lies inside `[0, size)` and no two overlap.
    pub fn check(&self, size: u64) -> Result<(), String> {
        let mut prev_end = 0u64;
        for (k, v) in &self.ext {
            let end = k.checked_add(v.len() as u64).ok_or("a page range is impossibly large")?;
            if *k < prev_end || end > size {
                return Err(format!("page range {k}..{end} overlaps another or is outside the blob of {size} bytes"));
            }
            prev_end = end;
        }
        Ok(())
    }

    /// Remove everything in `[start, end)`.
    pub fn clear(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let mut hit: Vec<u64> = vec![];
        if let Some((k, v)) = self.ext.range(..start).next_back() {
            if k + v.len() as u64 > start {
                hit.push(*k);
            }
        }
        hit.extend(self.ext.range(start..end).map(|(k, _)| *k));
        for k in hit {
            let Some(data) = self.ext.remove(&k) else { continue };
            let ext_end = k + data.len() as u64;
            if k < start {
                self.ext.insert(k, data.slice(..(start - k) as usize));
            }
            if ext_end > end {
                self.ext.insert(end, data.slice((end - k) as usize..));
            }
        }
    }

    pub fn write(&mut self, offset: u64, data: Bytes) {
        if data.is_empty() {
            return;
        }
        self.clear(offset, offset + data.len() as u64);
        self.ext.insert(offset, data);
    }

    pub fn truncate(&mut self, size: u64) {
        self.clear(size, u64::MAX);
    }

    /// Bytes of `[off, off + len)`, zeros where nothing was written.
    pub fn read(&self, off: u64, len: u64) -> Bytes {
        join(self.read_pieces(off, len))
    }

    /// Like `read`, in pieces: the stored extents (no copy) and the gaps as runs of zeros.
    pub fn read_pieces(&self, off: u64, len: u64) -> Vec<Piece> {
        let end = off + len;
        let mut out = vec![];
        let mut pos = off;
        let first = self.ext.range(..=off).next_back().filter(|(k, v)| *k + v.len() as u64 > off);
        for (k, v) in first.into_iter().chain(self.ext.range(off + 1..end)) {
            let (s, e) = ((*k).max(off), (*k + v.len() as u64).min(end));
            if s >= e {
                continue;
            }
            if s > pos {
                out.push(Piece::Zeros((s - pos) as usize));
            }
            out.push(Piece::Data(v.slice((s - k) as usize..(e - k) as usize)));
            pos = e;
        }
        if pos < end {
            out.push(Piece::Zeros((end - pos) as usize));
        }
        out
    }

    /// Written ranges inside `[from, to]` (inclusive), adjacent ones merged.
    pub fn ranges(&self, from: u64, to: u64) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = vec![];
        for (k, v) in &self.ext {
            let s = *k;
            let e = k + v.len() as u64 - 1;
            if e < from || s > to {
                continue;
            }
            let (s, e) = (s.max(from), e.min(to));
            match out.last_mut() {
                Some(last) if last.1 + 1 == s => last.1 = e,
                _ => out.push((s, e)),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_store_write_clear_read() {
        let mut p = PageStore::default();
        p.write(512, Bytes::from(vec![1u8; 1024]));
        assert_eq!(p.ranges(0, 4095), vec![(512, 1535)]);
        p.clear(1024, 1536);
        assert_eq!(p.ranges(0, 4095), vec![(512, 1023)]);
        let r = p.read(0, 1024);
        assert_eq!(&r[..512], &[0u8; 512][..]);
        assert_eq!(&r[512..], &[1u8; 512][..]);
        p.write(0, Bytes::from(vec![2u8; 512]));
        assert_eq!(p.ranges(0, 4095), vec![(0, 1023)]);
        p.truncate(512);
        assert_eq!(p.ranges(0, 4095), vec![(0, 511)]);
    }

    #[test]
    fn lease_expires_and_breaks() {
        let now = azemu_core::time::OffsetDateTime::UNIX_EPOCH;
        let mut l = Lease {
            id: Some("x".into()),
            state: LeaseState::Leased,
            infinite: false,
            secs: 15,
            expires: Some(now + azemu_core::time::Duration::seconds(15)),
        };
        l.refresh(now + azemu_core::time::Duration::seconds(14));
        assert_eq!(l.state, LeaseState::Leased);
        l.refresh(now + azemu_core::time::Duration::seconds(15));
        assert_eq!(l.state, LeaseState::Expired);
    }
}
