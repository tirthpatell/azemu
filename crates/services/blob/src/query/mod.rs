//! Query Blob Contents (`POST ...?comp=query`): a SQL query over the data of a block blob, answered as an Avro
//! stream of results, progress and errors. See `docs/services/blob-query.md`.
//!
//! Where the work happens:
//! 1. before the state lock (`pre::prepare`): the request body is read and the expression parsed;
//! 2. under the lock (`query_blob`): the blob is found, its conditions, lease and key are checked, and its data
//!    is taken out as shared `Bytes` pieces (no copy, the cost is the number of blocks) with the answer's headers;
//! 3. after the lock (`finish`): at most `Limits::max_concurrent` (4) queries run at a time, each on a blocking thread,
//!    reading the pieces record by record (`exec`). The answer is streamed: the scan writes it into a small channel and
//!    core sends it as the client reads (a client that reads slowly makes the scan wait, memory stays about two
//!    chunks). The place is held until the answer has been sent or the client is gone; the scan also stops, within
//!    milliseconds, when the request is cancelled (the client closed the connection).

mod ast;
mod avro;
mod budget;
mod datetime;
mod eval;
mod exec;
mod input;
mod json;
mod lexer;
mod like;
mod output;
mod request;
mod sql;
#[cfg(test)]
mod sql_tests;
mod value;

use azemu_core::resp::Resp;
use azemu_core::Response;
use std::sync::Arc;

use crate::auth::apply_sas_overrides;
use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, snapshot_param, version_param, Cx};
use crate::errors::{blob_not_found, err, ApiError, ApiResult};
use crate::headers::blob_headers;
use crate::lease::{self, Scope};
use crate::state::{BlobKind, Piece};
use crate::store::check_cpk;
use crate::util::hdr;

pub use exec::Limits;
pub use request::Plan;

/// What waits for a place to run after the lock is released.
pub struct QueryJob {
    plan: Arc<Plan>,
    pieces: Vec<Piece>,
    limits: Limits,
    pub dfs: bool,
}

/// The queries running now.
pub struct Gate(Arc<tokio::sync::Semaphore>);

impl Gate {
    pub fn new(places: usize) -> Gate {
        Gate(Arc::new(tokio::sync::Semaphore::new(places)))
    }
}

impl Default for Gate {
    fn default() -> Gate {
        Gate::new(Limits::default().max_concurrent)
    }
}

/// The parsed request, for `Pre`.
pub type Prepared = Result<Arc<Plan>, ApiError>;

/// Read and check the request body and parse the expression (no lock held).
pub fn prepare(body: &[u8]) -> Prepared {
    request::parse(body).map(Arc::new)
}

/// The handler: everything that needs the state. `dfs`: the request came through the Data Lake endpoint.
pub fn query_blob(cx: &mut Cx, container: &str, name: &str, dfs: bool) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let snapshot = snapshot_param(req)?;
    let version = version_param(req)?;
    let limits = cx.st.config.query;
    let authz = cx.authz.clone();
    let prepared = cx.pre.query.clone();
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let is_current = e.live().map(|b| b.version_id.as_deref()) == Some(version.as_deref()) && version.is_some()
        || (snapshot.is_none() && version.is_none());
    let b = e.find(snapshot.as_deref(), version.as_deref()).ok_or_else(blob_not_found)?;

    check_cpk(b, req)?;
    if snapshot.is_none() && version.is_none() {
        lease::check_read(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    }
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Read)?;
    cond::check_tags(req, Some(&b.tags))?;
    if b.kind != BlobKind::Block {
        return Err(
            err("InvalidBlobType").msg("The blob type is invalid for this operation: Query Blob Contents works on block blobs only.")
        );
    }
    if b.tier.as_deref() == Some("Archive") {
        return Err(err("BlobArchived"));
    }
    if b.size > limits.max_blob_bytes {
        return Err(
            ApiError::new("InvalidInput").msg(format!("The blob is larger than this emulator queries ({} bytes).", limits.max_blob_bytes))
        );
    }
    let plan = match prepared {
        Some(p) => p?,
        // not prepared (a request that did not come through `handle`): read it here
        None => prepare(&req.body)?,
    };
    let pieces = b.read_pieces(0, b.size);
    let r = blob_headers(Resp::new(200u16), b, is_current).header("Content-Type", "avro/binary");
    let r = apply_sas_overrides(r, &authz).header("Content-Type", "avro/binary");
    cx.st.query_job = Some(QueryJob { plan, pieces, limits, dfs });
    Ok(r.build())
}

/// Why a query could not run.
pub enum Refused {
    Busy,
}

/// How many pieces of the answer wait between the scan and the connection (each about `data_chunk_bytes`).
const PIECES_IN_FLIGHT: usize = 2;

/// The answer as core reads it. Holds the place in the gate for as long as the answer is being sent: when core drops it
/// (the answer is complete, the client went away, or the client stopped reading and the connection was closed), and the
/// scan has ended too, the place is free again.
struct Answer {
    pieces: tokio::sync::mpsc::Receiver<std::io::Result<azemu_core::bytes::Bytes>>,
    _place: Arc<tokio::sync::OwnedSemaphorePermit>,
}

impl azemu_core::stream::BodyStream for Answer {
    fn poll_chunk(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<std::io::Result<azemu_core::bytes::Bytes>>> {
        self.pieces.poll_recv(cx)
    }
}

/// Start the job: wait for a place, then run the scan on a blocking thread feeding the returned answer. `cancel` is the
/// token of the request: when it fires the scan stops.
async fn start(job: QueryJob, gate: &Gate, cancel: azemu_core::CancelToken) -> Result<Answer, Refused> {
    let place = match tokio::time::timeout(job.limits.queue_wait, gate.0.clone().acquire_owned()).await {
        Ok(Ok(p)) => Arc::new(p),
        _ => return Err(Refused::Busy),
    };
    let (tx, rx) = tokio::sync::mpsc::channel(PIECES_IN_FLIGHT);
    let scan_place = place.clone();
    tokio::task::spawn_blocking(move || {
        let _place = scan_place;
        let sent = tx.clone();
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // a full channel makes `blocking_send` wait: the scan goes at the speed of the client
            exec::run_streaming(&job.plan, job.pieces, &job.limits, &cancel, |part| sent.blocking_send(Ok(part.into())).is_ok());
        }));
        if ran.is_err() {
            // the status line has gone out already; the answer is cut off so the client sees a failure, not a short answer
            let _ = tx.blocking_send(Err(std::io::Error::other("the query failed inside the emulator")));
        }
    });
    Ok(Answer { pieces: rx, _place: place })
}

/// Run the job and make the answer a stream.
pub async fn finish(resp: &mut Response, job: QueryJob, gate: &Gate, cancel: azemu_core::CancelToken) -> Result<(), Refused> {
    let answer = start(job, gate, cancel).await?;
    azemu_core::stream::attach(resp, answer);
    Ok(())
}

pub fn refused(r: Refused) -> ApiError {
    match r {
        Refused::Busy => {
            ApiError::new("ServerBusy").status(503).msg("The server is busy: too many queries are already running. Try again in a moment.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_core::bytes::Bytes;
    use azemu_core::CancelToken;
    use std::time::{Duration, Instant};

    fn job(expr: &str, data: Bytes, limits: Limits) -> QueryJob {
        let plan =
            Arc::new(exec::tests::request(expr, "", "<OutputSerialization><Format><Type>json</Type></Format></OutputSerialization>"));
        QueryJob { plan, pieces: vec![Piece::Data(data)], limits, dfs: false }
    }

    fn rows() -> Bytes {
        Bytes::from("abcdefghij,klmnopqrst\n".repeat(60_000))
    }

    fn small_chunks() -> Limits {
        Limits { data_chunk_bytes: 16 * 1024, max_answer_bytes: usize::MAX, ..Limits::default() }
    }

    async fn until(what: &str, mut cond: impl FnMut() -> bool) {
        let t = Instant::now();
        while !cond() {
            assert!(t.elapsed() < Duration::from_secs(10), "gave up waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The answer is not built in memory. A client that reads nothing makes the scan wait after a couple of pieces.
    #[tokio::test]
    async fn the_scan_waits_for_a_client_that_does_not_read() {
        let gate = Gate::new(1);
        let mut a = start(job("SELECT _1, _2, _1, _2, _1, _2 FROM BlobStorage", rows(), small_chunks()), &gate, CancelToken::new())
            .await
            .ok()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(a.pieces.len() <= PIECES_IN_FLIGHT, "{} pieces wait in memory", a.pieces.len());
        assert_eq!(gate.0.available_permits(), 0, "the place is held while the answer is unread");
        // reading everything gives the whole answer (about 6 MB from a 1.3 MB blob), and then the place is free
        let mut total = 0;
        let mut pieces = 0;
        while let Some(p) = std::future::poll_fn(|cx| azemu_core::stream::BodyStream::poll_chunk(&mut a, cx)).await {
            total += p.unwrap().len();
            pieces += 1;
        }
        assert!(total > 5_000_000 && pieces > 300, "{total} bytes in {pieces} pieces");
        drop(a);
        until("the place is free", || gate.0.available_permits() == 1).await;
    }

    #[tokio::test]
    async fn dropping_the_answer_stops_the_scan_and_frees_the_place() {
        let gate = Gate::new(1);
        let a = start(job("SELECT _1, _2, _1, _2, _1, _2 FROM BlobStorage", rows(), small_chunks()), &gate, CancelToken::new())
            .await
            .ok()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(a);
        until("the place is free", || gate.0.available_permits() == 1).await;
    }

    /// The scan is busy (not waiting to send) when the request is cancelled: it notices on its own.
    #[tokio::test]
    async fn a_cancelled_request_stops_a_busy_scan() {
        let gate = Gate::new(1);
        // nothing matches, so nothing is ever sent; every record costs a pattern match
        let data = Bytes::from(format!("{}\n", "a".repeat(2_000_000)).repeat(20));
        let expr = format!("SELECT _1 FROM BlobStorage WHERE _1 LIKE '%{}b%'", "a_".repeat(100));
        let token = CancelToken::new();
        let a = start(job(&expr, data, Limits::default()), &gate, token.clone()).await.ok().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(gate.0.available_permits(), 0);
        let mut a = a;
        let t = Instant::now();
        token.cancel();
        // the scan ends without sending anything more: the answer ends (core has dropped it by then)
        let next =
            tokio::time::timeout(Duration::from_secs(2), std::future::poll_fn(|cx| azemu_core::stream::BodyStream::poll_chunk(&mut a, cx)))
                .await;
        assert!(matches!(next, Ok(None)), "the scan did not stop");
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        drop(a);
        until("the place is free", || gate.0.available_permits() == 1).await;
    }

    #[tokio::test]
    async fn a_busy_gate_refuses_after_the_wait() {
        let gate = Gate::new(1);
        let limits = Limits { queue_wait: Duration::from_millis(100), ..small_chunks() };
        let _held = start(job("SELECT * FROM BlobStorage", rows(), limits), &gate, CancelToken::new()).await.ok().unwrap();
        let second = start(job("SELECT * FROM BlobStorage", rows(), limits), &gate, CancelToken::new()).await;
        assert!(matches!(second, Err(Refused::Busy)));
    }
}
