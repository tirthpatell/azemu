//! How Data Lake shares the Blob data model.
//!
//! A **filesystem is a blob container** and a **path is a blob**: a file is a block blob, a
//! directory is a zero-length block blob with the metadata `hdi_isfolder=true`. Both
//! endpoints read and write the same maps in `Container::blobs`.
//!
//! A container is **hierarchical** (`Container::hns`) from the first time it is touched
//! through the `dfs` endpoint (created there, or any request that names it). A container used
//! only through the blob endpoint stays flat and behaves exactly as before. When a container
//! turns hierarchical, [`adopt`] gives every existing blob its attributes and creates the
//! directories its name implies (`a/b/c.txt` makes `a` and `a/b`). Blob-endpoint writes to a
//! hierarchical container set `Container::dirty`, and the next Data Lake request adopts them.
//!
//! Everything Data Lake knows about a path that Blob does not (owner, group, ACL, expiry,
//! appended-but-not-flushed data) is in [`DfsAttrs`], stored on the blob.

use std::collections::BTreeMap;
use std::sync::Arc;

use azemu_core::bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::dfs::acl::{self, Entry};
use crate::errors::ApiError;
use crate::state::{Blob, BlobEntry, BlobKind, Container, Time};

pub const SUPERUSER: &str = "$superuser";
pub const FOLDER_KEY: &str = "hdi_isfolder";

#[derive(Clone, Serialize, Deserialize)]
pub struct DfsAttrs {
    pub is_dir: bool,
    pub owner: String,
    pub group: String,
    /// Full ACL (access and default entries), see `acl.rs`. Shared: every path made below a directory with the same default ACL
    /// holds the same list (`new_child_attrs` hands out one `Arc`), and a path that changes its ACL gets a list of its own
    /// (`Arc::make_mut`), so a default ACL is stored once and not copied into every child.
    pub acl: Arc<Vec<Entry>>,
    /// The child ACLs this directory's default ACL has produced, kept so that the next child gets the same `Arc`.
    #[serde(skip)]
    inherited: InheritCache,
    pub sticky: bool,
    pub expiry: Option<Time>,
    /// Data sent with Append and not flushed yet, by file offset.
    #[serde(with = "crate::disk::piece_map")]
    pub pending: BTreeMap<u64, Bytes>,
    pub encryption_context: Option<String>,
}

/// What a directory's default ACL made for a new child: (child is a directory, permission bits asked for) and the list.
/// An entry is used only while the directory's ACL is still the list it was made from.
#[derive(Clone, Default)]
struct InheritCache(Arc<parking_lot::Mutex<Vec<InheritEntry>>>);

struct InheritEntry {
    source: Arc<Vec<Entry>>,
    child_is_dir: bool,
    requested: u16,
    result: Arc<Vec<Entry>>,
}

impl DfsAttrs {
    /// The access ACL of a new child made from this directory's default entries, `None` when it has none. The same `Arc`
    /// comes back for the same question as long as this directory's ACL is not changed.
    fn child_acl(&self, child_is_dir: bool, requested: u16) -> Option<Arc<Vec<Entry>>> {
        let mut cache = self.inherited.0.lock();
        if let Some(e) =
            cache.iter().find(|e| e.child_is_dir == child_is_dir && e.requested == requested && Arc::ptr_eq(&e.source, &self.acl))
        {
            return Some(e.result.clone());
        }
        let mut list = acl::inherit(&self.acl, child_is_dir)?;
        // The parent's default entries become the access entries, limited by the requested permissions (0666 / 0777 unless the
        // request gives some); the umask is not used.
        let bits = acl::permission_bits(&list) & requested;
        acl::set_permission_bits(&mut list, bits);
        let result = Arc::new(list);
        cache.retain(|e| Arc::ptr_eq(&e.source, &self.acl));
        cache.truncate(7);
        cache.push(InheritEntry { source: self.acl.clone(), child_is_dir, requested, result: result.clone() });
        Some(result)
    }

    /// Defaults of a path created without anything specified: owner and group `$superuser`,
    /// `rw-r-----` for a file and `rwxr-x---` for a directory.
    pub fn new(is_dir: bool) -> DfsAttrs {
        let bits = if is_dir { 0o750 } else { 0o640 };
        DfsAttrs {
            is_dir,
            owner: SUPERUSER.into(),
            group: SUPERUSER.into(),
            acl: Arc::new(acl::from_bits(bits)),
            inherited: InheritCache::default(),
            sticky: false,
            expiry: None,
            pending: BTreeMap::new(),
            encryption_context: None,
        }
    }

    pub fn permissions(&self) -> String {
        acl::permissions_string(acl::permission_bits(&self.acl), self.sticky)
    }

    pub fn acl_string(&self) -> String {
        acl::format_acl(&self.acl)
    }

    pub fn resource_type(&self) -> &'static str {
        if self.is_dir {
            "directory"
        } else {
            "file"
        }
    }
}

/// A path deleted through the `dfs` endpoint while soft delete is on. Undeleting it brings
/// back the whole subtree.
#[derive(Clone, Serialize, Deserialize)]
pub struct DeletedPath {
    pub deletion_id: String,
    pub name: String,
    pub deleted_at: Time,
    pub blob: Blob,
    /// Everything that lived below a deleted directory.
    pub children: Vec<(String, Blob)>,
}

/// Windows FILETIME (100 ns since 1601), the way `creationTime` and `expiryTime` appear in
/// path listings.
pub fn filetime(t: Time) -> i128 {
    t.unix_timestamp_nanos() / 100 + 116_444_736_000_000_000i128
}

/// A path as the URL names it, without surrounding or doubled slashes. Empty is the root.
pub fn norm(raw: &str) -> String {
    raw.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")
}

pub fn parent_of(name: &str) -> &str {
    name.rfind('/').map(|i| &name[..i]).unwrap_or("")
}

/// The rules for a path name, the same function the Blob endpoint uses (`names.rs`).
pub fn valid_name(name: &str) -> Result<(), ApiError> {
    crate::names::check(name, true)
}

/// `Some` when `name` is a live directory.
pub fn dir_attrs_of<'a>(c: &'a Container, name: &str) -> Option<&'a Blob> {
    live(c, name).filter(|b| is_dir(b))
}

pub fn live<'a>(c: &'a Container, name: &str) -> Option<&'a Blob> {
    c.blobs.get(name).and_then(|e| e.live())
}

pub fn is_dir(b: &Blob) -> bool {
    b.dfs.as_ref().map(|d| d.is_dir).unwrap_or(false)
}

/// Live names below the directory `dir` (empty: the whole container), in order. Doubles as
/// the traversal order of listings and recursive operations.
pub fn below(c: &Container, dir: &str) -> Vec<String> {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    c.blobs
        .range::<String, _>(prefix.clone()..)
        .take_while(|(k, _)| k.starts_with(&prefix))
        .filter(|(_, e)| e.live().is_some())
        .map(|(k, _)| k.clone())
        .collect()
}

pub fn has_children(c: &Container, dir: &str) -> bool {
    let prefix = format!("{dir}/");
    c.blobs.range::<String, _>(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).any(|(_, e)| e.live().is_some())
}

/// Attributes of a directory: the root's or a stored directory's.
pub fn dir_attrs<'a>(c: &'a Container, dir: &str) -> Option<&'a DfsAttrs> {
    if dir.is_empty() {
        return c.root.as_deref();
    }
    live(c, dir).and_then(|b| b.dfs.as_deref())
}

/// The root directory's attributes (created on first use).
pub fn root_attrs(c: &mut Container) -> &mut DfsAttrs {
    c.root.get_or_insert_with(|| Box::new(DfsAttrs::new(true)))
}

/// A directory blob.
pub fn new_dir_blob(attrs: DfsAttrs, now: Time) -> Blob {
    let mut b = Blob::new(BlobKind::Block, now);
    b.metadata = vec![(FOLDER_KEY.to_string(), "true".to_string())];
    b.dfs = Some(Box::new(attrs));
    b
}

/// A file blob (empty).
pub fn new_file_blob(attrs: DfsAttrs, now: Time) -> Blob {
    let mut b = Blob::new(BlobKind::Block, now);
    b.dfs = Some(Box::new(attrs));
    b
}

/// Make sure every ancestor of `name` exists as a directory. A file in the way is
/// `PathConflict`. New directories inherit from their parent's default ACL.
pub fn ensure_parents(c: &mut Container, name: &str, now: Time) -> Result<(), ApiError> {
    let parts: Vec<&str> = name.split('/').collect();
    let mut cur = String::new();
    for part in &parts[..parts.len().saturating_sub(1)] {
        let parent = cur.clone();
        if !cur.is_empty() {
            cur.push('/');
        }
        cur.push_str(part);
        match live(c, &cur) {
            Some(b) if is_dir(b) => continue,
            Some(_) => return Err(path_conflict()),
            None => {}
        }
        let attrs = new_child_attrs(c, &parent, true, &ChildSpec::default());
        let blob = new_dir_blob(attrs, now);
        let e = c.blobs.entry(cur.clone()).or_default();
        e.current = Some(blob);
    }
    Ok(())
}

pub fn path_conflict() -> ApiError {
    crate::dfs::errors::dfs_error("PathConflict")
}

/// What a create request says about the new path's ownership and permissions.
#[derive(Default, Clone)]
pub struct ChildSpec {
    pub owner: Option<String>,
    pub group: Option<String>,
    pub permissions: Option<(u16, bool)>,
    pub umask: Option<u16>,
    pub acl: Option<Vec<Entry>>,
}

/// Attributes of a new path below `parent_dir` (empty: the root): explicit ACL, else the
/// parent's default ACL, else permissions with the umask applied (default umask `0027`).
pub fn new_child_attrs(c: &Container, parent_dir: &str, is_dir: bool, spec: &ChildSpec) -> DfsAttrs {
    let mut a = DfsAttrs::new(is_dir);
    if let Some(o) = &spec.owner {
        a.owner = o.clone();
    }
    if let Some(g) = &spec.group {
        a.group = g.clone();
    }
    let default_bits = if is_dir { 0o777 } else { 0o666 };
    let umask = spec.umask.unwrap_or(0o027);
    let parent = dir_attrs(c, parent_dir);
    if let Some(given) = &spec.acl {
        let base = acl::from_bits(default_bits & !umask);
        a.acl = Arc::new(acl::normalize(given.iter().filter(|e| is_dir || !e.default).cloned().collect(), &base));
        if let Some((_, sticky)) = spec.permissions {
            a.sticky = sticky;
        }
        return a;
    }
    let (requested, sticky) = spec.permissions.unwrap_or((default_bits, false));
    match parent.and_then(|p| p.child_acl(is_dir, requested)) {
        Some(inherited) => {
            a.acl = inherited;
            a.sticky = sticky;
        }
        None => {
            a.acl = Arc::new(acl::from_bits(requested & !umask));
            a.sticky = sticky;
        }
    }
    a
}

/// Give every blob of a container that was touched through the blob endpoint its
/// attributes, and create the directories the names imply.
pub fn adopt(c: &mut Container, now: Time) {
    if !c.dirty && c.touched.is_empty() {
        return;
    }
    let names: Vec<String> = if c.dirty {
        c.blobs.iter().filter(|(_, e)| e.live().is_some()).map(|(k, _)| k.clone()).collect()
    } else {
        std::mem::take(&mut c.touched).into_iter().filter(|n| live(c, n).is_some()).collect()
    };
    c.dirty = false;
    c.touched.clear();
    for name in &names {
        if let Some(b) = c.blobs.get_mut(name).and_then(|e| e.live_mut()) {
            if b.dfs.is_none() {
                let dir =
                    b.size == 0 && b.metadata.iter().any(|(k, v)| k.eq_ignore_ascii_case(FOLDER_KEY) && v.eq_ignore_ascii_case("true"));
                b.dfs = Some(Box::new(DfsAttrs::new(dir)));
                if dir {
                    b.metadata.retain(|(k, _)| !k.eq_ignore_ascii_case(FOLDER_KEY));
                    b.metadata.push((FOLDER_KEY.to_string(), "true".to_string()));
                    b.metadata.sort();
                }
            }
        }
    }
    for name in names {
        // A file in the way of a directory is left alone: the blob endpoint allows it.
        let _ = ensure_parents(c, &name, now);
    }
}

/// Remove paths whose expiry time has passed.
pub fn purge_expired(c: &mut Container, now: Time) {
    let gone: Vec<String> = c
        .blobs
        .iter()
        .filter(|(_, e)| e.live().and_then(|b| b.dfs.as_ref()).and_then(|d| d.expiry).map(|t| t <= now).unwrap_or(false))
        .map(|(k, _)| k.clone())
        .collect();
    for k in gone {
        if let Some(e) = c.blobs.get_mut(&k) {
            e.current = None;
        }
        c.prune(&k);
    }
}

/// Move the whole entry (blob, snapshots, versions) of `name` out of the container.
pub fn take_entry(c: &mut Container, name: &str) -> Option<BlobEntry> {
    c.blobs.remove(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_core::time::OffsetDateTime;

    #[test]
    fn names_and_times() {
        assert_eq!(norm("/a//b/c/"), "a/b/c");
        assert_eq!(norm("/"), "");
        assert_eq!(parent_of("a/b/c"), "a/b");
        assert_eq!(parent_of("a"), "");
        let t = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(filetime(t), 116_444_736_000_000_000);
    }

    #[test]
    fn parents_are_created_and_files_block() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut c = Container::new("fs", now, "\"e\"".into());
        ensure_parents(&mut c, "a/b/c.txt", now).unwrap();
        assert!(is_dir(live(&c, "a").unwrap()) && is_dir(live(&c, "a/b").unwrap()));
        c.blobs.entry("a/f".into()).or_default().current = Some(new_file_blob(DfsAttrs::new(false), now));
        assert_eq!(ensure_parents(&mut c, "a/f/x", now).unwrap_err().code, "PathConflict");
        assert_eq!(below(&c, "a"), vec!["a/b", "a/f"]);
        assert!(has_children(&c, "a") && !has_children(&c, "a/b"));
    }

    #[test]
    fn umask_and_default_acl() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut c = Container::new("fs", now, "\"e\"".into());
        let spec = ChildSpec { permissions: Some((0o777, false)), umask: Some(0), ..Default::default() };
        assert_eq!(new_child_attrs(&c, "", true, &spec).permissions(), "rwxrwxrwx");
        assert_eq!(new_child_attrs(&c, "", false, &ChildSpec::default()).permissions(), "rw-r-----");
        let root = root_attrs(&mut c);
        root.acl = Arc::new(acl::normalize(
            acl::parse_acl("user::rwx,group::r-x,other::---,default:user::rwx,default:group::rwx,default:other::r--").unwrap(),
            &[],
        ));
        let a = new_child_attrs(&c, "", false, &ChildSpec::default());
        assert_eq!(a.permissions(), "rw-rw-r--");
        let d = new_child_attrs(&c, "", true, &ChildSpec::default());
        assert_eq!(d.permissions(), "rwxrwxr--");
    }

    fn big_default_acl() -> Vec<Entry> {
        let named: Vec<String> = (0..28).map(|i| format!("default:user:{i:032x}:rwx")).collect();
        let text = format!("user::rwx,group::r-x,other::---,default:user::rwx,default:group::r-x,default:other::r--,{}", named.join(","));
        acl::normalize(acl::parse_acl(&text).unwrap(), &[])
    }

    /// A default ACL is stored once, not copied into every child.
    #[test]
    fn children_share_the_list_the_default_acl_made() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut c = Container::new("fs", now, "\"e\"".into());
        root_attrs(&mut c).acl = Arc::new(big_default_acl());
        let kids: Vec<DfsAttrs> = (0..1000).map(|_| new_child_attrs(&c, "", false, &ChildSpec::default())).collect();
        assert!(kids.iter().all(|k| Arc::ptr_eq(&k.acl, &kids[0].acl)), "files share one list");
        assert!(kids[0].acl.len() > 28 && kids[0].acl.iter().all(|e| !e.default));
        let dirs: Vec<DfsAttrs> = (0..100).map(|_| new_child_attrs(&c, "", true, &ChildSpec::default())).collect();
        assert!(dirs.iter().all(|k| Arc::ptr_eq(&k.acl, &dirs[0].acl)) && !Arc::ptr_eq(&dirs[0].acl, &kids[0].acl));
        // a different request for permissions gets its own list, then shares that one
        let spec = ChildSpec { permissions: Some((0o440, false)), ..Default::default() };
        let (a, b) = (new_child_attrs(&c, "", false, &spec), new_child_attrs(&c, "", false, &spec));
        assert!(Arc::ptr_eq(&a.acl, &b.acl) && !Arc::ptr_eq(&a.acl, &kids[0].acl));
        // changing the directory's ACL ends the sharing with what it made before; a child that changes its own gets a copy
        root_attrs(&mut c).acl = Arc::new(acl::normalize(
            acl::parse_acl("user::rwx,group::r-x,other::---,default:user::rwx,default:group::rwx,default:other::---").unwrap(),
            &[],
        ));
        let later = new_child_attrs(&c, "", false, &ChildSpec::default());
        assert!(!Arc::ptr_eq(&later.acl, &kids[0].acl));
        assert_eq!(later.permissions(), "rw-rw----");
        let mut mine = kids[0].clone();
        Arc::make_mut(&mut mine.acl)[0].perm = 0;
        assert_ne!(mine.acl, kids[1].acl, "the copy is changed");
        assert_eq!(kids[0].acl, kids[1].acl, "the others are not");
    }

    /// And a 63-level path is made in time proportional to its depth.
    #[test]
    fn a_deep_path_with_a_big_default_acl_is_cheap() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut c = Container::new("fs", now, "\"e\"".into());
        root_attrs(&mut c).acl = Arc::new(big_default_acl());
        let name: String = (0..62).map(|i| format!("d{i}")).collect::<Vec<_>>().join("/") + "/f";
        let t = std::time::Instant::now();
        ensure_parents(&mut c, &name, now).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "{:?}", t.elapsed());
        assert_eq!(below(&c, "").len(), 62);
        // directories made one under another each derive their list from the one above; those made under the same
        // parent share one
        ensure_parents(&mut c, "d0/x/f", now).unwrap();
        ensure_parents(&mut c, "d0/y/f", now).unwrap();
        let (x, y) = (live(&c, "d0/x").unwrap().dfs.as_ref().unwrap(), live(&c, "d0/y").unwrap().dfs.as_ref().unwrap());
        assert!(Arc::ptr_eq(&x.acl, &y.acl));
    }
}
