//! Saving the state to disk and reading it back (`--data-dir`). The reasons are in `docs/services/persistence.md`
//! (section "ARM").
//!
//! Layout of the service directory:
//!
//! * `arm.state` (kind `arm`): the subscriptions that were used, the resource groups (location, tags, managed-by),
//!   the tags set on scopes and the tag names. Small; written again only when its content changed.
//! * `scope-<hash>.state` (kind `arm-resources`): the resources of one resource group (or, for what lives outside a
//!   group, of one subscription): the JSON the client sees, the provider-private data (`extra`: key generations,
//!   links to the data plane, deployment templates) and the time each was created. This includes the deployments and
//!   their history, which is already bounded (800 per scope, 64 MB per subscription). Written again only when that
//!   scope changed since the last save.
//!
//! ARM and the data-plane services are saved at slightly different moments, so after a restart they can disagree about
//! a resource. ARM does not try to settle it: it lists what it created ("ARM lists what ARM created"), and a GET of an
//! ARM resource whose data-plane object is gone still answers from what ARM stored. A data-plane object that ARM does
//! not know is not listed by ARM. Nothing at load reaches into the data plane.
//!
//! Not saved: writes that were running and groups that were being deleted (`writers`, `deleting`): a delete that did not
//! finish leaves the group as it was when the save was taken.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::Path;

use azemu_core::persist;
use azemu_core::time::OffsetDateTime;
use azemu_statefile as statefile;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::state::{current_revision, scope_of, Group, Stored};
use crate::ArmService;

pub const DEFS_FILE: &str = "arm.state";
pub const DEFS_KIND: &str = "arm";
pub const SCOPE_KIND: &str = "arm-resources";
pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct GroupD {
    key: String,
    sub: String,
    name: String,
    location: String,
    tags: Map<String, Value>,
    managed_by: Option<String>,
}

/// The values used for each tag name of a subscription.
type TagNames = Vec<(String, Vec<String>)>;

#[derive(Serialize, Deserialize, Default)]
struct DefsFile {
    subs: Vec<String>,
    groups: Vec<GroupD>,
    scope_tags: Vec<(String, Map<String, Value>)>,
    tag_names: Vec<(String, TagNames)>,
}

#[derive(Serialize, Deserialize)]
struct StoredD {
    key: String,
    id: String,
    sub: String,
    rg: String,
    ns: String,
    types: Vec<String>,
    names: Vec<String>,
    body: Value,
    extra: Value,
    created_ns: i64,
}

#[derive(Serialize, Deserialize)]
struct ScopeFile {
    scope: String,
    resources: Vec<StoredD>,
}

fn file_name(scope: &str) -> String {
    statefile::hashed_name("scope", &[scope])
}

fn stored_d(key: &str, s: &Stored) -> StoredD {
    StoredD {
        key: key.to_string(),
        id: s.id.clone(),
        sub: s.sub.clone(),
        rg: s.rg.clone(),
        ns: s.ns.clone(),
        types: s.types.clone(),
        names: s.names.clone(),
        body: s.body.clone(),
        extra: s.extra.clone(),
        created_ns: s.created.unix_timestamp_nanos().clamp(i64::MIN as i128, i64::MAX as i128) as i64,
    }
}

/// What the last save (or load) left on disk.
#[derive(Default)]
pub struct Saved {
    defs_hash: Option<u64>,
    scopes: HashMap<String, u64>,
}

impl Saved {
    pub fn forget(&mut self) {
        self.defs_hash = None;
        self.scopes.clear();
    }
}

#[derive(Default)]
pub struct SaveMemory(pub parking_lot::Mutex<Saved>);

fn hash_bytes(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

impl ArmService {
    pub(crate) fn save_dir(&self, dir: &Path) -> io::Result<()> {
        let mut saved = self.saved.0.lock();
        // -- under the lock: copy what changed --
        let (defs, copies, keep) = {
            let st = self.inner.state.lock();
            let defs = DefsFile {
                subs: st.subs.clone(),
                groups: st
                    .groups
                    .iter()
                    .map(|(k, g)| GroupD {
                        key: k.clone(),
                        sub: g.sub.clone(),
                        name: g.name.clone(),
                        location: g.location.clone(),
                        tags: g.tags.clone(),
                        managed_by: g.managed_by.clone(),
                    })
                    .collect(),
                scope_tags: st.scope_tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                tag_names: st
                    .tag_names
                    .iter()
                    .map(|(k, v)| (k.clone(), v.iter().map(|(n, vals)| (n.clone(), vals.clone())).collect()))
                    .collect(),
            };
            let mut by_scope: BTreeMap<&str, Vec<(&String, &Stored)>> = BTreeMap::new();
            for (k, s) in st.resources.iter() {
                by_scope.entry(scope_of(k)).or_default().push((k, s));
            }
            let mut copies = Vec::new();
            let mut keep = HashSet::new();
            for (scope, list) in by_scope {
                let file = file_name(scope);
                keep.insert(file.clone());
                let rev = st.resources.rev_of(scope);
                if saved.scopes.get(&file) == Some(&rev) {
                    continue;
                }
                copies.push((
                    file,
                    rev,
                    ScopeFile { scope: scope.to_string(), resources: list.into_iter().map(|(k, s)| stored_d(k, s)).collect() },
                ));
            }
            (defs, copies, keep)
        };
        // -- the lock is released --
        for (file, rev, f) in copies {
            statefile::write_stream(&dir.join(&file), SCOPE_KIND, VERSION, &f)?;
            saved.scopes.insert(file, rev);
        }
        let bytes = persist::encode(DEFS_KIND, VERSION, &defs)?;
        let h = hash_bytes(&bytes);
        if saved.defs_hash != Some(h) {
            persist::write_atomic_private(&dir.join(DEFS_FILE), &bytes)?;
            saved.defs_hash = Some(h);
        }
        statefile::remove_stale(dir, "scope", &keep)?;
        saved.scopes.retain(|k, _| keep.contains(k));
        Ok(())
    }

    pub(crate) fn load_dir(&self, dir: &Path) -> io::Result<()> {
        let defs_path = dir.join(DEFS_FILE);
        let Some((_, defs)) = statefile::read_state::<DefsFile>(&defs_path, DEFS_KIND, VERSION)? else {
            return Ok(());
        };
        let bad = |p: &Path, m: String| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {m}", p.display()));
        let mut st = crate::state::State { subs: defs.subs, ..Default::default() };
        for g in defs.groups {
            let group = Group { sub: g.sub, name: g.name, location: g.location, tags: g.tags, managed_by: g.managed_by };
            if st.groups.insert(g.key.clone(), group).is_some() {
                return Err(bad(&defs_path, format!("resource group {} is listed twice", g.key)));
            }
        }
        st.scope_tags = defs.scope_tags.into_iter().collect();
        st.tag_names = defs.tag_names.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect();
        let mut saved = Saved::default();
        // every scope file in the directory (a file is named after the scope it holds, which is checked)
        let mut names: Vec<String> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.starts_with("scope-") && n.ends_with(".state"))
            .collect();
        names.sort();
        for file in names {
            let p = dir.join(&file);
            let Some((_, f)) = statefile::read_state::<ScopeFile>(&p, SCOPE_KIND, VERSION)? else { continue };
            let scope = f.scope;
            if file_name(&scope) != file {
                return Err(bad(&p, format!("it holds the resources of {scope}, which belong in {}", file_name(&scope))));
            }
            // the resources of a group that is not in `arm.state` belong to a group whose delete was saved: they go
            if scope.contains("/resourcegroups/") && !st.groups.contains_key(&scope) {
                continue;
            }
            for r in f.resources {
                if scope_of(&r.key) != scope {
                    return Err(bad(&p, format!("the resource {} does not belong to {scope}", r.key)));
                }
                let created = OffsetDateTime::from_unix_timestamp_nanos(r.created_ns as i128).unwrap_or(OffsetDateTime::UNIX_EPOCH);
                let stored = Stored {
                    id: r.id,
                    sub: r.sub,
                    rg: r.rg,
                    ns: r.ns,
                    types: r.types,
                    names: r.names,
                    body: r.body,
                    extra: r.extra,
                    created,
                };
                st.resources.insert(r.key, stored);
            }
            saved.scopes.insert(file, st.resources.rev_of(&scope));
        }
        *self.inner.state.lock() = st;
        *self.saved.0.lock() = saved;
        Ok(())
    }

    pub(crate) fn generation(&self) -> u64 {
        current_revision()
    }
}
