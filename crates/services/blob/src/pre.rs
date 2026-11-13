//! Work that grows with the size of a body is done outside the state lock: before it is taken
//! (hashing the request body, parsing a block list) or after it is released (joining the pieces
//! of a response body, hashing it, putting together the data of a source blob).
//!
//! The lock only ever sees the results. `Pre` carries what was worked out before the request
//! runs; handlers ask it for a hash and fall back to computing it themselves when it has none
//! (so a hash that was not prepared is slow, never wrong). A handler that needs the joined data
//! of a source blob (the "from URL" operations) asks for it with `State::wait_source` and the
//! request is run again once the data is ready; see `BlobService::handle`.

use azemu_core::bytes::Bytes;
use azemu_core::{Request, Response};

use crate::ops::Op;
use crate::state::{join, Blob, OutBody, Piece, SourceWait};
use crate::util::{b64, crc64, crc64_b64, md5};
use crate::xml::{self, Node};

/// Bodies up to this size are handled where they are; bigger ones go to a blocking thread so
/// that the async workers keep serving other requests.
pub const INLINE_MAX: usize = 64 * 1024;

/// A source blob's data, joined and hashed (see `Pre::prefetch`).
pub struct Prefetched {
    pub data: Bytes,
    pub blob: Blob,
    pub url: String,
}

struct Hashed {
    ptr: usize,
    len: usize,
    md5: Vec<u8>,
    crc: u64,
}

#[derive(Default)]
pub struct Pre {
    hashed: Vec<Hashed>,
    pub source: Option<Prefetched>,
    /// The parsed body of a Put Block List; `None` when it was not prepared or is not UTF-8.
    pub block_list: Option<Result<Node, String>>,
    /// The parsed body of a Query Blob Contents request.
    pub query: Option<crate::query::Prepared>,
}

impl Pre {
    fn add(&mut self, data: &Bytes, md5: Vec<u8>, crc: u64) {
        if !data.is_empty() {
            self.hashed.push(Hashed { ptr: data.as_ptr() as usize, len: data.len(), md5, crc });
        }
    }

    fn find(&self, data: &Bytes) -> Option<&Hashed> {
        // The same buffer (a clone of a `Bytes` points at the same memory), not equal content.
        self.hashed.iter().find(|h| h.ptr == data.as_ptr() as usize && h.len == data.len())
    }

    /// MD5 of `data`: the prepared one, or computed now.
    pub fn md5(&self, data: &Bytes) -> Vec<u8> {
        match self.find(data) {
            Some(h) => h.md5.clone(),
            None => md5(data),
        }
    }

    /// CRC-64 of `data`: the prepared one, or computed now.
    pub fn crc64(&self, data: &Bytes) -> u64 {
        match self.find(data) {
            Some(h) => h.crc,
            None => crc64(data),
        }
    }

    /// MD5 and CRC-64 of the request body (or any buffer given to `prefetch` / `hash_body`).
    pub async fn hash_body(&mut self, body: &Bytes) {
        if body.is_empty() {
            return;
        }
        let (m, c) = hash(body).await;
        self.add(body, m, c);
    }

    /// Join and hash the data of a source blob; later `read_source` calls find it here.
    pub async fn prefetch(&mut self, w: SourceWait) {
        let total: usize = w.pieces.iter().map(Piece::len).sum();
        let pieces = w.pieces;
        let data = blocking(total, Bytes::new(), move || join(pieces)).await;
        self.hash_body(&data).await;
        self.source = Some(Prefetched { data, blob: w.blob, url: w.url });
    }
}

/// Run `f` here when `size` is small, on a blocking thread otherwise. `fallback` is what comes
/// back when the thread is lost (the runtime is shutting down).
async fn blocking<T: Send + 'static>(size: usize, fallback: T, f: impl FnOnce() -> T + Send + 'static) -> T {
    if size <= INLINE_MAX {
        return f();
    }
    tokio::task::spawn_blocking(f).await.unwrap_or(fallback)
}

async fn hash(data: &Bytes) -> (Vec<u8>, u64) {
    let d = data.clone();
    blocking(data.len(), (vec![], 0), move || (md5(&d), crc64(&d))).await
}

/// What a request needs before it takes the lock.
pub async fn prepare(req: &Request, op: Op) -> Pre {
    let mut pre = Pre::default();
    match op {
        Op::PutBlob | Op::PutBlock | Op::PutPage | Op::AppendBlock => pre.hash_body(&req.body).await,
        Op::PutBlockList => {
            pre.hash_body(&req.body).await;
            let body = req.body.clone();
            pre.block_list = blocking(body.len(), None, move || std::str::from_utf8(&body).ok().map(xml::parse)).await;
        }
        Op::QueryBlob => {
            let body = req.body.clone();
            pre.query = blocking(body.len(), None, move || Some(crate::query::prepare(&body))).await;
        }
        _ => {}
    }
    pre
}

/// A body of pieces bigger than this, that needs no hash, is sent piece by piece instead of being joined into one buffer.
const STREAM_MIN: usize = 1024 * 1024;
/// The size of the parts a streamed download is cut into. Core drops a client that takes no part for the body idle time
/// (60 s), so a part is small enough for a slow link.
const STREAM_PART: usize = 512 * 1024;

/// The pieces of a download, handed to core one part at a time as the client takes them (see `azemu_core::stream`). The
/// pieces are shared handles to the stored data (zero runs are made part by part), so a download of any size uses
/// memory for one part, and the blob can change while it is sent: this is the data it had when the request was served.
struct Download {
    pieces: std::collections::VecDeque<Piece>,
}

impl azemu_core::stream::BodyStream for Download {
    fn poll_chunk(&mut self, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<std::io::Result<Bytes>>> {
        let part = match self.pieces.pop_front() {
            None => return std::task::Poll::Ready(None),
            Some(Piece::Data(d)) if d.len() <= STREAM_PART => d,
            Some(Piece::Data(mut d)) => {
                let rest = d.split_off(STREAM_PART);
                self.pieces.push_front(Piece::Data(rest));
                d
            }
            Some(Piece::Zeros(n)) => {
                let now = n.min(STREAM_PART);
                if n > now {
                    self.pieces.push_front(Piece::Zeros(n - now));
                }
                Bytes::from(vec![0u8; now])
            }
        };
        std::task::Poll::Ready(Some(Ok(part)))
    }
}

/// Put the body together, hash it if asked to, and set the headers that go with it.
pub async fn finish_out(resp: &mut Response, o: OutBody) {
    let total: usize = o.pieces.iter().map(Piece::len).sum();
    let (want_md5, want_crc) = (o.md5, o.crc64);
    if !want_md5 && !want_crc && total > STREAM_MIN {
        set(resp, "Content-Length", &total.to_string());
        azemu_core::stream::attach(resp, Download { pieces: o.pieces.into() });
        return;
    }
    let pieces = o.pieces;
    let (body, m, c) = blocking(total, (Bytes::new(), None, None), move || {
        let b = join(pieces);
        let m = want_md5.then(|| md5(&b));
        let c = want_crc.then(|| crc64(&b));
        (b, m, c)
    })
    .await;
    if let Some(m) = m {
        set(resp, "Content-MD5", &b64(&m));
    }
    if let Some(c) = c {
        set(resp, "x-ms-content-crc64", &crc64_b64(c));
    }
    *resp.body_mut() = body;
}

fn set(resp: &mut Response, name: &'static str, value: &str) {
    if let Ok(v) = azemu_core::http::HeaderValue::from_str(value) {
        resp.headers_mut().insert(name, v);
    }
}

/// Like `prepare`, for the Data Lake endpoint.
pub async fn prepare_dfs(req: &Request, op: crate::dfs::ops::Op) -> Pre {
    let mut pre = Pre::default();
    if matches!(op, crate::dfs::ops::Op::Append) {
        pre.hash_body(&req.body).await;
    }
    if matches!(op, crate::dfs::ops::Op::Query) {
        let body = req.body.clone();
        pre.query = blocking(body.len(), None, move || Some(crate::query::prepare(&body))).await;
    }
    pre
}
