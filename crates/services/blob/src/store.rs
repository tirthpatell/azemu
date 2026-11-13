//! Shared write helpers: reading the properties a request sets on a new blob, and
//! installing / versioning blobs inside an entry.

use std::collections::{BTreeMap, HashMap};

use azemu_core::time::{self, parse_rfc1123};
use azemu_core::Request;

use crate::errors::{err, ApiError};
use crate::state::{unique_time, Account, Blob, BlobEntry, BlobKind, Container, Ctl, Immutability, Props, Time};
use crate::util::{b64_decode, hdr, hdr_true, parse_metadata, parse_tags_header};

/// Everything a "create a blob" request says about the new blob.
pub struct NewSpec {
    pub props: Props,
    pub metadata: Vec<(String, String)>,
    pub tags: BTreeMap<String, String>,
    pub tier: Option<String>,
    pub legal_hold: bool,
    pub immutability: Option<Immutability>,
    pub scope: Option<String>,
    pub cpk: Option<String>,
}

pub fn valid_tier(kind: BlobKind, t: &str) -> Option<String> {
    let canon = |names: &[&str]| names.iter().find(|n| n.eq_ignore_ascii_case(t)).map(|n| n.to_string());
    match kind {
        BlobKind::Block => canon(&["Hot", "Cool", "Cold", "Archive", "Smart", "Premium"]),
        BlobKind::Page => canon(&["P1", "P2", "P3", "P4", "P6", "P10", "P15", "P20", "P30", "P40", "P50", "P60", "P70", "P80"]),
        BlobKind::Append => None,
    }
}

/// Read the content properties, metadata, tags, tier and friends from the request.
/// `body_headers`: also honour the plain `Content-Type` / `Content-Encoding` / ... headers
/// (Put Blob does, Put Block List does not).
pub fn new_spec(req: &Request, kind: BlobKind, body_headers: bool) -> Result<NewSpec, ApiError> {
    let pick = |x: &str, plain: &str| -> Option<String> {
        hdr(req, x).map(str::to_string).or_else(|| if body_headers { hdr(req, plain).map(str::to_string) } else { None })
    };
    let nonempty = |v: Option<String>| v.filter(|s| !s.is_empty());
    let mut props = Props {
        content_type: pick("x-ms-blob-content-type", "content-type").unwrap_or_else(|| "application/octet-stream".into()),
        content_encoding: nonempty(pick("x-ms-blob-content-encoding", "content-encoding")),
        content_language: nonempty(pick("x-ms-blob-content-language", "content-language")),
        content_disposition: nonempty(hdr(req, "x-ms-blob-content-disposition").map(str::to_string)),
        cache_control: nonempty(pick("x-ms-blob-cache-control", "cache-control")),
        content_md5: None,
    };
    if props.content_type.is_empty() {
        props.content_type = "application/octet-stream".into();
    }
    if let Some(v) = hdr(req, "x-ms-blob-content-md5") {
        match b64_decode(v) {
            Some(d) => props.content_md5 = Some(d),
            None => return Err(ApiError::invalid_header("x-ms-blob-content-md5", v)),
        }
    }
    let tier = match hdr(req, "x-ms-access-tier") {
        None => None,
        Some(t) => Some(valid_tier(kind, t).ok_or_else(|| ApiError::invalid_header("x-ms-access-tier", t))?),
    };
    let immutability = match hdr(req, "x-ms-immutability-policy-until-date") {
        None => None,
        Some(d) => {
            let until = parse_rfc1123(d).ok_or_else(|| ApiError::invalid_header("x-ms-immutability-policy-until-date", d))?;
            let mode = hdr(req, "x-ms-immutability-policy-mode").unwrap_or("unlocked");
            Some(Immutability { until, mode: mode.to_ascii_lowercase() })
        }
    };
    crate::cpk::require_key_with_hash(req)?;
    Ok(NewSpec {
        props,
        metadata: parse_metadata(req)?,
        tags: parse_tags_header(req)?,
        tier,
        legal_hold: hdr_true(req, "x-ms-legal-hold"),
        immutability,
        scope: hdr(req, "x-ms-encryption-scope").map(str::to_string),
        cpk: hdr(req, "x-ms-encryption-key-sha256").map(str::to_string),
    })
}

impl NewSpec {
    /// A blob of `kind` carrying these properties.
    pub fn build(self, kind: BlobKind, now: Time) -> Blob {
        let mut b = Blob::new(kind, now);
        b.props = self.props;
        b.metadata = self.metadata;
        b.tags = self.tags;
        if let Some(t) = self.tier {
            b.tier = Some(t);
            b.tier_changed = Some(now);
        }
        b.legal_hold = self.legal_hold;
        b.immutability = self.immutability;
        b.encryption_scope = self.scope;
        b.cpk_sha256 = self.cpk;
        b
    }
}

/// The version / snapshot id for `now`, unique in the account.
pub fn new_id(last_ts: &mut Option<Time>, now: Time) -> String {
    time::iso8601_7(unique_time(last_ts, now))
}

/// Previous versions kept per blob name; the oldest are dropped beyond this.
pub const MAX_VERSIONS: usize = 500;
/// Soft-deleted snapshots (made when a blob is overwritten) kept per blob name.
pub const MAX_SOFT_SNAPSHOTS: usize = 500;

/// Keep `old` as a previous version of the blob, unless the account keeps none.
fn keep_version(ctl: &mut Ctl, entry: &mut BlobEntry, old: Blob) {
    if !ctl.policy.keep_versions {
        return;
    }
    ctl.version_pool.est += old.stored_bytes();
    entry.versions.push(old);
    if entry.versions.len() > MAX_VERSIONS {
        entry.versions.remove(0);
    }
}

/// The base blob was deleted: with versions, its content stays as a previous version.
pub fn keep_deleted(ctl: &mut Ctl, entry: &mut BlobEntry, mut cur: Blob) {
    if cur.version_id.is_some() {
        cur.lease = Default::default();
        keep_version(ctl, entry, cur);
    }
}

/// Put `new` in place of the base blob. The old base blob is kept as a version (when it has a
/// version id), and as a soft-deleted snapshot when soft delete is on. A lease on the old blob
/// carries over. The new blob gets a version id only when the account has versioning on.
pub fn install(ctl: &mut Ctl, entry: &mut BlobEntry, mut new: Blob, now: Time) {
    if let Some(mut old) = entry.current.take() {
        if old.deleted.is_none() {
            new.lease = old.lease.clone();
            if new.dfs.is_none() {
                new.dfs = old.dfs.clone();
            }
            if ctl.policy.retention.is_some() && ctl.policy.keep_snapshots {
                let mut snap = old.clone();
                let id = new_id(ctl.last_ts, now);
                snap.snapshot = Some(id.clone());
                snap.deleted = Some(now);
                snap.lease = Default::default();
                ctl.snapshot_pool.est += snap.stored_bytes();
                entry.snapshots.insert(id, snap);
                cap_soft_snapshots(entry);
            }
        }
        if old.version_id.is_some() {
            old.deleted = None;
            old.lease = Default::default();
            keep_version(ctl, entry, old);
        }
    }
    new.version_id = ctl.policy.versioning.then(|| new_id(ctl.last_ts, now));
    new.snapshot = None;
    new.deleted = None;
    entry.current = Some(new);
    entry.uncommitted.clear();
}

/// At most `MAX_SOFT_SNAPSHOTS` soft-deleted snapshots per blob name, oldest dropped first.
fn cap_soft_snapshots(entry: &mut BlobEntry) {
    if entry.snapshots.len() <= MAX_SOFT_SNAPSHOTS {
        return;
    }
    let mut extra = entry.snapshots.values().filter(|s| s.deleted.is_some()).count().saturating_sub(MAX_SOFT_SNAPSHOTS);
    entry.snapshots.retain(|_, s| {
        if extra > 0 && s.deleted.is_some() {
            extra -= 1;
            false
        } else {
            true
        }
    });
}

/// Before changing the base blob in place: keep its present state as a version and give
/// the blob a new version id. Without versioning the blob is changed in place; if it still
/// carries a version id from when versioning was on, that state stays as a previous version
/// and the blob stops being a version.
pub fn roll_version(ctl: &mut Ctl, entry: &mut BlobEntry, now: Time) {
    let id = ctl.policy.versioning.then(|| new_id(ctl.last_ts, now));
    let Some(cur) = entry.current.as_mut() else { return };
    if cur.version_id.is_some() {
        let mut old = cur.clone();
        old.lease = Default::default();
        cur.version_id = id;
        keep_version(ctl, entry, old);
    } else {
        cur.version_id = id;
    }
}

/// Keep the old copies of the account (previous versions, soft-deleted snapshots) inside their
/// byte limits, oldest first. Called after every write; it only does work when the running
/// estimate says a limit may have been passed.
pub fn trim(acct: &mut Account) {
    if acct.version_pool.est > acct.version_pool.limit {
        trim_pool(acct, false);
    }
    if acct.snapshot_pool.est > acct.snapshot_pool.limit {
        trim_pool(acct, true);
    }
}

/// One old copy: where it is and which buffers it holds.
struct Old {
    id: String,
    container: String,
    name: String,
    bufs: Vec<(usize, usize)>,
}

/// Work out the bytes held (each stored buffer counted once, however many copies share it),
/// and if that is over the limit drop the oldest copies until it is below the limit by a margin.
fn trim_pool(acct: &mut Account, snapshots: bool) {
    let limit = if snapshots { acct.snapshot_pool.limit } else { acct.version_pool.limit };
    let mut items: Vec<Old> = vec![];
    for (cn, c) in &acct.containers {
        for (bn, e) in &c.blobs {
            if snapshots {
                for (id, s) in e.snapshots.iter().filter(|(_, s)| s.deleted.is_some()) {
                    items.push(Old { id: id.clone(), container: cn.clone(), name: bn.clone(), bufs: s.buffers() });
                }
            } else {
                for v in &e.versions {
                    items.push(Old {
                        id: v.version_id.clone().unwrap_or_default(),
                        container: cn.clone(),
                        name: bn.clone(),
                        bufs: v.buffers(),
                    });
                }
            }
        }
    }
    let mut refs: HashMap<(usize, usize), u32> = HashMap::new();
    let mut held: u64 = 0;
    for it in &items {
        for b in &it.bufs {
            let n = refs.entry(*b).or_insert(0);
            if *n == 0 {
                held += b.1 as u64;
            }
            *n += 1;
        }
    }
    let pool = if snapshots { &mut acct.snapshot_pool } else { &mut acct.version_pool };
    if held <= limit {
        pool.est = held;
        return;
    }
    // Drop down to a little below the limit, so a stream of small writes does not trigger
    // a count on every request.
    let target = limit - (limit / 10).min(8 * 1024 * 1024);
    // Ids are time stamps of one fixed width: sorted as text they are in time order.
    items.sort_by(|a, b| a.id.cmp(&b.id));
    for it in items {
        if held <= target {
            break;
        }
        let Some(c) = acct.containers.get_mut(&it.container) else { continue };
        if let Some(e) = c.blobs.get_mut(&it.name) {
            if snapshots {
                e.snapshots.remove(&it.id);
            } else {
                e.versions.retain(|v| v.version_id.as_deref() != Some(it.id.as_str()));
            }
        }
        c.prune(&it.name);
        for b in &it.bufs {
            if let Some(n) = refs.get_mut(b) {
                *n -= 1;
                if *n == 0 {
                    held -= b.1 as u64;
                }
            }
        }
    }
    let pool = if snapshots { &mut acct.snapshot_pool } else { &mut acct.version_pool };
    pool.est = held;
}

/// Reject writes to a blob that is immutable.
pub fn check_mutable(b: &Blob, now: Time) -> Result<(), ApiError> {
    if b.legal_hold {
        return Err(err("BlobImmutableDueToLegalHold"));
    }
    if let Some(i) = &b.immutability {
        if i.until > now {
            return Err(err("BlobImmutableDueToPolicy"));
        }
    }
    Ok(())
}

/// A blob written with a customer-provided key can only be read with it.
pub fn check_cpk(b: &Blob, req: &Request) -> Result<(), ApiError> {
    let Some(stored) = &b.cpk_sha256 else { return Ok(()) };
    match hdr(req, "x-ms-encryption-key-sha256") {
        None => Err(err("BlobUsesCustomerSpecifiedEncryption")),
        // the hash of the key in the request is the hash of the key the blob was written with
        Some(given) if given.trim() == stored => Ok(()),
        Some(_) => Err(err("BlobUsesCustomerSpecifiedEncryption")
            .msg("The blob is encrypted with customer specified encryption, but the key provided in the request is not the one it was written with.")),
    }
}

/// A blob written with an explicit encryption scope needs the scope on writes too.
pub fn check_scope_write(b: &Blob, req: &Request) -> Result<(), ApiError> {
    if b.encryption_scope.is_some() && hdr(req, "x-ms-encryption-scope").is_none() {
        return Err(err("BlobUsesCustomerSpecifiedEncryption"));
    }
    Ok(())
}

/// A container that denies encryption scope overrides only takes its default scope.
pub fn check_container_scope(c: &Container, req: &Request) -> Result<(), ApiError> {
    if c.deny_scope_override {
        if let Some(s) = hdr(req, "x-ms-encryption-scope") {
            if s != c.default_scope {
                return Err(err("RequestForbiddenByContainerEncryptionPolicy")
                    .status(403)
                    .msg("The request is forbidden by the container encryption policy."));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Config;
    use azemu_core::bytes::Bytes;

    fn now() -> Time {
        azemu_core::time::OffsetDateTime::UNIX_EPOCH
    }

    fn overwrite(acct: &mut Account, name: &str, data: Vec<u8>) {
        let (containers, mut ctl) = acct.split();
        let c = containers.entry("c".into()).or_insert_with(|| Container::new("c", now(), "e".into()));
        let entry = c.blobs.entry(name.to_string()).or_default();
        let mut b = Blob::new(BlobKind::Block, now());
        b.set_content(Bytes::from(data));
        install(&mut ctl, entry, b, now());
        trim(acct);
    }

    #[test]
    fn soft_deleted_snapshots_have_their_own_limit() {
        let config = Config { versioning: false, version_bytes: 1 << 20, soft_snapshot_bytes: 1000, ..Config::default() };
        let mut acct = Account::new(&config);
        acct.props.delete_retention.enabled = true;
        for i in 0..20u8 {
            overwrite(&mut acct, "b", vec![i; 600]);
        }
        let e = &acct.containers["c"].blobs["b"];
        assert!(e.versions.is_empty(), "no versioning, so no versions");
        let kept: Vec<_> = e.snapshots.values().filter(|s| s.deleted.is_some()).collect();
        assert_eq!(kept.len(), 1, "600 bytes each, 1000 allowed");
        assert_eq!(kept[0].read_all().as_ref(), &[18u8; 600][..], "the newest old copy stays");
        assert_eq!(e.current.as_ref().map(|b| b.size), Some(600));
    }

    #[test]
    fn count_per_blob_is_capped_even_for_empty_blobs() {
        let config = Config { versioning: true, version_bytes: u64::MAX, soft_snapshot_bytes: u64::MAX, ..Config::default() };
        let mut acct = Account::new(&config);
        acct.props.delete_retention.enabled = true;
        for _ in 0..MAX_VERSIONS + 50 {
            overwrite(&mut acct, "b", vec![]);
        }
        let e = &acct.containers["c"].blobs["b"];
        assert_eq!(e.versions.len(), MAX_VERSIONS);
        assert_eq!(e.snapshots.len(), MAX_SOFT_SNAPSHOTS);
    }

    #[test]
    fn limits_are_per_account_total_across_blobs() {
        let config = Config { versioning: true, version_bytes: 2500, soft_snapshot_bytes: 0, ..Config::default() };
        let mut acct = Account::new(&config);
        for round in 0..5u8 {
            for name in ["a", "b", "c"] {
                overwrite(&mut acct, name, vec![round; 1000]);
            }
        }
        let old: usize = acct.containers["c"].blobs.values().map(|e| e.versions.len()).sum();
        assert!((1..=2).contains(&old), "{old} old versions of 1000 bytes under a 2500 byte limit");
        // The oldest across blobs went first: what is left comes from the last rounds.
        for e in acct.containers["c"].blobs.values() {
            for v in &e.versions {
                assert!(v.read_all()[0] >= 3);
            }
        }
    }
}
