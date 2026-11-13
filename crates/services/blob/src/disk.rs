//! Saving the Blob and Data Lake state to a data directory (`--data-dir`) and reading it back.
//!
//! Layout of `<data-dir>/blob/`:
//!
//! ```text
//! blob.state          everything except the bytes of blobs (kind "blob", version 1): the accounts, containers,
//!                     blobs, snapshots, versions, leases, tags, ACLs ... and a table that says where each
//!                     piece of data is
//! pieces/<32 hex>     data, written once and never changed: either one big buffer, or many small ones packed
//!                     together. A file is deleted when the newest blob.state no longer refers to it.
//! ```
//!
//! The data of a blob is a list of buffers (`Bytes`): blocks, page ranges, staged blocks. Buffers are told apart by
//! where they are in memory (address and length), which is safe because the index holds a reference to each buffer
//! it knows, so the address cannot be used by anything else. A buffer that is already in a piece file is not
//! written again; so a save after a small change writes the new `blob.state` and the new buffers only.
//!
//! A save goes: copy the metadata under the lock (cheap: buffers are shared, not copied) -> encode it and write the
//! new pieces without the lock -> replace `blob.state` -> delete the pieces nothing refers to any more. A crash at
//! any point leaves the old `blob.state` with all of its pieces, so the directory still loads.
//!
//! A packed file is rewritten (its live buffers go to a new pack) when most of it is dead, so a long-running
//! emulator does not keep a 8 MiB pack for the sake of one small blob.
//!
//! Loading reads every referenced piece file into memory (the emulator keeps blob data in memory anyway; the data
//! type does not allow reading on first use) and checks its length and checksum.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use azemu_core::bytes::Bytes;
use azemu_core::persist;
use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::state::{Account, Blob, Container, State};

pub const KIND: &str = "blob";
pub const VERSION: u32 = 1;
const STATE_FILE: &str = "blob.state";
const PIECES_DIR: &str = "pieces";
/// The reference of an empty buffer (nothing is stored for it).
const EMPTY: u32 = u32::MAX;
/// The largest `blob.state` read (it holds metadata only).
const MAX_STATE_BYTES: u64 = 2 << 30;

/// When buffers get their own file, how big a pack gets, and when a pack is rewritten.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// A buffer of at least this many bytes is a piece file of its own.
    pub large: usize,
    /// Small buffers are packed into one file until it holds this many bytes.
    pub pack_target: usize,
    /// A pack is rewritten when less than a quarter of it is alive and at least this many bytes are dead.
    pub min_waste: u64,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy { large: 1 << 20, pack_target: 8 << 20, min_waste: 1 << 20 }
    }
}

// ---------------------------------------------------------------------------
// Buffers inside the serialized state: a number that refers to the table of pieces
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Collector {
    used: Vec<Bytes>,
    seen: HashMap<(usize, usize), u32>,
}

thread_local! {
    /// While the state is being encoded: the buffers met so far.
    static COLLECT: RefCell<Option<Collector>> = const { RefCell::new(None) };
    /// While the state is being decoded: the buffers of the table of pieces.
    static TABLE: RefCell<Option<Vec<Bytes>>> = const { RefCell::new(None) };
}

fn key(b: &Bytes) -> (usize, usize) {
    (b.as_ptr() as usize, b.len())
}

fn reference(b: &Bytes) -> Result<u32, String> {
    if b.is_empty() {
        return Ok(EMPTY);
    }
    COLLECT.with(|c| match c.borrow_mut().as_mut() {
        None => Err("encoding outside of a save".to_string()),
        Some(c) => {
            if let Some(i) = c.seen.get(&key(b)) {
                return Ok(*i);
            }
            let i = c.used.len();
            if i >= (EMPTY - 1) as usize {
                return Err("too many buffers".to_string());
            }
            c.used.push(b.clone());
            c.seen.insert(key(b), i as u32);
            Ok(i as u32)
        }
    })
}

fn resolve(i: u32) -> Result<Bytes, String> {
    if i == EMPTY {
        return Ok(Bytes::new());
    }
    TABLE.with(|t| match t.borrow().as_ref() {
        None => Err("decoding outside of a load".to_string()),
        Some(v) => v.get(i as usize).cloned().ok_or_else(|| format!("data reference {i} is outside the table of {} pieces", v.len())),
    })
}

/// `#[serde(with = "crate::disk::piece")]` for a `Bytes` field.
pub mod piece {
    use super::*;

    pub fn serialize<S: Serializer>(b: &Bytes, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(reference(b).map_err(S::Error::custom)?)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
        resolve(u32::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// `#[serde(with = "crate::disk::piece_map")]` for a `BTreeMap<K, Bytes>` field.
pub mod piece_map {
    use super::*;

    pub fn serialize<K: Serialize, S: Serializer>(m: &BTreeMap<K, Bytes>, s: S) -> Result<S::Ok, S::Error> {
        let mut items = Vec::with_capacity(m.len());
        for (k, v) in m {
            items.push((k, reference(v).map_err(S::Error::custom)?));
        }
        s.collect_map(items)
    }

    pub fn deserialize<'de, K: Deserialize<'de> + Ord, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<K, Bytes>, D::Error> {
        let raw = BTreeMap::<K, u32>::deserialize(d)?;
        raw.into_iter().map(|(k, i)| resolve(i).map(|b| (k, b)).map_err(D::Error::custom)).collect()
    }
}

// ---------------------------------------------------------------------------
// The index: which buffers are already in a piece file
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Loc {
    file: Arc<str>,
    off: u64,
    len: u64,
    crc: u64,
}

#[derive(Default)]
pub(crate) struct DiskIndex {
    /// Each buffer that is in a piece file. The `Bytes` is kept so that its address stays taken.
    known: HashMap<(usize, usize), (Bytes, Loc)>,
    /// Size of every piece file the newest `blob.state` refers to.
    sizes: HashMap<Arc<str>, u64>,
    /// SHA-256 of the `blob.state` written last, to skip writing identical content.
    last_state: Option<[u8; 32]>,
    pub(crate) policy: Policy,
}

/// The table in `blob.state`: where each piece is. `pieces[i]` is (index in `files`, offset, length, checksum).
#[derive(Serialize, Deserialize)]
struct Table {
    files: Vec<String>,
    pieces: Vec<(u32, u64, u64, u64)>,
}

fn is_piece_name(n: &str) -> bool {
    n.len() == 32 && n.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn new_piece_name() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

struct Placed {
    locs: Vec<Loc>,
    new_sizes: Vec<(Arc<str>, u64)>,
}

impl DiskIndex {
    /// Where every buffer of `used` is on disk, writing those that are not on disk yet.
    fn place(&self, pieces_dir: &Path, used: &[Bytes]) -> io::Result<Placed> {
        // a pack that is mostly dead is not reused: its live buffers move to a new file
        let mut live: HashMap<Arc<str>, u64> = HashMap::new();
        for b in used {
            if let Some((_, loc)) = self.known.get(&key(b)) {
                *live.entry(loc.file.clone()).or_default() += loc.len;
            }
        }
        let rewrite: HashSet<Arc<str>> = live
            .iter()
            .filter(|(f, l)| {
                let size = self.sizes.get(*f).copied().unwrap_or(0);
                **l < size && (size - **l) >= self.policy.min_waste && **l * 4 < size
            })
            .map(|(f, _)| f.clone())
            .collect();

        let mut locs: Vec<Option<Loc>> = Vec::with_capacity(used.len());
        let mut pending = Vec::new();
        for (i, b) in used.iter().enumerate() {
            match self.known.get(&key(b)) {
                Some((_, loc)) if !rewrite.contains(&loc.file) => locs.push(Some(loc.clone())),
                _ => {
                    locs.push(None);
                    pending.push(i);
                }
            }
        }

        let mut new_sizes: Vec<(Arc<str>, u64)> = Vec::new();
        let mut pack: Vec<u8> = Vec::new();
        let mut pack_name: Arc<str> = Arc::from(new_piece_name());
        std::fs::create_dir_all(pieces_dir).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", pieces_dir.display())))?;
        let flush = |pack: &mut Vec<u8>, name: &Arc<str>, new_sizes: &mut Vec<(Arc<str>, u64)>| -> io::Result<()> {
            if !pack.is_empty() {
                persist::write_atomic(&pieces_dir.join(&**name), pack)?;
                new_sizes.push((name.clone(), pack.len() as u64));
                pack.clear();
            }
            Ok(())
        };
        for i in pending {
            let b = &used[i];
            let crc = crate::util::crc64(b);
            if b.len() >= self.policy.large {
                let name: Arc<str> = Arc::from(new_piece_name());
                persist::write_atomic(&pieces_dir.join(&*name), b)?;
                new_sizes.push((name.clone(), b.len() as u64));
                locs[i] = Some(Loc { file: name, off: 0, len: b.len() as u64, crc });
            } else {
                if !pack.is_empty() && pack.len() + b.len() > self.policy.pack_target {
                    flush(&mut pack, &pack_name, &mut new_sizes)?;
                    pack_name = Arc::from(new_piece_name());
                }
                locs[i] = Some(Loc { file: pack_name.clone(), off: pack.len() as u64, len: b.len() as u64, crc });
                pack.extend_from_slice(b);
            }
        }
        flush(&mut pack, &pack_name, &mut new_sizes)?;
        Ok(Placed { locs: locs.into_iter().flatten().collect(), new_sizes })
    }
}

/// Delete piece files that `keep` does not name, and left-over temporary files.
fn collect_garbage(pieces_dir: &Path, keep: &HashSet<&str>) {
    let Ok(rd) = std::fs::read_dir(pieces_dir) else { return };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let ours = (is_piece_name(&name) && !keep.contains(name.as_str())) || (name.starts_with('.') && name.contains(".tmp-"));
        if ours && ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            let _ = std::fs::remove_file(ent.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Save
// ---------------------------------------------------------------------------

impl crate::Shared {
    pub(crate) fn save_to(&self, dir: &Path) -> io::Result<()> {
        // 1. under the lock: copy the metadata. Buffers are shared (reference counted), not copied.
        let accounts: BTreeMap<String, Account> = {
            let st = self.state.lock();
            let since = Instant::now();
            let copy = st.accounts.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            self.hold.record(since.elapsed());
            copy
        };

        // 2. encode, noting every buffer met
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                COLLECT.with(|c| *c.borrow_mut() = None);
            }
        }
        COLLECT.with(|c| *c.borrow_mut() = Some(Collector::default()));
        let _clear = Clear;
        let body = rmp_serde::to_vec_named(&accounts).map_err(|e| bad(format!("cannot encode blob state: {e}")))?;
        let used = COLLECT.with(|c| c.borrow_mut().take()).map(|c| c.used).unwrap_or_default();
        drop(accounts);

        // 3. write the buffers that are not on disk yet
        let pieces_dir = dir.join(PIECES_DIR);
        let mut index = self.disk.lock();
        let placed = index.place(&pieces_dir, &used)?;

        // 4. the new blob.state
        let mut files: Vec<String> = Vec::new();
        let mut file_no: HashMap<Arc<str>, u32> = HashMap::new();
        let mut pieces = Vec::with_capacity(placed.locs.len());
        for l in &placed.locs {
            let n = *file_no.entry(l.file.clone()).or_insert_with(|| {
                files.push(l.file.to_string());
                (files.len() - 1) as u32
            });
            pieces.push((n, l.off, l.len, l.crc));
        }
        let mut bytes = persist::encode(KIND, VERSION, &Table { files, pieces })?;
        // the table is a map of two entries; add "accounts" with the already encoded value
        let at = bytes.iter().position(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
        if bytes.get(at) != Some(&0x82) {
            return Err(bad("unexpected layout of the encoded table".into()));
        }
        bytes[at] = 0x83;
        rmp_serde::encode::write_named(&mut bytes, "accounts").map_err(|e| bad(e.to_string()))?;
        bytes.extend_from_slice(&body);
        drop(body);

        use sha2::Digest;
        let digest: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        let state_path = dir.join(STATE_FILE);
        if index.last_state != Some(digest) || !state_path.exists() {
            persist::write_atomic(&state_path, &bytes)?;
        }

        // 5. the new state is durable: nothing refers to the old pieces any more
        let keep: HashSet<&str> = placed.locs.iter().map(|l| &*l.file).collect();
        collect_garbage(&pieces_dir, &keep);

        // 6. remember where everything is
        let mut sizes: HashMap<Arc<str>, u64> = HashMap::new();
        for (f, s) in &placed.new_sizes {
            sizes.insert(f.clone(), *s);
        }
        for l in &placed.locs {
            if !sizes.contains_key(&l.file) {
                if let Some(s) = index.sizes.get(&l.file) {
                    sizes.insert(l.file.clone(), *s);
                }
            }
        }
        index.known = used.into_iter().zip(placed.locs).map(|(b, l)| (key(&b), (b, l))).collect();
        index.sizes = sizes;
        index.last_state = Some(digest);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Load
// ---------------------------------------------------------------------------

impl crate::Shared {
    pub(crate) fn load_from(&self, dir: &Path) -> io::Result<()> {
        let state_path = dir.join(STATE_FILE);
        let meta = match std::fs::metadata(&state_path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()), // nothing saved yet
            Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", state_path.display()))),
        };
        let at = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", state_path.display()));
        if meta.len() > MAX_STATE_BYTES {
            return Err(at(bad(format!("the file is {} bytes; more than {MAX_STATE_BYTES} is not a blob state file", meta.len()))));
        }
        let bytes = std::fs::read(&state_path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", state_path.display())))?;

        // the table of pieces first (the accounts after it are skipped), then the pieces, then everything
        let (_, table): (u32, Table) = persist::decode(KIND, VERSION, &bytes).map_err(at)?;
        let pieces_dir = dir.join(PIECES_DIR);
        let (buffers, known, sizes) = read_pieces(&pieces_dir, &table)?;

        TABLE.with(|t| *t.borrow_mut() = Some(buffers));
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                TABLE.with(|t| *t.borrow_mut() = None);
            }
        }
        let _clear = Clear;
        #[derive(Deserialize)]
        struct Whole {
            accounts: HashMap<String, Account>,
        }
        let (_, whole): (u32, Whole) = persist::decode(KIND, VERSION, &bytes).map_err(at)?;
        drop(_clear);
        let mut accounts = whole.accounts;
        validate(&accounts).map_err(|why| at(bad(why)))?;
        share_acls(&mut accounts);

        let digest: [u8; 32] = {
            use sha2::Digest;
            sha2::Sha256::digest(&bytes).into()
        };
        {
            let mut st = self.state.lock();
            *st = State { accounts, ..State::with_config(st.config) };
        }
        let mut index = self.disk.lock();
        index.known = known;
        index.sizes = sizes;
        index.last_state = Some(digest);
        Ok(())
    }
}

type Loaded = (Vec<Bytes>, HashMap<(usize, usize), (Bytes, Loc)>, HashMap<Arc<str>, u64>);

/// Read the piece files the table names; check names, lengths and checksums.
fn read_pieces(pieces_dir: &Path, table: &Table) -> io::Result<Loaded> {
    let mut data: Vec<Option<Bytes>> = Vec::with_capacity(table.files.len().min(4096));
    let mut sizes: HashMap<Arc<str>, u64> = HashMap::new();
    // what each file must hold at least: the end of its furthest piece
    let mut need: Vec<u64> = vec![0; table.files.len()];
    for (n, (file, off, len, _)) in table.pieces.iter().enumerate() {
        let f = *file as usize;
        if f >= table.files.len() {
            return Err(bad(format!("{STATE_FILE}: piece {n} names file number {file}, but there are {} files", table.files.len())));
        }
        let end = off.checked_add(*len).ok_or_else(|| bad(format!("{STATE_FILE}: piece {n} has an impossible size")))?;
        need[f] = need[f].max(end);
    }
    for (f, name) in table.files.iter().enumerate() {
        if !is_piece_name(name) {
            return Err(bad(format!("{STATE_FILE}: '{name}' is not a name of a piece file")));
        }
        let path = pieces_dir.join(name);
        let bytes = std::fs::read(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                bad(format!(
                    "{}: this data file is missing (blob.state refers to it); restore it or move the whole blob directory away",
                    path.display()
                ))
            } else {
                io::Error::new(e.kind(), format!("{}: {e}", path.display()))
            }
        })?;
        if (bytes.len() as u64) < need[f] {
            return Err(bad(format!(
                "{}: the file has {} bytes but blob.state needs {}; it was cut short",
                path.display(),
                bytes.len(),
                need[f]
            )));
        }
        sizes.insert(Arc::from(name.as_str()), bytes.len() as u64);
        data.push(Some(Bytes::from(bytes)));
    }
    let mut buffers = Vec::with_capacity(table.pieces.len().min(1 << 20));
    let mut known = HashMap::with_capacity(table.pieces.len().min(1 << 20));
    for (n, (file, off, len, crc)) in table.pieces.iter().enumerate() {
        let whole = data[*file as usize].as_ref().ok_or_else(|| bad(format!("piece {n}: file is gone")))?;
        let (off, len) = (*off as usize, *len as usize);
        let b = whole.slice(off..off + len);
        if crate::util::crc64(&b) != *crc {
            let name = &table.files[*file as usize];
            return Err(bad(format!(
                "{}: the data at offset {off}, {len} bytes, does not match its checksum; the file is damaged",
                pieces_dir.join(name).display()
            )));
        }
        if !b.is_empty() {
            let loc = Loc { file: Arc::from(table.files[*file as usize].as_str()), off: off as u64, len: len as u64, crc: *crc };
            known.insert(key(&b), (b.clone(), loc));
        }
        buffers.push(b);
    }
    Ok((buffers, known, sizes))
}

fn for_each_blob(c: &Container, f: &mut impl FnMut(&str, &Blob)) {
    for (name, e) in &c.blobs {
        for b in e.current.iter().chain(e.snapshots.values()).chain(e.versions.iter()) {
            f(name, b);
        }
    }
    for d in &c.deleted_paths {
        f(&d.name, &d.blob);
        for (n, b) in &d.children {
            f(n, b);
        }
    }
}

/// Facts the rest of the service relies on and that a damaged file could break.
fn validate(accounts: &HashMap<String, Account>) -> Result<(), String> {
    for (an, a) in accounts {
        for c in a.containers.values().chain(a.deleted_containers.iter()) {
            let mut problem = None;
            for_each_blob(c, &mut |name, b| {
                if problem.is_some() {
                    return;
                }
                if let Err(why) = b.check_consistent() {
                    problem = Some(format!("blob '{an}/{}/{name}' is inconsistent: {why}", c.name));
                }
            });
            if let Some(p) = problem {
                return Err(p);
            }
        }
    }
    Ok(())
}

/// Paths that were saved with equal ACLs share one list again, as they do when made through the API.
fn share_acls(accounts: &mut HashMap<String, Account>) {
    let mut seen: HashMap<Vec<crate::dfs::acl::Entry>, Arc<Vec<crate::dfs::acl::Entry>>> = HashMap::new();
    let mut share = |b: &mut Blob| {
        if let Some(d) = b.dfs.as_mut() {
            let list = seen.entry((*d.acl).clone()).or_insert_with(|| d.acl.clone()).clone();
            d.acl = list;
        }
    };
    for a in accounts.values_mut() {
        for c in a.containers.values_mut().chain(a.deleted_containers.iter_mut()) {
            for e in c.blobs.values_mut() {
                for b in e.current.iter_mut().chain(e.snapshots.values_mut()).chain(e.versions.iter_mut()) {
                    share(b);
                }
            }
            for d in &mut c.deleted_paths {
                share(&mut d.blob);
                for (_, b) in &mut d.children {
                    share(b);
                }
            }
        }
    }
}
