//! Storage events: what Blob and Data Lake tell Event Grid about.
//!
//! The blob crate does not know Event Grid. The unified binary sets an event sink
//! ([`BlobService::set_event_sink`](crate::BlobService::set_event_sink)); this module builds
//! the events and hands them over.
//!
//! How it works:
//!
//! * Before a request is handled, the service asks the sink whether anyone listens to the
//!   account ([`StorageEventSink::wants`]). If not, nothing below runs.
//! * While the request runs, [`before_blob`] / [`before_dfs`] note the few facts that are gone
//!   after the operation (the content type of a blob that is deleted, the tier before a tier
//!   change), and [`after_blob`] / [`after_dfs`] build the events from the state the operation
//!   left behind. Only an operation that succeeded makes an event. The events wait in
//!   `State::events`; nothing leaves the blob lock yet.
//! * After the lock is released, the service passes the events to the sink.
//!
//! The shapes follow the system event schemas in
//! `refs/specs/specification/eventgrid/data-plane/EventGridSystemEvents` (Storage.tsp and the
//! examples) and the Microsoft Learn page "Azure Blob Storage as an Event Grid source".
//! `docs/services/storage-events.md` lists where those two disagree and which one was followed.

use std::sync::atomic::{AtomicU64, Ordering};

use azemu_core::http::Method;
use azemu_core::time::OffsetDateTime;
use azemu_core::{Request, Response};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde_json::{json, Map, Value};

use crate::dfs::model::is_dir;
use crate::ops::Op;
use crate::state::{Blob, State};

/// One event, before Event Grid wraps it (topic, `metadataVersion` and the schema are Event
/// Grid's business).
#[derive(Clone, Debug)]
pub struct StorageEvent {
    /// Lower-case account name.
    pub account: String,
    /// `Microsoft.Storage.BlobCreated` and so on.
    pub event_type: String,
    /// `/blobServices/default/containers/{container}/blobs/{blob}`
    pub subject: String,
    /// The `data` object.
    pub data: Value,
    /// `""` for the blob endpoint, `"2"` for Data Lake create and delete of a file, `"1"` for
    /// the other Data Lake events (as on the Learn page).
    pub data_version: String,
    /// Unique id of the event.
    pub id: String,
    /// When the operation ran (the request time).
    pub time: OffsetDateTime,
}

/// Where events go. Both methods are called without any blob lock held.
pub trait StorageEventSink: Send + Sync {
    /// Does anyone listen to events of this account? Called for every request, so it must be
    /// cheap. When it says no, no event is built.
    fn wants(&self, account: &str) -> bool;
    /// Take one event. Must not block and must not fail the request.
    fn raise(&self, event: StorageEvent);
}

static SEQUENCER: AtomicU64 = AtomicU64::new(1);

/// An opaque, ever increasing string (48 characters, like the ones Azure sends).
fn sequencer() -> String {
    format!("{:032x}{:016x}", 0u128, SEQUENCER.fetch_add(1, Ordering::Relaxed))
}

const PATH_SET: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'<').add(b'>').add(b'?').add(b'`').add(b'{').add(b'}');

/// What is lost once the operation has run.
#[derive(Clone, Debug, Default)]
pub(crate) struct Pre {
    content_type: String,
    blob_type: &'static str,
    size: u64,
    /// The explicitly set tier before the operation (`None` when the tier is inferred).
    tier: Option<String>,
    is_dir: bool,
}

impl Pre {
    fn of(b: &Blob) -> Pre {
        Pre { content_type: b.props.content_type.clone(), blob_type: b.kind.name(), size: b.size, tier: b.tier.clone(), is_dir: is_dir(b) }
    }
}

fn find<'a>(st: &'a State, acct: &str, container: &str, name: &str) -> Option<&'a Blob> {
    st.accounts.get(acct)?.containers.get(container)?.blobs.get(name)?.live()
}

/// Learn: "The `$logs` and `$blobchangefeed` containers aren't integrated with Event Grid, so
/// activity in these containers will not generate events."
fn no_events_container(name: &str) -> bool {
    name == "$logs" || name == "$blobchangefeed"
}

fn snapshot_or_version(req: &Request) -> bool {
    req.query.has("snapshot") || req.query.has("versionid")
}

/// Facts to remember before a Blob request runs. `None`: the operation needs none.
pub(crate) fn before_blob(st: &State, acct: &str, op: Op, container: Option<&str>, blob: Option<&str>) -> Option<Pre> {
    if !matches!(op, Op::DeleteBlob | Op::SetBlobTier) {
        return None;
    }
    find(st, acct, container?, blob?).map(Pre::of)
}

/// Facts to remember before a Data Lake request runs.
pub(crate) fn before_dfs(st: &State, acct: &str, op: crate::dfs::ops::Op, fs: Option<&str>, path: &str) -> Option<Pre> {
    if !matches!(op, crate::dfs::ops::Op::DeletePath) {
        return None;
    }
    find(st, acct, fs?, path).map(Pre::of)
}

/// Where the data of a request starts: `api`, `clientRequestId`, `requestId`.
fn head(req: &Request, api: &str) -> Map<String, Value> {
    let mut d = Map::new();
    d.insert("api".into(), json!(api));
    if let Some(id) = req.header("x-ms-client-request-id") {
        d.insert("clientRequestId".into(), json!(id));
    }
    d.insert("requestId".into(), json!(req.ctx.request_id));
    d
}

/// `sequencer` and `storageDiagnostics`, which end every event.
fn tail(d: &mut Map<String, Value>) {
    d.insert("sequencer".into(), json!(sequencer()));
    d.insert("storageDiagnostics".into(), json!({ "batchId": uuid::Uuid::new_v4().to_string() }));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Blob,
    Dfs,
}

/// The URL of a path on the blob or the Data Lake endpoint. It starts from the URL the client
/// used, with the service label swapped when the other endpoint is wanted.
fn url_of(req: &Request, from: Endpoint, want: Endpoint, container: &str, name: &str) -> String {
    let mut base = req.ctx.endpoint.base_url();
    if from != want {
        let (a, b) = if want == Endpoint::Blob { (".dfs.", ".blob.") } else { (".blob.", ".dfs.") };
        base = base.replacen(a, b, 1);
    }
    let path: Vec<String> = name.split('/').map(|s| utf8_percent_encode(s, PATH_SET).to_string()).collect();
    format!("{}/{}/{}", base.trim_end_matches('/'), utf8_percent_encode(container, PATH_SET), path.join("/"))
}

fn subject(container: &str, name: &str) -> String {
    format!("/blobServices/default/containers/{container}/blobs/{name}")
}

fn event(
    req: &Request,
    acct: &str,
    event_type: &str,
    container: &str,
    name: &str,
    data: Map<String, Value>,
    version: &str,
) -> StorageEvent {
    StorageEvent {
        account: acct.to_string(),
        event_type: format!("Microsoft.Storage.{event_type}"),
        subject: subject(container, name),
        data: Value::Object(data),
        data_version: version.to_string(),
        id: uuid::Uuid::new_v4().to_string(),
        time: req.ctx.time,
    }
}

#[allow(clippy::too_many_arguments)]
fn blob_created(req: &Request, acct: &str, c: &str, name: &str, b: &Blob, api: &str, ep: Endpoint, from: Endpoint) -> StorageEvent {
    let mut d = head(req, api);
    d.insert("eTag".into(), json!(b.etag.trim_matches('"')));
    d.insert("contentType".into(), json!(b.props.content_type));
    d.insert("contentLength".into(), json!(b.size));
    if ep == Endpoint::Dfs {
        d.insert("contentOffset".into(), json!(req.query.get("position").and_then(|p| p.parse::<u64>().ok()).unwrap_or(0)));
    }
    d.insert("blobType".into(), json!(b.kind.name()));
    d.insert("accessTier".into(), json!(b.tier.clone().unwrap_or_else(|| "Default".into())));
    d.insert("url".into(), json!(url_of(req, from, ep, c, name)));
    tail(&mut d);
    event(req, acct, "BlobCreated", c, name, d, if ep == Endpoint::Dfs { "2" } else { "" })
}

/// Build the events of a Blob request that succeeded and queue them in `st.events`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn after_blob(
    st: &mut State,
    req: &Request,
    acct: &str,
    op: Op,
    container: Option<&str>,
    blob: Option<&str>,
    pre: Option<Pre>,
    resp: &Response,
) {
    if !resp.status().is_success() {
        return;
    }
    let (Some(c), Some(name)) = (container, blob) else { return };
    if no_events_container(c) {
        return;
    }
    let ev = match op {
        Op::PutBlob | Op::PutBlobFromUrl | Op::PutBlockList | Op::CopyBlob => {
            if snapshot_or_version(req) {
                return;
            }
            // A copy that is not finished raises nothing yet (the emulator copies at once, so
            // this only guards the header).
            if op == Op::CopyBlob && resp.headers().get("x-ms-copy-status").and_then(|v| v.to_str().ok()) != Some("success") {
                return;
            }
            let api = match op {
                Op::PutBlob => "PutBlob",
                Op::PutBlobFromUrl => "PutBlobFromUrl",
                Op::PutBlockList => "PutBlockList",
                _ => "CopyBlob",
            };
            let Some(b) = find(st, acct, c, name) else { return };
            blob_created(req, acct, c, name, b, api, Endpoint::Blob, Endpoint::Blob)
        }
        Op::DeleteBlob => {
            let Some(pre) = pre else { return };
            let only_snapshots = req.header("x-ms-delete-snapshots").map(|v| v.eq_ignore_ascii_case("only")).unwrap_or(false);
            if snapshot_or_version(req) || only_snapshots {
                return;
            }
            let mut d = head(req, "DeleteBlob");
            d.insert("contentType".into(), json!(pre.content_type));
            d.insert("blobType".into(), json!(pre.blob_type));
            d.insert("url".into(), json!(url_of(req, Endpoint::Blob, Endpoint::Blob, c, name)));
            tail(&mut d);
            event(req, acct, "BlobDeleted", c, name, d, "")
        }
        Op::SetBlobTier => {
            let Some(pre) = pre else { return };
            if snapshot_or_version(req) {
                return;
            }
            let Some(b) = find(st, acct, c, name) else { return };
            // Leaving the archive tier is a rehydration: it ends later, so the answer is 202
            // and Event Grid gets "AsyncOperationInitiated" instead.
            let rehydrating = resp.status().as_u16() == 202;
            let mut d = head(req, "SetBlobTier");
            d.insert("contentType".into(), json!(pre.content_type));
            d.insert("contentLength".into(), json!(pre.size));
            d.insert("blobType".into(), json!(pre.blob_type));
            if !rehydrating {
                d.insert("accessTier".into(), json!(b.tier.clone().unwrap_or_else(|| "Default".into())));
                // Learn: "If the blob is inferring the tier from the storage account, this
                // field will not appear."
                if let Some(prev) = &pre.tier {
                    d.insert("previousTier".into(), json!(prev));
                }
            }
            d.insert("url".into(), json!(url_of(req, Endpoint::Blob, Endpoint::Blob, c, name)));
            tail(&mut d);
            event(req, acct, if rehydrating { "AsyncOperationInitiated" } else { "BlobTierChanged" }, c, name, d, "")
        }
        _ => return,
    };
    st.events.push(ev);
}

/// Build the events of a Data Lake request that succeeded and queue them in `st.events`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn after_dfs(
    st: &mut State,
    req: &Request,
    acct: &str,
    op: crate::dfs::ops::Op,
    fs: Option<&str>,
    path: &str,
    pre: Option<Pre>,
    resp: &Response,
) {
    use crate::dfs::ops::Op as D;
    if !resp.status().is_success() {
        return;
    }
    let Some(c) = fs else { return };
    if path.is_empty() || no_events_container(c) {
        return;
    }
    let ev = match op {
        D::CreatePath { dir: false } => {
            let Some(b) = find(st, acct, c, path) else { return };
            blob_created(req, acct, c, path, b, "CreateFile", Endpoint::Dfs, Endpoint::Dfs)
        }
        D::CreatePath { dir: true } => {
            let Some(b) = find(st, acct, c, path) else { return };
            let mut d = head(req, "CreateDirectory");
            d.insert("eTag".into(), json!(b.etag.trim_matches('"')));
            d.insert("blobUrl".into(), json!(url_of(req, Endpoint::Dfs, Endpoint::Blob, c, path)));
            d.insert("url".into(), json!(url_of(req, Endpoint::Dfs, Endpoint::Dfs, c, path)));
            tail(&mut d);
            event(req, acct, "DirectoryCreated", c, path, d, "1")
        }
        D::Flush => {
            // Only the flush that closes the file commits it ("FlushWithClose").
            let close = req.query.get("close").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false);
            if !close {
                return;
            }
            let Some(b) = find(st, acct, c, path) else { return };
            blob_created(req, acct, c, path, b, "FlushWithClose", Endpoint::Dfs, Endpoint::Dfs)
        }
        D::DeletePath => {
            let Some(pre) = pre else { return };
            if pre.is_dir {
                let recursive = req.query.get("recursive").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false);
                let mut d = head(req, "DeleteDirectory");
                d.insert("blobUrl".into(), json!(url_of(req, Endpoint::Dfs, Endpoint::Blob, c, path)));
                d.insert("url".into(), json!(url_of(req, Endpoint::Dfs, Endpoint::Dfs, c, path)));
                d.insert("recursive".into(), json!(recursive.to_string()));
                tail(&mut d);
                event(req, acct, "DirectoryDeleted", c, path, d, "1")
            } else {
                let mut d = head(req, "DeleteFile");
                d.insert("contentType".into(), json!(pre.content_type));
                d.insert("blobType".into(), json!(pre.blob_type));
                d.insert("url".into(), json!(url_of(req, Endpoint::Dfs, Endpoint::Dfs, c, path)));
                tail(&mut d);
                event(req, acct, "BlobDeleted", c, path, d, "2")
            }
        }
        D::RenamePath => {
            if req.method != Method::PUT {
                return;
            }
            let Ok((src_fs, src)) = crate::dfs::rename_source(req.header("x-ms-rename-source").unwrap_or("")) else { return };
            if src_fs == c && src == path {
                return;
            }
            let Some(b) = find(st, acct, c, path) else { return };
            let dir = is_dir(b);
            let mut d = head(req, if dir { "RenameDirectory" } else { "RenameFile" });
            d.insert("destinationBlobUrl".into(), json!(url_of(req, Endpoint::Dfs, Endpoint::Blob, c, path)));
            d.insert("sourceBlobUrl".into(), json!(url_of(req, Endpoint::Dfs, Endpoint::Blob, &src_fs, &src)));
            d.insert("destinationUrl".into(), json!(url_of(req, Endpoint::Dfs, Endpoint::Dfs, c, path)));
            d.insert("sourceUrl".into(), json!(url_of(req, Endpoint::Dfs, Endpoint::Dfs, &src_fs, &src)));
            tail(&mut d);
            event(req, acct, if dir { "DirectoryRenamed" } else { "BlobRenamed" }, c, path, d, "1")
        }
        _ => return,
    };
    st.events.push(ev);
}
