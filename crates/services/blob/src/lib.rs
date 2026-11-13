//! Azure Blob Storage for azemu.
//!
//! `http://{account}.blob.localhost:{port}/{container}/{blob}` on the gateway and
//! `http://127.0.0.1:{port}/{account}/{container}/{blob}` on a pinned port.
//! See `docs/services/blob.md` for what is built.

mod analytics;
mod append;
mod auth;
mod batch;
mod blob;
mod block;
mod cond;
mod container;
mod copy;
mod cpk;
mod cx;
mod dfs;
mod disk;
mod errors;
mod events;
mod headers;
mod lease;
mod list;
mod names;
mod ops;
mod page;
mod pre;
mod props;
mod query;
mod router;
mod service_props;
mod state;
mod store;
mod structured;
mod tags;
mod util;
mod xml;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::storage::StorageAccounts;
use azemu_core::{async_trait, Mount, Request, Response, Service};
use parking_lot::Mutex;

use crate::pre::Pre;
use crate::state::{Config, State};

pub use crate::dfs::DataLakeService;
pub use crate::events::{StorageEvent, StorageEventSink};
pub use crate::query::Limits as QueryLimits;
use crate::query::Limits;

/// What the Blob and the Data Lake service share: one set of accounts, containers and blobs.
#[derive(Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
    /// Account keys, for checking signatures. Derived from the account name, so
    /// `reset()` does not touch them.
    accounts: StorageAccounts,
    /// Where storage events go (see `events.rs`). Unset: no events are built.
    sink: parking_lot::RwLock<Option<Arc<dyn StorageEventSink>>>,
    /// How long the state lock was held by one request: the longest, and the total.
    hold: HoldStats,
    /// How many queries (Query Blob Contents) run at once.
    query_gate: query::Gate,
    /// Raised on every change to the state that a save would write (see `persist_generation`).
    generation: AtomicU64,
    /// Which buffers are already in piece files (see `disk.rs`).
    disk: Mutex<disk::DiskIndex>,
    /// Storage Analytics logging: the queue of log lines and the switches (see `analytics.rs`).
    logger: analytics::Logger,
}

/// Time the state lock was held, kept so that a test can check that no request holds it for
/// work that grows with the size of a body.
#[derive(Default)]
pub(crate) struct HoldStats {
    max_ns: AtomicU64,
    total_ns: AtomicU64,
    count: AtomicU64,
}

impl HoldStats {
    fn record(&self, held: Duration) {
        let ns = held.as_nanos().min(u64::MAX as u128) as u64;
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        self.total_ns.fetch_add(ns, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

impl Shared {
    /// Run one request: what depends on the size of a body happens before the lock is taken
    /// (`pre`) or after it is released; the lock is taken and released inside this call, never
    /// across an `.await`.
    pub(crate) async fn serve(&self, req: &Request, mut pre: Pre, run: impl Fn(&mut State, &Pre) -> Response) -> Response {
        let account = req.account.to_ascii_lowercase();
        let mut round = 0;
        loop {
            round += 1;
            let reads =
                matches!(req.method, azemu_core::http::Method::GET | azemu_core::http::Method::HEAD | azemu_core::http::Method::OPTIONS);
            let (mut resp, wait, out, query) = self.run_with_events(&account, |st| {
                let r = run(st, &pre);
                // a read changes nothing a save would write, except where it sets `changed` itself
                st.changed |= !reads;
                (r, st.wait_source.take(), st.out_body.take(), st.query_job.take())
            });
            match wait {
                // A from-URL request wants the source data joined and hashed first; run it again.
                Some(w) if round < 3 => {
                    pre.prefetch(w).await;
                    continue;
                }
                _ => {}
            }
            if let Some(o) = out {
                pre::finish_out(&mut resp, o).await;
            }
            if let Some(q) = query {
                // the query runs here, after the lock is released
                let dfs = q.dfs;
                if let Err(why) = query::finish(&mut resp, q, &self.query_gate, req.ctx.cancel_token()).await {
                    let e = query::refused(why);
                    resp = if dfs { crate::dfs::error_response(e, &req.ctx) } else { e.into_response(&req.ctx) };
                    resp = azemu_core::storage::finish(resp, req, if dfs { dfs::SERVER } else { router::SERVER });
                }
            }
            return resp;
        }
    }

    /// Drop all data. The settings from the environment stay; a per-account versioning switch does not.
    pub(crate) fn reset(&self) {
        let mut st = self.state.lock();
        let config = st.config;
        *st = State::with_config(config);
        drop(st);
        self.logger.clear();
        self.logger.set_all(Default::default());
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Run `f` with the state locked and pass the events it built to the sink after the lock
    /// is released. `account` is the lower-case account name of the request.
    pub(crate) fn run_with_events<R>(&self, account: &str, f: impl FnOnce(&mut State) -> R) -> R {
        let sink = self.sink.read().clone();
        let wanted = sink.as_ref().map(|s| s.wants(account)).unwrap_or(false);
        let (out, events) = {
            let mut st = self.state.lock();
            let since = Instant::now();
            st.events_wanted = wanted;
            let out = f(&mut st);
            st.events_wanted = false;
            if std::mem::take(&mut st.changed) {
                // inside the lock and after the change: a save that sees the new number also sees the change
                self.generation.fetch_add(1, Ordering::SeqCst);
            }
            let events = std::mem::take(&mut st.events);
            self.hold.record(since.elapsed());
            (out, events)
        };
        if let Some(sink) = sink {
            for e in events {
                sink.raise(e);
            }
        }
        out
    }
}

/// Shared by the Blob and the Data Lake service. See `Service::abort_on_disconnect`.
pub(crate) fn abort_on_disconnect(method: &azemu_core::http::Method, path: &str) -> bool {
    use azemu_core::http::Method;
    match *method {
        Method::GET => true,
        Method::POST => path.trim_matches('/').split('/').filter(|s| !s.is_empty()).count() >= 2,
        _ => false,
    }
}

#[derive(Default)]
pub struct BlobService {
    shared: Arc<Shared>,
}

impl BlobService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Send storage events (`Microsoft.Storage.BlobCreated` and so on) to `sink`. The Data Lake
    /// service shares the setting. See `events.rs`.
    pub fn set_event_sink(&self, sink: Arc<dyn StorageEventSink>) {
        *self.shared.sink.write() = Some(sink);
    }

    /// A service with explicit versioning settings instead of the environment variables:
    /// `versioning` is the default for every account, `old_version_bytes` the limit for the bytes
    /// the previous versions of one account may hold (0: none are kept). Soft-deleted snapshots
    /// keep their default limit.
    pub fn with_versioning(versioning: bool, old_version_bytes: u64) -> Self {
        let config = Config { versioning, version_bytes: old_version_bytes, ..Config::from_env() };
        BlobService { shared: Arc::new(Shared { state: Mutex::new(State::with_config(config)), ..Shared::default() }) }
    }

    /// A service with explicit Storage Analytics logging settings instead of the environment variables (used by tests):
    /// the delay before log lines are written, and the limits on log blobs per account.
    #[doc(hidden)]
    pub fn with_log_settings(delay: Duration, max_blobs: u64, max_bytes: u64) -> Self {
        let config = Config { log_delay: delay, log_max_blobs: max_blobs, log_max_bytes: max_bytes, ..Config::from_env() };
        BlobService { shared: Arc::new(Shared { state: Mutex::new(State::with_config(config)), ..Shared::default() }) }
    }

    /// A service with explicit Query Blob Contents limits instead of the environment variables (used by tests).
    pub fn with_query_limits(limits: Limits) -> Self {
        let config = Config { query: limits, ..Config::from_env() };
        BlobService {
            shared: Arc::new(Shared {
                state: Mutex::new(State::with_config(config)),
                query_gate: query::Gate::new(limits.max_concurrent),
                ..Shared::default()
            }),
        }
    }

    /// Switch versioning on or off for one account (`isVersioningEnabled` of the Blob service
    /// properties in the management API). The account is created if it does not exist yet.
    /// Turning it off keeps the versions that exist; new writes no longer create any, carry no
    /// `x-ms-version-id`, and a request for a `versionid` that does not exist is `404 BlobNotFound`.
    pub fn set_versioning_enabled(&self, account: &str, enabled: bool) {
        self.shared.state.lock().account(account).versioning = enabled;
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Say whether the account allows public access to its containers (`allowBlobPublicAccess` in the management API).
    /// The account is created if it does not exist yet. A container that is already public stays so, but an
    /// anonymous request to it is refused (409 `PublicAccessNotPermitted`) while the setting is off, and no container
    /// can be made public.
    pub fn set_public_access_allowed(&self, account: &str, allowed: bool) {
        self.shared.state.lock().account(account).public_access_allowed = allowed;
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Tests only: when buffers get their own data file, how big a pack of small ones gets, and how many bytes
    /// of a pack must be dead before it is rewritten.
    #[doc(hidden)]
    pub fn set_piece_policy_for_tests(&self, large: usize, pack_target: usize, min_waste: u64) {
        self.shared.disk.lock().policy = disk::Policy { large, pack_target, min_waste };
    }

    /// Whether versioning is on for the account. Accounts that do not exist yet follow the
    /// default (`AZEMU_BLOB_VERSIONING`: off, as on Azure, unless `AZEMU_LENIENT` or the variable itself says on).
    pub fn versioning_enabled(&self, account: &str) -> bool {
        let st = self.shared.state.lock();
        st.accounts.get(&account.to_ascii_lowercase()).map(|a| a.versioning).unwrap_or(st.config.versioning)
    }

    /// Tests only: write the queued Storage Analytics log lines now instead of after the delay.
    #[doc(hidden)]
    pub fn flush_logs_for_tests(&self) {
        self.shared.flush_logs();
    }

    /// Tests only: log lines that were not queued because the queue was full, and how many wait.
    #[doc(hidden)]
    pub fn log_queue_for_tests(&self) -> (u64, usize) {
        (self.shared.logger.dropped.load(Ordering::Relaxed), self.shared.logger.queued())
    }

    /// The Data Lake Gen2 (`dfs` endpoint) service over the same data: a filesystem is a
    /// container, a path is a blob. Register both with the emulator.
    pub fn datalake(&self) -> DataLakeService {
        DataLakeService { shared: self.shared.clone() }
    }
}

#[async_trait]
impl Service for BlobService {
    fn name(&self) -> &'static str {
        "blob"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("blob")]
    }

    fn path_style_on_pinned(&self) -> bool {
        true
    }

    fn default_account(&self) -> &'static str {
        "devstoreaccount1"
    }

    async fn handle(&self, req: Request) -> Response {
        let op = router::op_of(&req);
        // customer-provided key headers must agree with each other (`cpk.rs`)
        if let Err(e) = cpk::validate(&req) {
            return azemu_core::storage::finish(e.into_response(&req.ctx), &req, router::SERVER);
        }
        // `x-ms-structured-body`: an upload is taken apart before anything else sees the body,
        // a download is wrapped on the way out (see `structured.rs`).
        let mut wrapped = false;
        let req = match op {
            ops::Op::PutBlob | ops::Op::PutBlock | ops::Op::PutPage | ops::Op::AppendBlock => match structured::unwrap_upload(&req) {
                Ok(Some(plain)) => {
                    wrapped = true;
                    plain
                }
                Ok(None) => req,
                Err(e) => return azemu_core::storage::finish(e.into_response(&req.ctx), &req, router::SERVER),
            },
            _ => req,
        };
        let wrap_get = if op == ops::Op::GetBlob {
            match structured::download_wanted(&req) {
                Ok(w) => w,
                Err(e) => return azemu_core::storage::finish(e.into_response(&req.ctx), &req, router::SERVER),
            }
        } else {
            false
        };
        let started = Instant::now();
        let pre = pre::prepare(&req, op).await;
        let mut resp = self.shared.serve(&req, pre, |st, pre| router::run(st, &req, Some(&self.shared.accounts), pre)).await;
        if wrapped {
            structured::echo(&mut resp);
        }
        if wrap_get {
            structured::wrap_download(&mut resp);
        }
        if op == ops::Op::SetServiceProperties {
            // the switches of Storage Analytics logging may have changed
            self.shared.refresh_log_cfg();
        }
        // Storage Analytics logging: a line in a bounded queue, nothing under the state lock (see `analytics.rs`)
        let (container, blob, _) = router::target_of(&req);
        self.shared.log_request(&analytics::Done {
            req: &req,
            resp: &resp,
            op,
            millis: started.elapsed().as_millis() as u64,
            container: container.as_deref(),
            blob: blob.as_deref(),
        });
        resp
    }

    fn reset(&self) {
        self.shared.reset();
    }

    /// Empty one storage account (Blob and Data Lake share it): containers, blobs, versions, the service properties and the
    /// account's versioning switch, which goes back to the default. Persistence sees a change.
    fn reset_account(&self, account: &str) -> bool {
        self.shared.state.lock().accounts.remove(&account.to_ascii_lowercase());
        self.shared.logger.set_cfg(&account.to_ascii_lowercase(), Default::default());
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Work that costs a lot and changes nothing: a query over a blob (a `POST` on a blob path; the batch requests of
    /// the account and of a container have one segment at most) and reads (`GET`). Everything else runs to its end.
    fn abort_on_disconnect(&self, method: &azemu_core::http::Method, path: &str) -> bool {
        abort_on_disconnect(method, path)
    }

    /// The Blob service owns the files of the state it shares with the Data Lake service (`dfs` keeps nothing of
    /// its own).
    fn persist_generation(&self) -> Option<u64> {
        Some(self.shared.generation.load(Ordering::SeqCst))
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        // log lines that wait for their delay are written first, so that a stop does not lose them
        self.shared.flush_logs();
        self.shared.save_to(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.shared.load_from(dir)?;
        self.shared.refresh_log_cfg();
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod lock_tests {
    //! The state lock must not be held for work that grows with the size of a body.
    //!
    //! The tests measure how long the lock was held (`HoldStats`, taken inside the lock), not
    //! how long a request took, so a busy machine does not make them flaky: each one compares the
    //! longest lock hold with the time the same machine needs to hash (or join) the same bytes.

    use std::time::Instant;

    use azemu_core::bytes::Bytes;
    use azemu_core::http::{HeaderMap, HeaderValue, Method};
    use azemu_core::{Endpoint, Query, Request, RequestCtx};

    use super::*;

    fn rq(method: &str, path: &str, headers: &[(&str, &str)], body: impl Into<Bytes>) -> Request {
        let (p, q) = path.split_once('?').unwrap_or((path, ""));
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("SharedKey acct:c2ln"));
        for (k, v) in headers {
            h.insert(azemu_core::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        Request {
            method: Method::from_bytes(method.as_bytes()).unwrap(),
            path: p.to_string(),
            query: Query::parse(q),
            headers: h,
            body: body.into(),
            account: "acct".into(),
            ctx: RequestCtx {
                request_id: "test".into(),
                time: azemu_core::time::OffsetDateTime::now_utc(),
                endpoint: Endpoint { scheme: "http", authority: "acct.blob.localhost:1".into(), path_style: false, account: "acct".into() },
            },
        }
    }

    const BLOCK: &[(&str, &str)] = &[("x-ms-blob-type", "BlockBlob")];

    /// Longest lock hold since the last call.
    fn take_max(svc: &BlobService) -> Duration {
        Duration::from_nanos(svc.shared.hold.max_ns.swap(0, Ordering::Relaxed))
    }

    /// What it costs on this machine to hash `data` once.
    fn hash_time(data: &[u8]) -> Duration {
        let t = Instant::now();
        let _ = (crate::util::md5(data), crate::util::crc64(data));
        t.elapsed()
    }

    async fn ok(svc: &BlobService, r: Request, want: u16) -> azemu_core::Response {
        let mut resp = svc.handle(r).await;
        assert_eq!(resp.status().as_u16(), want, "{}", String::from_utf8_lossy(resp.body()));
        // a streamed answer (a big download, a query) is read to the end here, as the connection would
        if azemu_core::stream::is_streamed(&resp) {
            let all = azemu_core::stream::collect(&mut resp).await.expect("streamed").expect("no failure");
            *resp.body_mut() = all;
        }
        resp
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn big_bodies_are_not_hashed_or_joined_under_the_lock() {
        let svc = BlobService::new();
        ok(&svc, rq("PUT", "/cnt?restype=container", &[], ""), 201).await;
        let size = 16 * 1024 * 1024;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let hash = hash_time(&data);
        let limit = hash / 10;
        take_max(&svc);

        // Put Blob: MD5 and CRC-64 of the body are worked out before the lock.
        let body = Bytes::from(data.clone());
        let resp = ok(&svc, rq("PUT", "/cnt/big", BLOCK, body), 201).await;
        let held = take_max(&svc);
        assert!(held < limit, "Put Blob held the lock {held:?}; hashing the body takes {hash:?}");
        assert_eq!(resp.headers()["content-md5"].to_str().unwrap(), crate::util::b64(&crate::util::md5(&data)));

        // Put Block x 4, Put Block List: the blob now has four blocks.
        let quarter = size / 4;
        let mut list = String::from("<BlockList>");
        for i in 0..4usize {
            let id = crate::util::b64(format!("block-{i}").as_bytes());
            let chunk = Bytes::copy_from_slice(&data[i * quarter..(i + 1) * quarter]);
            ok(&svc, rq("PUT", &format!("/cnt/blocks?comp=block&blockid={}", id.replace('=', "%3D")), &[], chunk), 201).await;
            list.push_str(&format!("<Latest>{id}</Latest>"));
        }
        list.push_str("</BlockList>");
        assert!(take_max(&svc) < limit, "Put Block held the lock too long");
        ok(&svc, rq("PUT", "/cnt/blocks?comp=blocklist", &[], list), 201).await;
        assert!(take_max(&svc) < limit, "Put Block List held the lock too long");

        // Get Blob of a four-block blob: the pieces are joined after the lock is released.
        let resp = ok(&svc, rq("GET", "/cnt/blocks", &[], ""), 200).await;
        let held = take_max(&svc);
        assert!(held < limit, "Get Blob held the lock {held:?}");
        assert_eq!(resp.body().as_ref(), &data[..], "the joined body is the uploaded data");

        // A range over two blocks, with its MD5 and CRC-64 asked for (at most 4 MiB).
        let (from, to) = (quarter - 1000, quarter + 1000);
        let range = format!("bytes={from}-{to}");
        let resp = ok(
            &svc,
            rq(
                "GET",
                "/cnt/blocks",
                &[("x-ms-range", &range), ("x-ms-range-get-content-md5", "true"), ("x-ms-range-get-content-crc64", "true")],
                "",
            ),
            206,
        )
        .await;
        let want = &data[from..=to];
        assert_eq!(resp.body().as_ref(), want);
        assert_eq!(resp.headers()["content-md5"].to_str().unwrap(), crate::util::b64(&crate::util::md5(want)));
        assert_eq!(resp.headers()["x-ms-content-crc64"].to_str().unwrap(), crate::util::crc64_b64(crate::util::crc64(want)));

        // Put Blob From URL of the four-block blob: joined and hashed outside the lock, and the
        // copy is the same data.
        let url = "http://acct.blob.localhost:1/cnt/blocks";
        take_max(&svc);
        let resp = ok(&svc, rq("PUT", "/cnt/copy", &[("x-ms-blob-type", "BlockBlob"), ("x-ms-copy-source", url)], ""), 201).await;
        let held = take_max(&svc);
        assert!(held < limit, "Put Blob From URL held the lock {held:?}; hashing takes {hash:?}");
        assert_eq!(resp.headers()["content-md5"].to_str().unwrap(), crate::util::b64(&crate::util::md5(&data)));
        let resp = ok(&svc, rq("GET", "/cnt/copy", &[], ""), 200).await;
        assert_eq!(resp.body().as_ref(), &data[..]);

        // A source that does not exist still answers as before.
        let resp = svc
            .handle(rq(
                "PUT",
                "/cnt/x",
                &[("x-ms-blob-type", "BlockBlob"), ("x-ms-copy-source", "http://acct.blob.localhost:1/cnt/nope")],
                "",
            ))
            .await;
        assert_eq!(resp.status().as_u16(), 404);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn page_blob_zeros_are_made_after_the_lock() {
        let svc = BlobService::new();
        ok(&svc, rq("PUT", "/cnt?restype=container", &[], ""), 201).await;
        let size = 32 * 1024 * 1024u64;
        ok(&svc, rq("PUT", "/cnt/p", &[("x-ms-blob-type", "PageBlob"), ("x-ms-blob-content-length", &size.to_string())], ""), 201).await;
        let page = vec![9u8; 512];
        ok(&svc, rq("PUT", "/cnt/p?comp=page", &[("x-ms-page-write", "update"), ("x-ms-range", "bytes=1024-1535")], page.clone()), 201)
            .await;
        take_max(&svc);
        let t = Instant::now();
        let zeros = crate::state::join(vec![crate::state::Piece::Zeros(size as usize)]);
        let make = t.elapsed();
        drop(zeros);
        let resp = ok(&svc, rq("GET", "/cnt/p", &[], ""), 200).await;
        let held = take_max(&svc);
        assert!(
            held < make / 5 + Duration::from_micros(500),
            "Get Blob of a page blob held the lock {held:?}; making the zeros takes {make:?}"
        );
        let body = resp.body();
        assert_eq!(body.len() as u64, size);
        assert_eq!(&body[1024..1536], &page[..]);
        assert!(body[..1024].iter().all(|b| *b == 0) && body[1536..].iter().all(|b| *b == 0));
    }

    /// While one big upload is being hashed another request is served at once: the small request is
    /// done long before the big one is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_small_request_does_not_wait_for_a_big_upload() {
        let svc = Arc::new(BlobService::new());
        ok(&svc, rq("PUT", "/cnt?restype=container", &[], ""), 201).await;
        ok(&svc, rq("PUT", "/cnt/small", BLOCK, "hello"), 201).await;
        let data = Bytes::from(vec![5u8; 24 * 1024 * 1024]);
        let hash = hash_time(&data);
        let upload = {
            let svc = svc.clone();
            tokio::spawn(async move {
                let t = Instant::now();
                let resp = svc.handle(rq("PUT", "/cnt/big", BLOCK, data)).await;
                (resp.status().as_u16(), t.elapsed())
            })
        };
        // Let the upload start hashing.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let mut slowest = Duration::ZERO;
        for _ in 0..20 {
            let t = Instant::now();
            let resp = svc.handle(rq("GET", "/cnt/small", &[], "")).await;
            slowest = slowest.max(t.elapsed());
            assert_eq!(resp.body().as_ref(), b"hello");
        }
        let (status, took) = upload.await.unwrap();
        assert_eq!(status, 201);
        assert!(took >= hash / 2, "the upload took {took:?}, hashing alone {hash:?}");
        assert!(slowest < took / 2, "a small GET took {slowest:?} while the big upload took {took:?}");
    }

    /// A query reads and evaluates the whole blob, but none of that happens under the lock: it only
    /// takes the blob's pieces out (handles, no copy).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_query_does_not_scan_the_blob_under_the_lock() {
        let svc = BlobService::new();
        ok(&svc, rq("PUT", "/cnt?restype=container", &[], ""), 201).await;
        let rows: String = (0..600_000).map(|i| format!("{i},name-{i},{}\n", i % 97)).collect();
        ok(&svc, rq("PUT", "/cnt/big", BLOCK, rows.clone()), 201).await;
        take_max(&svc);
        let body = "<QueryRequest><QueryType>SQL</QueryType><Expression>SELECT UPPER(_2), CAST(_3 AS INT) + 1 FROM BlobStorage WHERE _1 LIKE '%7'</Expression></QueryRequest>";
        let t = Instant::now();
        // the answer is streamed; `ok` reads it to the end, so the scan runs inside this call
        let resp = ok(&svc, rq("POST", "/cnt/big?comp=query", &[], body), 200).await;
        let took = t.elapsed();
        let held = take_max(&svc);
        assert!(resp.body().len() > 1000);
        assert!(held < took / 20, "the lock was held {held:?} by a query that took {took:?}");
    }

    /// A save copies the metadata under the lock and does the encoding, the hashing of new data and the writing
    /// without it: the lock is held for a small part of the time the save takes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn save_does_not_encode_or_write_under_the_lock() {
        let dir = std::env::temp_dir().join(format!("azemu-blob-lock-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let svc = BlobService::new();
        ok(&svc, rq("PUT", "/cnt?restype=container", &[], ""), 201).await;
        for i in 0..3000 {
            ok(
                &svc,
                rq(
                    "PUT",
                    &format!("/cnt/small-{i}"),
                    &[("x-ms-blob-type", "BlockBlob"), ("x-ms-meta-n", "metadata value"), ("x-ms-tags", "a=b")],
                    "x".repeat(100),
                ),
                201,
            )
            .await;
        }
        for i in 0..6 {
            let big = Bytes::from(vec![i as u8; 4 * 1024 * 1024]);
            ok(&svc, rq("PUT", &format!("/cnt/big-{i}"), BLOCK, big), 201).await;
        }
        take_max(&svc);
        let t = Instant::now();
        svc.save(&dir).unwrap();
        let took = t.elapsed();
        let held = take_max(&svc);
        assert!(held > Duration::ZERO, "the save did take the lock to copy the metadata");
        assert!(held < took / 4, "the save held the lock for {held:?} of {took:?}");
        // a save of nothing new does the copy and the encoding, but writes no data
        let t = Instant::now();
        svc.save(&dir).unwrap();
        let again = t.elapsed();
        let held_again = take_max(&svc);
        assert!(held_again < again / 2, "the second save held the lock for {held_again:?} of {again:?}");
        // the saved directory loads into a service that answers the same
        let other = BlobService::new();
        other.load(&dir).unwrap();
        let r = ok(&other, rq("GET", "/cnt/big-3", &[], ""), 200).await;
        assert_eq!(r.body().len(), 4 * 1024 * 1024);
        assert!(r.body().iter().all(|b| *b == 3));
        ok(&other, rq("GET", "/cnt/small-2999", &[], ""), 200).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// More than 100,000 staged blocks on one blob are refused, as Azure does.
    #[tokio::test]
    async fn staged_blocks_are_limited_per_blob() {
        let svc = BlobService::new();
        ok(&svc, rq("PUT", "/cnt?restype=container", &[], ""), 201).await;
        for i in 0..100_000u32 {
            let id = crate::util::b64(format!("{i:08}").as_bytes());
            let r = svc.handle(rq("PUT", &format!("/cnt/b?comp=block&blockid={}", id.replace('=', "%3D")), &[], "x")).await;
            assert_eq!(r.status().as_u16(), 201);
        }
        let id = crate::util::b64(b"00100000");
        let r = svc.handle(rq("PUT", &format!("/cnt/b?comp=block&blockid={}", id.replace('=', "%3D")), &[], "x")).await;
        assert_eq!(r.status().as_u16(), 409);
        // Re-staging one that exists is fine.
        let id = crate::util::b64(b"00000007");
        let r = svc.handle(rq("PUT", &format!("/cnt/b?comp=block&blockid={}", id.replace('=', "%3D")), &[], "y")).await;
        assert_eq!(r.status().as_u16(), 201);
    }
}
