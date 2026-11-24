//! What the service keeps: accounts, their shares, the tree of directories and files in each share, and the snapshots.
//!
//! Names are case-insensitive and case-preserving, as on a Windows file system: a directory's children are keyed by the lower-case
//! name and each keeps the name it was created with. File content is kept in pages of 512 bytes (the unit Azure reports ranges in)
//! so a file can be sparse and a snapshot costs references, not copies.

use std::collections::BTreeMap;

use azemu_core::bytes::Bytes;
use azemu_core::time::OffsetDateTime;
use serde::{Deserialize, Serialize};

use crate::lease::Lease;
use crate::props::ServiceProps;

/// The size of a page of file content (Azure reports ranges in multiples of it).
pub const PAGE: u64 = 512;
/// The largest file: 4 TiB.
pub const MAX_FILE_SIZE: u64 = 4 << 40;
/// The most data one Put Range takes.
pub const MAX_PUT_RANGE: usize = 4 << 20;
/// The most `File::read` puts in one buffer (a Put Range From URL reads at most `MAX_PUT_RANGE`; bigger reads are streamed).
pub const MAX_BUFFERED_READ: u64 = 8 << 20;

/// A part of the content of a file: stored bytes, or a run of zeros (a hole in a sparse file).
#[derive(Clone)]
pub enum Piece {
    Data(Bytes),
    Zeros(u64),
}
/// The share quota a new share has (Azure: 100 TiB, in GiB).
pub const DEFAULT_QUOTA: u64 = 102_400;

/// The permission key of a directory and of a file that nobody gave a permission (Azure answers these for the default
/// security descriptor).
pub const DEFAULT_DIR_KEY: &str = "4905121398068318397*3227211760911779636";
pub const DEFAULT_FILE_KEY: &str = "9541354914390863802*3227211760911779636";

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct State {
    /// By lower-case account name.
    pub accounts: BTreeMap<String, Account>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Account {
    pub props: ServiceProps,
    /// By share name (shares have lower-case names).
    pub shares: BTreeMap<String, Share>,
    /// Shares deleted while soft delete was on, oldest first.
    #[serde(default)]
    pub deleted: Vec<DeletedShare>,
    /// Security descriptors created with Create Permission (and the ones the Set headers carried), by key.
    #[serde(default)]
    pub permissions: BTreeMap<String, String>,
    /// The next file id.
    #[serde(default)]
    pub next_id: u64,
}

impl Account {
    /// Forget the shares that were deleted more than `days` days ago.
    pub fn purge_deleted(&mut self, now: OffsetDateTime, days: u32) {
        let keep = azemu_core::time::Duration::days(i64::from(days));
        self.deleted.retain(|d| days > 0 && d.deleted + keep > now);
    }
}

/// A new file id. They only have to be different; Azure's look like this.
pub fn new_id(next: &mut u64) -> u64 {
    *next += 1;
    0xC000_0000_0000_0000 | (*next << 20)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DeletedShare {
    pub share: Share,
    pub deleted: OffsetDateTime,
    /// What `x-ms-deleted-share-version` and the `Version` element carry.
    pub version: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedId {
    pub id: String,
    pub start: Option<String>,
    pub expiry: Option<String>,
    pub permission: Option<String>,
    /// The identifier had an `AccessPolicy` element (even an empty one).
    pub has_policy: bool,
}

/// A share, and its snapshots.
#[derive(Clone, Serialize, Deserialize)]
pub struct Share {
    pub name: String,
    pub etag: String,
    pub last_modified: OffsetDateTime,
    pub quota: u64,
    pub tier: String,
    pub tier_changed: OffsetDateTime,
    pub tier_change_count: u32,
    /// Set when the tier was changed (`pending-from-transactionOptimized`); Azure moves the data in the background.
    pub tier_transition: Option<String>,
    pub metadata: Vec<(String, String)>,
    pub acl: Vec<SignedId>,
    pub lease: Lease,
    pub root: Dir,
    /// Oldest first.
    pub snapshots: Vec<Snapshot>,
    pub enabled_protocols: String,
}

/// A share as it was when the snapshot was taken.
#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// `2026-11-24T16:46:56.0000000Z`
    pub time: String,
    pub etag: String,
    pub last_modified: OffsetDateTime,
    pub quota: u64,
    pub tier: String,
    pub tier_changed: OffsetDateTime,
    pub tier_change_count: u32,
    pub tier_transition: Option<String>,
    pub metadata: Vec<(String, String)>,
    pub lease: Lease,
    pub root: Dir,
    pub enabled_protocols: String,
}

impl Share {
    pub fn new(name: &str, now: OffsetDateTime) -> Share {
        Share {
            name: name.to_string(),
            etag: azemu_core::etag::new_etag(),
            last_modified: now,
            quota: DEFAULT_QUOTA,
            tier: "TransactionOptimized".into(),
            tier_changed: now,
            tier_change_count: 0,
            tier_transition: None,
            metadata: Vec::new(),
            acl: Vec::new(),
            lease: Lease::default(),
            root: Dir::new("", 0, 0, now, DEFAULT_DIR_KEY),
            snapshots: Vec::new(),
            enabled_protocols: "SMB".into(),
        }
    }

    pub fn touch(&mut self, now: OffsetDateTime) {
        self.etag = azemu_core::etag::new_etag();
        self.last_modified = now;
    }

    /// The snapshot of this share taken at `time`.
    pub fn take_snapshot(&self, time: String, metadata: Vec<(String, String)>) -> Snapshot {
        Snapshot {
            time,
            etag: self.etag.clone(),
            last_modified: self.last_modified,
            quota: self.quota,
            tier: self.tier.clone(),
            tier_changed: self.tier_changed,
            tier_change_count: self.tier_change_count,
            tier_transition: self.tier_transition.clone(),
            metadata,
            lease: Lease::default(),
            root: self.root.clone(),
            enabled_protocols: self.enabled_protocols.clone(),
        }
    }
}

/// The attributes of a file or a directory (the Windows file attribute bits).
pub mod attr {
    pub const READ_ONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const ARCHIVE: u32 = 0x20;
    pub const NORMAL: u32 = 0x80;
    pub const TEMPORARY: u32 = 0x100;
    pub const OFFLINE: u32 = 0x1000;
    pub const NOT_CONTENT_INDEXED: u32 = 0x2000;
    pub const NO_SCRUB_DATA: u32 = 0x20000;
    /// Name and bit, in the order Azure writes them.
    pub const NAMES: [(&str, u32); 10] = [
        ("ReadOnly", READ_ONLY),
        ("Hidden", HIDDEN),
        ("System", SYSTEM),
        ("Directory", DIRECTORY),
        ("Archive", ARCHIVE),
        ("None", NORMAL),
        ("Temporary", TEMPORARY),
        ("Offline", OFFLINE),
        ("NotContentIndexed", NOT_CONTENT_INDEXED),
        ("NoScrubData", NO_SCRUB_DATA),
    ];

    /// `Hidden | Directory`
    pub fn render(bits: u32) -> String {
        let parts: Vec<&str> = NAMES.iter().filter(|(_, b)| bits & b != 0).map(|(n, _)| *n).collect();
        if parts.is_empty() {
            "None".into()
        } else {
            parts.join(" | ")
        }
    }
}

/// The properties a file and a directory share (the SMB ones).
#[derive(Clone, Serialize, Deserialize)]
pub struct Smb {
    pub attributes: u32,
    pub creation: OffsetDateTime,
    pub last_write: OffsetDateTime,
    pub change: OffsetDateTime,
    pub permission_key: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Dir {
    /// The name it was created with (empty for the root of a share).
    pub name: String,
    pub id: u64,
    pub parent_id: u64,
    pub etag: String,
    pub last_modified: OffsetDateTime,
    pub smb: Smb,
    pub metadata: Vec<(String, String)>,
    /// By lower-case name.
    pub children: BTreeMap<String, Node>,
}

// A file is the larger variant but also the common one; boxing it would only add a pointer to every lookup.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Serialize, Deserialize)]
pub enum Node {
    Dir(Dir),
    File(File),
}

impl Node {
    pub fn name(&self) -> &str {
        match self {
            Node::Dir(d) => &d.name,
            Node::File(f) => &f.name,
        }
    }
    pub fn is_dir(&self) -> bool {
        matches!(self, Node::Dir(_))
    }
}

impl Dir {
    pub fn new(name: &str, id: u64, parent_id: u64, now: OffsetDateTime, key: &str) -> Dir {
        Dir {
            name: name.to_string(),
            id,
            parent_id,
            etag: azemu_core::etag::new_etag(),
            last_modified: now,
            smb: Smb { attributes: attr::DIRECTORY, creation: now, last_write: now, change: now, permission_key: key.to_string() },
            metadata: Vec::new(),
            children: BTreeMap::new(),
        }
    }

    pub fn touch(&mut self, now: OffsetDateTime) {
        self.stamp(now);
        self.smb.change = now;
    }

    /// A new ETag and modification time, for a change that has set the change time itself.
    pub fn stamp(&mut self, now: OffsetDateTime) {
        self.etag = azemu_core::etag::new_etag();
        self.last_modified = now;
    }

    pub fn dir(&self, name: &str) -> Option<&Dir> {
        match self.children.get(&name.to_lowercase()) {
            Some(Node::Dir(d)) => Some(d),
            _ => None,
        }
    }

    pub fn dir_mut(&mut self, name: &str) -> Option<&mut Dir> {
        match self.children.get_mut(&name.to_lowercase()) {
            Some(Node::Dir(d)) => Some(d),
            _ => None,
        }
    }

    pub fn file(&self, name: &str) -> Option<&File> {
        match self.children.get(&name.to_lowercase()) {
            Some(Node::File(f)) => Some(f),
            _ => None,
        }
    }

    pub fn file_mut(&mut self, name: &str) -> Option<&mut File> {
        match self.children.get_mut(&name.to_lowercase()) {
            Some(Node::File(f)) => Some(f),
            _ => None,
        }
    }
}

/// How a file was copied, for `x-ms-copy-*`.
#[derive(Clone, Serialize, Deserialize)]
pub struct CopyInfo {
    pub id: String,
    pub source: String,
    /// `success`, `aborted`, `failed` or `pending`.
    pub status: String,
    pub completed: OffsetDateTime,
    pub progress: String,
    pub description: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct File {
    pub name: String,
    pub id: u64,
    pub parent_id: u64,
    pub etag: String,
    pub last_modified: OffsetDateTime,
    pub size: u64,
    /// Page number to its 512 bytes. A page that is not here is zeros. Pages past the size are not kept.
    #[serde(default)]
    pub pages: BTreeMap<u64, Bytes>,
    pub content_type: String,
    pub content_encoding: Option<String>,
    pub content_language: Option<String>,
    pub cache_control: Option<String>,
    pub content_disposition: Option<String>,
    /// The MD5 the client gave with the content (`x-ms-content-md5`), returned by a whole-file read.
    pub content_md5: Option<Vec<u8>>,
    pub metadata: Vec<(String, String)>,
    pub smb: Smb,
    pub lease: Lease,
    pub copy: Option<CopyInfo>,
}

impl File {
    pub fn touch(&mut self, now: OffsetDateTime) {
        self.stamp(now);
        self.smb.change = now;
    }

    pub fn stamp(&mut self, now: OffsetDateTime) {
        self.etag = azemu_core::etag::new_etag();
        self.last_modified = now;
    }

    /// The content of `start..end` (`end` exclusive, not past the size) as the pages that hold data and the runs of zeros between
    /// them. Nothing is allocated for a hole, however long it is, and a page is a handle to the stored bytes, not a copy: the
    /// list is as long as the number of stored pages in the range, never as long as the range.
    pub fn pieces(&self, start: u64, end: u64) -> Vec<Piece> {
        let mut out = Vec::new();
        if end <= start {
            return out;
        }
        let mut pos = start;
        for (page, data) in self.pages.range(start / PAGE..=(end - 1) / PAGE) {
            let page_start = page * PAGE;
            let from = start.max(page_start);
            let to = end.min(page_start + PAGE);
            if from > pos {
                out.push(Piece::Zeros(from - pos));
            }
            // a page that is shorter than it should be (a damaged state file) is read as it is, the rest of it as zeros
            let (a, b) = ((from - page_start) as usize, ((to - page_start) as usize).min(data.len()));
            if a < b {
                out.push(Piece::Data(data.slice(a..b)));
            }
            let covered = a.max(b) as u64 + page_start;
            if covered < to {
                out.push(Piece::Zeros(to - covered));
            }
            pos = to;
        }
        if end > pos {
            out.push(Piece::Zeros(end - pos));
        }
        out
    }

    /// Bytes `start..end` (`end` exclusive, not past the size) in one buffer. For ranges a request may not make long: no more
    /// than [`MAX_BUFFERED_READ`] bytes are read, whatever `end` is; a longer read is answered in pieces (`pieces`).
    pub fn read(&self, start: u64, end: u64) -> Vec<u8> {
        let end = end.min(start.saturating_add(MAX_BUFFERED_READ));
        let mut out = Vec::new();
        for p in self.pieces(start, end) {
            match p {
                Piece::Data(d) => out.extend_from_slice(&d),
                Piece::Zeros(n) => out.resize(out.len() + n as usize, 0),
            }
        }
        out
    }

    /// Write `data` at `start`. The range must be inside the size (the caller checked).
    pub fn write(&mut self, start: u64, data: &[u8]) {
        let end = start + data.len() as u64;
        let mut pos = start;
        while pos < end {
            let page = pos / PAGE;
            let page_start = page * PAGE;
            let upto = end.min(page_start + PAGE);
            let chunk = &data[(pos - start) as usize..(upto - start) as usize];
            let mut buf = match self.pages.get(&page) {
                Some(b) => b.to_vec(),
                None => vec![0u8; PAGE as usize],
            };
            buf[(pos - page_start) as usize..(upto - page_start) as usize].copy_from_slice(chunk);
            self.pages.insert(page, Bytes::from(buf));
            pos = upto;
        }
    }

    /// Zero `start..end`. A page that is zero all through is dropped, so it is no longer reported as a range. Only the pages that
    /// are stored are visited, so clearing the whole of a file of terabytes costs what the file holds.
    pub fn clear(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let first = start / PAGE;
        let last = (end - 1) / PAGE;
        // the pages cleared whole are `whole_from..whole_to`; a page at either edge that is cleared in part is done on its own
        let (mut whole_from, mut whole_to) = (first, last + 1);
        if !start.is_multiple_of(PAGE) {
            self.clear_in_page(first, start - first * PAGE, (end - first * PAGE).min(PAGE));
            whole_from = first + 1;
        }
        if !end.is_multiple_of(PAGE) && last >= whole_from {
            self.clear_in_page(last, 0, end - last * PAGE);
            whole_to = last;
        }
        if whole_from < whole_to {
            let gone: Vec<u64> = self.pages.range(whole_from..whole_to).map(|(p, _)| *p).collect();
            for p in gone {
                self.pages.remove(&p);
            }
        }
    }

    /// Zero bytes `from..to` of one page (offsets in the page).
    fn clear_in_page(&mut self, page: u64, from: u64, to: u64) {
        let Some(b) = self.pages.get(&page) else { return };
        let mut buf = b.to_vec();
        let (from, to) = ((from as usize).min(buf.len()), (to as usize).min(buf.len()));
        buf[from..to].fill(0);
        if buf.iter().all(|x| *x == 0) {
            self.pages.remove(&page);
        } else {
            self.pages.insert(page, Bytes::from(buf));
        }
    }

    /// Make the file `size` bytes: cut off what is past the end, or leave the new part as zeros.
    pub fn resize(&mut self, size: u64) {
        if size < self.size {
            if !size.is_multiple_of(PAGE) {
                self.clear(size, (size / PAGE + 1) * PAGE);
            }
            let first_gone = size.div_ceil(PAGE);
            let _ = self.pages.split_off(&first_gone);
        }
        self.size = size;
    }
}
