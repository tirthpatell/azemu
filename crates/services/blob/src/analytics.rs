//! Classic Storage Analytics logging for the Blob service: the `$logs` container.
//!
//! What Azure does (Microsoft Learn: "Azure Storage Analytics logging", "Storage Analytics log format",
//! "Storage Analytics logged operations and status messages"):
//!
//! * `$logs` is made when logging is turned on (Set Blob Service Properties with `Read`, `Write` or `Delete`
//!   true); it is also made by updates of CORS, static website, the default service version and soft delete. It
//!   cannot be deleted once logging has been enabled (its blobs can). Plain List Containers does not return it; it
//!   comes back with `include=system`.
//! * A request is logged when its class (read, write, delete) is switched on in the `Logging` element. Authorized
//!   requests are logged whatever the outcome; an anonymous or SAS request only when it succeeded, failed with a
//!   server error or was a `304`.
//! * Entries are written, a while after the request, into block blobs named
//!   `blob/YYYY/MM/DD/hh00/NNNNNN.log` (the minute part is always `00`; the counter counts the log blobs of the
//!   service in that hour) with the metadata `LogType` (`read`, `write`, `delete`, joined by commas), `StartTime`,
//!   `EndTime` and `LogVersion`. A finished log blob is not changed again.
//! * Each line has 30 fields separated by `;` (version 1.0). A field that can hold a quote, a semicolon or a newline
//!   is HTML encoded and quoted. The query string of the URL keeps its parameters; the SAS signature is `sig=XXXXX`.
//!
//! What this emulator does differently is listed in `docs/services/blob.md` ("Storage Analytics logging").
//!
//! Design: the request path only builds the finished text lines and puts them in a bounded queue (no state lock).
//! One tokio task per batch sleeps for the delay (`AZEMU_BLOB_LOG_DELAY`), then writes the batch as log blobs under
//! one short hold of the state lock. Log blobs are ordinary blobs, so persistence, listing and deleting work as for
//! any other container.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use azemu_core::etag;
use azemu_core::time::{self, OffsetDateTime};
use azemu_core::{Request, Response};
use parking_lot::{Mutex, RwLock};

use crate::ops::Op;
use crate::state::{Account, Blob, BlobEntry, BlobKind, Config, Container, Seg, ServiceProps, Time};
use crate::util::{crc64, md5};
use crate::Shared;

/// The system container.
pub const LOGS: &str = "$logs";

/// Largest log blob. A bigger batch is split into several blobs.
const MAX_LOG_BLOB_BYTES: usize = 4 * 1024 * 1024;
/// Limits of the queue between the request path and the writer: lines waiting for the next flush.
const MAX_QUEUED_LINES: usize = 50_000;
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
/// A field that comes from the client is cut at this length (Azure's own limit for the client request id is 1 KiB).
const MAX_URL: usize = 4096;
const MAX_FIELD: usize = 1024;

/// Which class of operation a request belongs to: the three switches of the `Logging` element.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Read,
    Write,
    Delete,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Read => "read",
            Class::Write => "write",
            Class::Delete => "delete",
        }
    }
}

/// The part of the service properties that decides what is logged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogCfg {
    pub read: bool,
    pub write: bool,
    pub delete: bool,
    /// `Version` is `2.0`: lines get the eight extra fields of format 2.0 (left empty).
    pub v2: bool,
    /// Days log blobs are kept (`RetentionPolicy` enabled with `Days`); `None`: no age limit.
    pub retention_days: Option<u32>,
}

impl LogCfg {
    pub fn from_props(p: &ServiceProps) -> LogCfg {
        let l = &p.logging;
        LogCfg {
            read: l.read,
            write: l.write,
            delete: l.delete,
            v2: l.version.trim() == "2.0",
            retention_days: if l.retention.enabled { l.retention.days } else { None },
        }
    }

    pub fn any(&self) -> bool {
        self.read || self.write || self.delete
    }

    fn wants(&self, c: Class) -> bool {
        match c {
            Class::Read => self.read,
            Class::Write => self.write,
            Class::Delete => self.delete,
        }
    }
}

/// One finished log line waiting for the next flush.
pub struct Pending {
    pub account: String,
    /// When the request finished: names the hour of the log blob.
    pub end: Time,
    /// When the request arrived (the `StartTime` / `EndTime` metadata).
    pub start: Time,
    pub class: Class,
    pub line: String,
}

#[derive(Default)]
struct Queue {
    items: Vec<Pending>,
    bytes: usize,
}

/// The queue, the per-account switches and the bookkeeping of the writer.
#[derive(Default)]
pub struct Logger {
    cfg: RwLock<HashMap<String, LogCfg>>,
    queue: Mutex<Queue>,
    /// A flush is already scheduled.
    pub armed: AtomicBool,
    /// Next counter of an hour that already had log blobs, by (account, hour directory). Never goes down, so a
    /// name that was used and then removed is not used again (a client that remembers names would skip the new blob).
    counters: Mutex<BTreeMap<(String, String), u32>>,
    /// Lines that were not queued because the queue was full.
    pub dropped: AtomicU64,
}

impl Logger {
    /// The switches of one account; `None` when nothing is logged for it.
    pub fn cfg_for(&self, account: &str) -> Option<LogCfg> {
        self.cfg.read().get(account).copied().filter(LogCfg::any)
    }

    pub fn set_cfg(&self, account: &str, cfg: LogCfg) {
        let mut m = self.cfg.write();
        if cfg.any() {
            m.insert(account.to_string(), cfg);
        } else {
            m.remove(account);
        }
    }

    pub fn set_all(&self, all: HashMap<String, LogCfg>) {
        *self.cfg.write() = all.into_iter().filter(|(_, c)| c.any()).collect();
    }

    pub fn clear(&self) {
        let mut q = self.queue.lock();
        q.items.clear();
        q.bytes = 0;
        self.counters.lock().clear();
    }

    /// Queue lines; whatever does not fit is dropped and counted.
    pub fn push(&self, lines: Vec<Pending>) {
        let mut q = self.queue.lock();
        for p in lines {
            let size = p.line.len() + p.account.len() + 64;
            if q.items.len() >= MAX_QUEUED_LINES || q.bytes + size > MAX_QUEUED_BYTES {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            q.bytes += size;
            q.items.push(p);
        }
    }

    fn take(&self) -> Vec<Pending> {
        let mut q = self.queue.lock();
        q.bytes = 0;
        std::mem::take(&mut q.items)
    }

    pub fn queued(&self) -> usize {
        self.queue.lock().items.len()
    }
}

// ---------------------------------------------------------------------------
// What a request is
// ---------------------------------------------------------------------------

/// The class of a request: reads fetch, deletes remove, everything else writes.
pub fn class_of(op: Op, method: &str) -> Class {
    match op {
        Op::QueryBlob | Op::GetUserDelegationKey | Op::Options => Class::Read,
        _ => match method {
            "GET" | "HEAD" | "OPTIONS" => Class::Read,
            "DELETE" => Class::Delete,
            _ => Class::Write,
        },
    }
}

/// The operation names of "Storage Analytics logged operations" for the Blob service. Lease operations are named by their
/// action; a clear on a page blob is `ClearPage`. Operations the page does not list get a name made the same way.
pub fn operation_name(op: Op, req: &Request) -> &'static str {
    let lease = req.header("x-ms-lease-action").map(|a| a.to_ascii_lowercase());
    let lease = lease.as_deref();
    match op {
        Op::ListContainers => "ListContainers",
        Op::GetServiceProperties => "GetBlobServiceProperties",
        Op::SetServiceProperties => "SetBlobServiceProperties",
        Op::GetServiceStats => "GetBlobServiceStats",
        Op::GetUserDelegationKey => "GetUserDelegationKey",
        Op::GetAccountInfo => "GetAccountInformation",
        Op::FindBlobsAccount | Op::FindBlobsContainer => "FindBlobsByTags",
        Op::BatchAccount | Op::BatchContainer => "BlobBatch",
        Op::CreateContainer => "CreateContainer",
        Op::DeleteContainer => "DeleteContainer",
        Op::GetContainerProperties => "GetContainerProperties",
        Op::GetContainerMetadata => "GetContainerMetadata",
        Op::SetContainerMetadata => "SetContainerMetadata",
        Op::GetContainerAcl => "GetContainerACL",
        Op::SetContainerAcl => "SetContainerACL",
        Op::LeaseContainer => match lease {
            Some("renew") => "RenewContainerLease",
            Some("release") => "ReleaseContainerLease",
            Some("break") => "BreakContainerLease",
            Some("change") => "ChangeContainerLease",
            _ => "AcquireContainerLease",
        },
        Op::ListBlobs => "ListBlobs",
        Op::UndeleteContainer => "RestoreContainer",
        Op::RenameContainer => "RenameContainer",
        Op::CreateSession => "CreateContainerSession",
        Op::PutBlob | Op::PutBlobFromUrl => "PutBlob",
        Op::CopyBlob => "CopyBlob",
        Op::AbortCopy => "AbortCopyBlob",
        Op::IncrementalCopy => "IncrementalCopyBlob",
        Op::GetBlob => "GetBlob",
        Op::GetBlobProperties => "GetBlobProperties",
        Op::GetBlobMetadata => "GetBlobMetadata",
        Op::DeleteBlob => "DeleteBlob",
        Op::UndeleteBlob => "UndeleteBlob",
        Op::SetBlobProperties => "SetBlobProperties",
        Op::SetBlobMetadata => "SetBlobMetadata",
        Op::SnapshotBlob => "SnapshotBlob",
        Op::LeaseBlob => match lease {
            Some("renew") => "RenewBlobLease",
            Some("release") => "ReleaseBlobLease",
            Some("break") => "BreakBlobLease",
            Some("change") => "ChangeBlobLease",
            _ => "AcquireBlobLease",
        },
        Op::SetBlobTier => "SetBlobTier",
        Op::GetBlobTags => "GetBlobTags",
        Op::SetBlobTags => "SetBlobTags",
        Op::PutBlock => "PutBlock",
        Op::PutBlockFromUrl => "PutBlockFromURL",
        Op::PutBlockList => "PutBlockList",
        Op::GetBlockList => "GetBlockList",
        Op::AppendBlock | Op::AppendBlockFromUrl => "AppendBlock",
        Op::SealBlob => "SealBlob",
        Op::PutPage | Op::PutPageFromUrl => {
            if req.header("x-ms-page-write").map(|v| v.eq_ignore_ascii_case("clear")).unwrap_or(false) {
                "ClearPage"
            } else {
                "PutPage"
            }
        }
        Op::GetPageRanges => "GetPageRanges",
        Op::SetImmutabilityPolicy => "SetBlobImmutabilityPolicy",
        Op::DeleteImmutabilityPolicy => "DeleteBlobImmutabilityPolicy",
        Op::SetLegalHold => "SetBlobLegalHold",
        Op::SetExpiry => "SetBlobExpiry",
        Op::QueryBlob => "QueryBlobContents",
        Op::Options => "BlobPreflightRequest",
        Op::Unknown => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// Making a line
// ---------------------------------------------------------------------------

/// HTML encode what the format asks to be encoded: quote, ampersand, angle brackets and line breaks.
pub fn html_encode(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '\n' => o.push_str("&#10;"),
            '\r' => o.push_str("&#13;"),
            c => o.push(c),
        }
    }
    o
}

fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// A quoted, encoded field; empty text stays an empty field (as in the examples of the format page).
fn quoted(s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        format!("\"{}\"", html_encode(cut(s, MAX_FIELD)))
    }
}

/// The query string with the value of `sig` replaced (Azure writes `sig=XXXXX`). Nothing else of a credential is in a
/// URL: the account key and the `Authorization` header are never written.
pub fn mask_query(raw: &str) -> String {
    raw.split('&')
        .map(|p| {
            let name = p.split('=').next().unwrap_or("");
            if name.eq_ignore_ascii_case("sig") {
                format!("{name}=XXXXX")
            } else {
                p.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// A URL (the request's or a copy source) with a SAS signature masked.
pub fn mask_url(url: &str) -> String {
    match url.split_once('?') {
        Some((a, q)) => format!("{a}?{}", mask_query(q)),
        None => url.to_string(),
    }
}

/// `Monday, 03-Jan-22 20:34:54 GMT`, the form of the last-modified field.
fn lmt(t: OffsetDateTime) -> String {
    const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{}, {:02}-{}-{:02} {:02}:{:02}:{:02} GMT",
        DAYS[t.weekday().number_days_from_monday() as usize],
        t.day(),
        MONTHS[t.month() as usize - 1],
        t.year().rem_euclid(100),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// How the request was authorized.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Auth {
    Authenticated,
    Sas,
    Bearer,
    Anonymous,
}

fn auth_of(req: &Request) -> Auth {
    match req.header("authorization") {
        Some(a) if a.len() >= 6 && a[..6].eq_ignore_ascii_case("bearer") => Auth::Bearer,
        Some(_) => Auth::Authenticated,
        None if req.query.has("sig") => Auth::Sas,
        None => Auth::Anonymous,
    }
}

/// The status of the request as the log writes it: `Success` (with the prefix of how it was authorized) or, for a
/// failure, the error code (versions 2017-04-17 and later) or the class of failure.
fn status_text(auth: Auth, status: u16, code: Option<&str>) -> String {
    let prefix = match auth {
        Auth::Authenticated => "",
        Auth::Sas => "SAS",
        Auth::Bearer => "OAuth",
        Auth::Anonymous => "Anonymous",
    };
    if (200..300).contains(&status) {
        return format!("{prefix}Success");
    }
    if let Some(c) = code {
        return c.to_string();
    }
    let kind = match status {
        503 => "ThrottlingError",
        500..=599 => "ServerOtherError",
        _ => "ClientOtherError",
    };
    // OAuth requests have no own failure names on the status page; they use the plain ones
    format!("{}{kind}", if auth == Auth::Bearer { "" } else { prefix })
}

/// Whether Azure would write this request down. Authorized requests always; anonymous and SAS requests when they
/// worked, failed with a server error or were a `304` on a GET. A SAS that fails its check is anonymous to the
/// service, so a SAS request refused with `403` is not written.
fn is_logged(auth: Auth, status: u16, method: &str) -> bool {
    match auth {
        Auth::Authenticated | Auth::Bearer => true,
        _ => (200..300).contains(&status) || status >= 500 || (status == 304 && method == "GET"),
    }
}

/// What a finished request tells the log.
pub struct Done<'a> {
    pub req: &'a Request,
    pub resp: &'a Response,
    pub op: Op,
    /// Whole time the request took here, in milliseconds.
    pub millis: u64,
    pub container: Option<&'a str>,
    pub blob: Option<&'a str>,
}

fn header_bytes(h: &azemu_core::http::HeaderMap) -> usize {
    h.iter().map(|(k, v)| k.as_str().len() + v.len() + 4).sum()
}

/// The URL the client used, with the SAS signature masked and the query kept as sent.
fn request_url(req: &Request) -> String {
    let mut u = format!("{}{}", req.ctx.endpoint.base_url(), req.path);
    if !req.query.raw().is_empty() {
        u.push('?');
        u.push_str(&mask_query(req.query.raw()));
    }
    u
}

/// Build the lines of one request (more than one for Copy Blob) and say which class they are. `None`: not logged.
pub fn lines(cfg: &LogCfg, d: &Done) -> Option<Vec<Pending>> {
    let req = d.req;
    if d.op == Op::Unknown || d.container == Some(LOGS) {
        // Reads of the logs are not logged here: with a delay of seconds, reading `$logs` (which a client that
        // follows the logs does every two seconds) would produce a new log blob each time.
        return None;
    }
    let method = req.method.as_str();
    let class = class_of(d.op, method);
    if !cfg.wants(class) {
        return None;
    }
    let status = d.resp.status().as_u16();
    let auth = auth_of(req);
    if !is_logged(auth, status, method) {
        return None;
    }
    let code = d.resp.headers().get("x-ms-error-code").and_then(|v| v.to_str().ok());
    let rh = |n: &str| d.resp.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("");
    let qh = |n: &str| req.header(n).unwrap_or("");
    let account = req.account.to_ascii_lowercase();
    let url = cut(&request_url(req), MAX_URL).to_string();
    let key_of = |c: Option<&str>, b: Option<&str>| match (c, b) {
        (Some(c), Some(b)) => format!("/{account}/{c}/{b}"),
        (Some(c), None) => format!("/{account}/{c}"),
        _ => format!("/{account}"),
    };
    let key = key_of(d.container, d.blob);
    let (auth_name, requester) = match auth {
        Auth::Authenticated => ("authenticated", account.as_str()),
        Auth::Sas => ("sas", ""),
        Auth::Bearer => ("bearer", account.as_str()),
        Auth::Anonymous => ("anonymous", ""),
    };
    let body_len = req.body.len();
    let resp_len = rh("content-length").parse::<usize>().unwrap_or_else(|_| d.resp.body().len());
    let end = req.ctx.time + time::Duration::milliseconds(d.millis as i64);
    let md5_in = if qh("content-md5").is_empty() { qh("x-ms-blob-content-md5") } else { qh("content-md5") };
    let lm = time::parse_rfc1123(rh("last-modified")).map(lmt).unwrap_or_default();
    let mut conditions = vec![];
    for name in ["If-Modified-Since", "If-Unmodified-Since", "If-Match", "If-None-Match"] {
        let v = qh(name);
        if !v.is_empty() {
            conditions.push(format!("{name}={v}"));
        }
    }
    let status_s = status_text(auth, status, code);
    let version = if cfg.v2 { "2.0" } else { "1.0" };

    let one = |name: &str, key: &str, count: u32| -> String {
        let mut f: Vec<String> = vec![
            version.to_string(),
            time::iso8601_7(req.ctx.time),
            name.to_string(),
            status_s.clone(),
            status.to_string(),
            d.millis.to_string(),
            d.millis.to_string(),
            auth_name.to_string(),
            requester.to_string(),
            account.clone(),
            "blob".to_string(),
            quoted(&url),
            quoted(key),
            req.ctx.request_id.clone(),
            count.to_string(),
            // the address of the client is not known to a service (core does not pass it on)
            String::new(),
            qh("x-ms-version").to_string(),
            header_bytes(&req.headers).to_string(),
            body_len.to_string(),
            header_bytes(d.resp.headers()).to_string(),
            resp_len.to_string(),
            body_len.to_string(),
            quoted(md5_in),
            quoted(rh("content-md5")),
            quoted(rh("etag")),
            quoted(&lm),
            quoted(&conditions.join(";")),
            quoted(qh("user-agent")),
            quoted(qh("referer")),
            quoted(qh("x-ms-client-request-id")),
        ];
        if cfg.v2 {
            // format 2.0 adds UserObjectId, TenantId, ApplicationId, Resource ID, Issuer, UserPrincipalName,
            // Reserved and AuthorizationDetail; nothing here knows them
            f.extend(std::iter::repeat_n(String::new(), 8));
        }
        f.join(";")
    };
    let mk = |line: String| Pending { account: account.clone(), end, start: req.ctx.time, class, line };
    let name = operation_name(d.op, req);
    let mut out = vec![mk(one(name, &key, 0))];
    if d.op == Op::CopyBlob {
        // Copy Blob logs three operations with one request id: the key of the source line is the source URL
        let source = mask_url(qh("x-ms-copy-source"));
        out.push(mk(one("CopyBlobSource", &source, 1)));
        out.push(mk(one("CopyBlobDestination", &key, 2)));
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The container and its blobs
// ---------------------------------------------------------------------------

/// Make `$logs` when it is not there. `pin`: logging is switched on, so the container can no longer be deleted.
pub fn ensure_logs(acct: &mut Account, now: Time, pin: bool) {
    if !acct.containers.contains_key(LOGS) {
        acct.containers.insert(LOGS.to_string(), Container::new(LOGS, now, etag::new_etag()));
    }
    if pin {
        acct.logs_pinned = true;
    }
}

/// Make `$web`, the container of a static website, when it is not there. It is an ordinary container (listed, and
/// it can be deleted).
pub fn ensure_web(acct: &mut Account, now: Time) {
    if !acct.containers.contains_key("$web") {
        acct.containers.insert("$web".to_string(), Container::new("$web", now, etag::new_etag()));
    }
}

/// `blob/2026/11/13/1300`: the directory of the log blobs of one hour.
pub fn hour_dir(t: Time) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!("blob/{:04}/{:02}/{:02}/{:02}00", t.year(), t.month() as u8, t.day(), t.hour())
}

fn is_log_blob(name: &str, e: &BlobEntry) -> bool {
    name.starts_with("blob/")
        && e.current.as_ref().map(|b| b.metadata.iter().any(|(k, _)| k.eq_ignore_ascii_case("LogType"))).unwrap_or(false)
}

/// Put one log blob into `$logs`. The metadata is what Azure writes.
#[allow(clippy::too_many_arguments)]
fn store_log(acct: &mut Account, name: &str, text: String, classes: &[Class], first: Time, last: Time, version: &str, now: Time) {
    ensure_logs(acct, now, false);
    let data = azemu_core::bytes::Bytes::from(text.into_bytes());
    let mut b = Blob::new(BlobKind::Block, now);
    b.size = data.len() as u64;
    b.props.content_md5 = Some(md5(&data));
    b.crc64 = Some(crc64(&data));
    let mut kinds: Vec<Class> = classes.to_vec();
    kinds.sort();
    kinds.dedup();
    let log_type = kinds.iter().map(|c| c.name()).collect::<Vec<_>>().join(",");
    b.metadata = vec![
        ("LogType".into(), log_type),
        ("StartTime".into(), time::iso8601_secs(first)),
        ("EndTime".into(), time::iso8601_secs(last)),
        ("LogVersion".into(), version.to_string()),
    ];
    b.segs = vec![Seg { id: None, data }];
    if let Some(c) = acct.containers.get_mut(LOGS) {
        c.blobs.insert(name.to_string(), BlobEntry { current: Some(b), ..BlobEntry::default() });
        c.touch(now);
    }
}

/// The counter after the highest one of an hour that is in the container.
fn next_counter(c: &Container, dir: &str) -> u32 {
    let prefix = format!("{dir}/");
    c.blobs
        .range::<String, _>(prefix.clone()..)
        .take_while(|(k, _)| k.starts_with(&prefix))
        .filter_map(|(k, _)| k[prefix.len()..].strip_suffix(".log").and_then(|n| n.parse::<u32>().ok()))
        .max()
        .map(|m| m + 1)
        .unwrap_or(0)
}

/// Remove log blobs that are older than the retention period, then the oldest ones until the account is inside the hard
/// limits. Only blobs this writer made are touched (a name under `blob/` with the `LogType` metadata). Returns how many
/// blobs went.
pub fn prune(acct: &mut Account, now: Time, cfg: &Config) -> usize {
    let days = LogCfg::from_props(&acct.props).retention_days;
    let Some(c) = acct.containers.get_mut(LOGS) else { return 0 };
    let mut gone = 0;
    if let Some(d) = days {
        let limit = now - time::Duration::days(d as i64);
        let old: Vec<String> = c
            .blobs
            .iter()
            .filter(|(k, e)| is_log_blob(k, e) && e.current.as_ref().map(|b| b.last_modified < limit).unwrap_or(false))
            .map(|(k, _)| k.clone())
            .collect();
        for k in old {
            c.blobs.remove(&k);
            gone += 1;
        }
    }
    let mut names: Vec<(String, u64)> = c
        .blobs
        .iter()
        .filter(|(k, e)| is_log_blob(k, e))
        .map(|(k, e)| (k.clone(), e.current.as_ref().map(|b| b.size).unwrap_or(0)))
        .collect();
    let mut bytes: u64 = names.iter().map(|n| n.1).sum();
    // the names sort by time: the front is the oldest
    let mut i = 0;
    while (names.len() - i > cfg.log_max_blobs as usize || bytes > cfg.log_max_bytes) && i < names.len() {
        // never remove the newest blob, even when it alone is over the byte limit
        if i + 1 == names.len() {
            break;
        }
        c.blobs.remove(&names[i].0);
        bytes -= names[i].1;
        i += 1;
        gone += 1;
    }
    names.clear();
    gone
}

/// Turn a batch into log blobs: one or more per account and hour. Returns (account, blob name, text, classes, first,
/// last, version) without touching the state, so that the work is done before the lock is taken.
struct Planned {
    account: String,
    dir: String,
    text: String,
    classes: Vec<Class>,
    first: Time,
    last: Time,
}

fn plan(items: Vec<Pending>) -> Vec<Planned> {
    let mut groups: BTreeMap<(String, String), Vec<Pending>> = BTreeMap::new();
    for p in items {
        groups.entry((p.account.clone(), hour_dir(p.end))).or_default().push(p);
    }
    let mut out = vec![];
    for ((account, dir), mut lines) in groups {
        lines.sort_by_key(|p| p.start);
        let mut cur: Option<Planned> = None;
        for p in lines {
            let full = cur.as_ref().map(|c| c.text.len() + p.line.len() + 1 > MAX_LOG_BLOB_BYTES).unwrap_or(false);
            if full {
                out.extend(cur.take());
            }
            let c = cur.get_or_insert_with(|| Planned {
                account: account.clone(),
                dir: dir.clone(),
                text: String::new(),
                classes: vec![],
                first: p.start,
                last: p.start,
            });
            c.text.push_str(&p.line);
            c.text.push('\n');
            c.classes.push(p.class);
            c.first = c.first.min(p.start);
            c.last = c.last.max(p.start);
        }
        out.extend(cur);
    }
    out
}

impl Shared {
    /// Make the switches match the service properties of every account (after a load, a reset, a Set Blob Service
    /// Properties).
    pub(crate) fn refresh_log_cfg(&self) {
        let all: HashMap<String, LogCfg> =
            self.state.lock().accounts.iter().map(|(k, a)| (k.clone(), LogCfg::from_props(&a.props))).collect();
        self.logger.set_all(all);
    }

    /// Write everything that is queued as log blobs, and remove what the retention and the limits say. Called by the
    /// timer after the delay, before a save, and by tests.
    pub(crate) fn flush_logs(&self) {
        let items = self.logger.take();
        let planned = plan(items);
        if planned.is_empty() {
            return;
        }
        let now = OffsetDateTime::now_utc();
        let mut st = self.state.lock();
        let config = st.config;
        let mut touched: Vec<String> = vec![];
        for p in planned {
            let Some(acct) = st.accounts.get_mut(&p.account) else { continue };
            ensure_logs(acct, now, false);
            let version = if acct.props.logging.version.trim() == "2.0" { "2.0" } else { "1.0" };
            let existing = acct.containers.get(LOGS).map(|c| next_counter(c, &p.dir)).unwrap_or(0);
            let n = {
                let mut counters = self.logger.counters.lock();
                let slot = counters.entry((p.account.clone(), p.dir.clone())).or_insert(0);
                let n = existing.max(*slot);
                *slot = n + 1;
                // the counters of hours long gone are not needed
                if counters.len() > 512 {
                    let keep = counters.keys().rev().take(256).cloned().collect::<Vec<_>>();
                    counters.retain(|k, _| keep.contains(k));
                }
                n
            };
            let name = format!("{}/{:06}.log", p.dir, n);
            store_log(acct, &name, p.text, &p.classes, p.first, p.last, version, now);
            if !touched.contains(&p.account) {
                touched.push(p.account);
            }
        }
        for a in touched {
            if let Some(acct) = st.accounts.get_mut(&a) {
                prune(acct, now, &config);
            }
        }
        // a change a save has to write
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Queue the lines of a finished request and make sure a flush is coming.
    pub(crate) fn log_request(self: &std::sync::Arc<Self>, d: &Done) {
        let Some(cfg) = self.logger.cfg_for(&d.req.account.to_ascii_lowercase()) else { return };
        let Some(lines) = lines(&cfg, d) else { return };
        self.logger.push(lines);
        self.arm();
    }

    fn arm(self: &std::sync::Arc<Self>) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        if self.logger.armed.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        let delay = self.state.lock().config.log_delay;
        rt.spawn(async move {
            tokio::time::sleep(delay).await;
            // lines that arrive from here on start the next round
            me.logger.armed.store(false, Ordering::Release);
            me.flush_logs();
        });
    }
}

#[cfg(test)]
#[path = "analytics_tests.rs"]
mod tests;
