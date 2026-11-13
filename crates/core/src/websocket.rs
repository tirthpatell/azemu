//! WebSocket (RFC 6455, server side): the upgrade contract and a byte-stream adapter.
//!
//! Core answers an HTTP/1.1 request with `Upgrade: websocket` on any listener (plain or TLS,
//! gateway or pinned) when a registered [`WebSocketProtocol`] wants it. It sends `101` with the
//! right `Sec-WebSocket-Accept` and the chosen subprotocol, then hands the connection over as a
//! plain byte stream ([`WsConn::io`], a [`WsStream`]). The payloads of the binary frames the client
//! sends are concatenated into one stream (frame boundaries mean nothing); bytes written to the
//! stream go out as unmasked binary frames.
//!
//! [`WsStream`] does the framing by hand: it unmasks client frames as they are read (no payload
//! is buffered), joins fragments, answers ping with pong, answers close with close, and splits big
//! writes into frames of at most 32 KiB. At most one output chunk and a few control frames are
//! held in memory.
//!
//! Without any registered protocol, core serves the upgrade with the raw protocols (AMQP): see
//! [`RawOverWebSocket`]. The services need no code for it.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use bytes::Bytes;
use http::{header, HeaderMap, Method, StatusCode, Version};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::server::{read_prefix, Prefixed};
use crate::service::{Io, RawConn, RawProtocol};

/// Default limit for the payload of one client frame: 16 MiB.
pub const DEFAULT_MAX_FRAME_BYTES: u64 = 16 * 1024 * 1024;

/// Payload bytes per server frame. A bigger write is split.
const WRITE_CHUNK: usize = 32 * 1024;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

// ---------------------------------------------------------------------------------------------
// The contract
// ---------------------------------------------------------------------------------------------

/// What core knows about an upgrade request.
#[derive(Debug, Clone)]
pub struct WsRequest {
    /// Raw request path, still percent-encoded. Starts with "/".
    pub path: String,
    /// Raw query without the leading '?'.
    pub query: String,
    /// The `Host` header as sent (with port), empty when absent.
    pub host: String,
    pub headers: HeaderMap,
    /// `Sec-WebSocket-Protocol` values the client offered, in the order sent.
    pub subprotocols: Vec<String>,
    /// The connection runs inside TLS (`wss://`).
    pub tls: bool,
    pub peer: SocketAddr,
    pub local: SocketAddr,
    /// The TLS server name when there is one, otherwise the host part of `Host` (no port).
    pub server_name: Option<String>,
}

/// A handler's answer to "do you want this upgrade request?".
#[derive(Debug, Clone, Default)]
pub struct WsAccept {
    /// The subprotocol to answer with. It must be one the client offered (sent back exactly as
    /// the client wrote it). `None` sends no `Sec-WebSocket-Protocol` header.
    pub subprotocol: Option<String>,
}

/// A connection after the `101`.
pub struct WsConn {
    /// The WebSocket seen as a byte stream (a [`WsStream`]).
    pub io: Box<dyn Io>,
    pub request: WsRequest,
    /// The subprotocol that was answered.
    pub subprotocol: Option<String>,
}

/// For protocols that run inside a WebSocket. Registered with
/// `EmulatorBuilder::websocket_protocol`; works on every listener, with and without TLS.
#[async_trait]
pub trait WebSocketProtocol: Send + Sync + 'static {
    /// Called for an HTTP request that asks for a WebSocket upgrade. Return `Some` to take it
    /// (core then checks the handshake headers and answers `101` or `400`/`426`); `None` lets the
    /// request go on as ordinary HTTP.
    fn accept(&self, req: &WsRequest) -> Option<WsAccept>;
    /// Serve the connection until it ends.
    async fn serve(&self, conn: WsConn);
}

// ---------------------------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------------------------

/// A request that passed the handshake checks.
pub(crate) struct Offer {
    pub key: String,
}

/// Why a request that asks for an upgrade is refused.
pub(crate) struct Refusal {
    pub status: StatusCode,
    pub message: &'static str,
}

fn token_list(headers: &HeaderMap, name: header::HeaderName) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// `None`: not a WebSocket upgrade request at all (carries on as HTTP).
/// `Some(Err)`: it asks for one but is malformed.
pub(crate) fn inspect(method: &Method, version: Version, headers: &HeaderMap) -> Option<Result<Offer, Refusal>> {
    let upgrade_ws = token_list(headers, header::UPGRADE).iter().any(|t| t.eq_ignore_ascii_case("websocket"));
    let conn_upgrade = token_list(headers, header::CONNECTION).iter().any(|t| t.eq_ignore_ascii_case("upgrade"));
    if !upgrade_ws || !conn_upgrade {
        return None;
    }
    let bad = |message| Some(Err(Refusal { status: StatusCode::BAD_REQUEST, message }));
    if method != Method::GET || version != Version::HTTP_11 {
        return bad("A WebSocket upgrade is a GET request over HTTP/1.1.");
    }
    let version_ok = headers.get("sec-websocket-version").and_then(|v| v.to_str().ok()).is_some_and(|v| v.trim() == "13");
    if !version_ok {
        return Some(Err(Refusal { status: StatusCode::UPGRADE_REQUIRED, message: "Only WebSocket version 13 is supported." }));
    }
    let key = headers
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|k| B64.decode(k).is_ok_and(|d| d.len() == 16));
    let Some(key) = key else {
        return bad("Sec-WebSocket-Key must be the base64 of 16 bytes.");
    };
    Some(Ok(Offer { key }))
}

pub(crate) fn offered_subprotocols(headers: &HeaderMap) -> Vec<String> {
    token_list(headers, header::HeaderName::from_static("sec-websocket-protocol"))
}

/// `Sec-WebSocket-Accept` for a client key.
pub fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(GUID.as_bytes());
    B64.encode(h.finalize())
}

// ---------------------------------------------------------------------------------------------
// WsStream
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Data,
    Ping,
    Pong,
    Close,
}

/// A WebSocket seen as a byte stream. `S` is the connection after the `101`.
pub struct WsStream<S> {
    inner: S,
    max_frame: u64,

    // reading
    hdr: [u8; 14],
    hdr_have: usize,
    /// Payload bytes of the current frame still to come.
    left: u64,
    mask: [u8; 4],
    mask_pos: usize,
    kind: Kind,
    /// Payload of the control frame being read (at most 125 bytes).
    ctl: Vec<u8>,
    /// A fragmented data message is open (its last fragment has not arrived).
    in_message: bool,
    read_done: bool,

    // writing: `out` holds whole frames; `out_pos` is how much of it went out
    out: Vec<u8>,
    out_pos: usize,
    pending_pong: Option<Vec<u8>>,
    pending_close: Option<Vec<u8>>,
    /// A close frame is queued or sent: no more data frames.
    close_queued: bool,
}

impl<S> WsStream<S> {
    /// `max_frame` limits the payload of one client frame (bigger: close with 1009).
    pub fn new(inner: S, max_frame: u64) -> WsStream<S> {
        WsStream {
            inner,
            max_frame,
            hdr: [0; 14],
            hdr_have: 0,
            left: 0,
            mask: [0; 4],
            mask_pos: 0,
            kind: Kind::Data,
            ctl: Vec::new(),
            in_message: false,
            read_done: false,
            out: Vec::new(),
            out_pos: 0,
            pending_pong: None,
            pending_close: None,
            close_queued: false,
        }
    }

    /// Queue a close frame with a status code (sent by the next write, flush or shutdown).
    /// The reason is cut to 123 bytes.
    pub fn close_with(&mut self, code: u16, reason: &str) {
        if self.close_queued {
            return;
        }
        let mut payload = code.to_be_bytes().to_vec();
        let mut end = reason.len().min(123);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        payload.extend_from_slice(&reason.as_bytes()[..end]);
        self.queue_close_frame(payload);
    }

    fn queue_close_frame(&mut self, payload: Vec<u8>) {
        if self.close_queued {
            return;
        }
        let mut f = Vec::with_capacity(payload.len() + 2);
        f.push(0x80 | OP_CLOSE);
        f.push(payload.len() as u8);
        f.extend_from_slice(&payload);
        self.pending_close = Some(f);
        self.close_queued = true;
    }

    fn header_need(&self) -> usize {
        if self.hdr_have < 2 {
            return 2;
        }
        let ext = match self.hdr[1] & 0x7f {
            126 => 2,
            127 => 8,
            _ => 0,
        };
        2 + ext + 4
    }

    /// The reader found a protocol error: queue a close frame, stop reading, report it.
    fn protocol_error(&mut self, code: u16, msg: &'static str) -> io::Error {
        self.close_with(code, msg);
        self.read_done = true;
        io::Error::new(io::ErrorKind::InvalidData, msg)
    }
}

fn unmask(data: &mut [u8], mask: [u8; 4], pos: usize) {
    let m = [mask[pos & 3], mask[(pos + 1) & 3], mask[(pos + 2) & 3], mask[(pos + 3) & 3]];
    let key = u32::from_ne_bytes(m);
    let (chunks, rest) = data.as_chunks_mut::<4>();
    for c in chunks {
        *c = (u32::from_ne_bytes(*c) ^ key).to_ne_bytes();
    }
    for (i, b) in rest.iter_mut().enumerate() {
        *b ^= m[i];
    }
}

fn push_frame_header(out: &mut Vec<u8>, opcode: u8, len: usize) {
    out.push(0x80 | opcode);
    if len < 126 {
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> WsStream<S> {
    /// Move the next queued control frame into `out` when `out` is empty.
    fn fill_out(&mut self) {
        if self.out_pos < self.out.len() {
            return;
        }
        self.out.clear();
        self.out_pos = 0;
        if let Some(c) = self.pending_close.take() {
            self.out = c;
        } else if let Some(p) = self.pending_pong.take() {
            push_frame_header(&mut self.out, OP_PONG, p.len());
            self.out.extend_from_slice(&p);
        }
    }

    /// Write everything queued (a partly sent frame first, then control frames).
    fn poll_flush_out(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            self.fill_out();
            if self.out_pos >= self.out.len() {
                return Poll::Ready(Ok(()));
            }
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.out[self.out_pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.out_pos += n;
        }
    }

    fn handle_control(&mut self) {
        match self.kind {
            Kind::Ping => {
                // answering only the latest ping is allowed (RFC 6455 5.5.3)
                if !self.close_queued {
                    self.pending_pong = Some(std::mem::take(&mut self.ctl));
                }
            }
            Kind::Pong | Kind::Data => {}
            Kind::Close => {
                // echo the status code, drop the reason
                let reply = if self.ctl.len() >= 2 { self.ctl[..2].to_vec() } else { Vec::new() };
                self.queue_close_frame(reply);
                self.read_done = true;
            }
        }
        self.ctl.clear();
    }

    /// Parse the complete header in `hdr`. `Ok` sets up the payload state.
    fn start_frame(&mut self) -> Result<(), io::Error> {
        let b0 = self.hdr[0];
        let b1 = self.hdr[1];
        let fin = b0 & 0x80 != 0;
        let op = b0 & 0x0f;
        let len7 = (b1 & 0x7f) as u64;
        if b0 & 0x70 != 0 {
            // no extension is negotiated, so a reserved bit is an error
            return Err(self.protocol_error(1002, "reserved bits are set"));
        }
        let (len, mask_at) = match len7 {
            126 => (u16::from_be_bytes([self.hdr[2], self.hdr[3]]) as u64, 4),
            127 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&self.hdr[2..10]);
                let v = u64::from_be_bytes(b);
                if v >> 63 != 0 {
                    return Err(self.protocol_error(1002, "invalid frame length"));
                }
                (v, 10)
            }
            n => (n, 2),
        };
        self.mask.copy_from_slice(&self.hdr[mask_at..mask_at + 4]);
        self.mask_pos = 0;
        self.hdr_have = 0;
        let kind = match op {
            OP_CONT | OP_TEXT | OP_BINARY => {
                if op == OP_CONT && !self.in_message {
                    return Err(self.protocol_error(1002, "continuation frame without a message"));
                }
                if op != OP_CONT && self.in_message {
                    return Err(self.protocol_error(1002, "new message inside a fragmented one"));
                }
                if len > self.max_frame {
                    return Err(self.protocol_error(1009, "frame too big"));
                }
                self.in_message = !fin;
                Kind::Data
            }
            OP_CLOSE | OP_PING | OP_PONG => {
                if !fin || len > 125 {
                    return Err(self.protocol_error(1002, "bad control frame"));
                }
                match op {
                    OP_CLOSE => Kind::Close,
                    OP_PING => Kind::Ping,
                    _ => Kind::Pong,
                }
            }
            _ => return Err(self.protocol_error(1002, "unknown opcode")),
        };
        if len == 1 && kind == Kind::Close {
            return Err(self.protocol_error(1002, "bad close frame"));
        }
        self.kind = kind;
        self.left = len;
        self.ctl.clear();
        if len == 0 && kind != Kind::Data {
            self.handle_control();
        }
        Ok(())
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsStream<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            // push out queued pongs, a close reply, or the tail of a data frame; the writer may
            // be idle, and a pong must not wait for the next write
            let _ = this.poll_flush_out(cx);
            if this.read_done {
                return Poll::Ready(Ok(()));
            }
            if this.left == 0 {
                // header
                let need = this.header_need();
                if this.hdr_have < need {
                    let mut rb = ReadBuf::new(&mut this.hdr[this.hdr_have..need]);
                    ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
                    let n = rb.filled().len();
                    if n == 0 {
                        if this.hdr_have == 0 && !this.in_message {
                            // the peer ended the connection without a close frame
                            this.read_done = true;
                            return Poll::Ready(Ok(()));
                        }
                        return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                    }
                    this.hdr_have += n;
                    if this.hdr_have >= 2 && this.hdr[1] & 0x80 == 0 {
                        let e = this.protocol_error(1002, "client frames must be masked");
                        let _ = this.poll_flush_out(cx);
                        return Poll::Ready(Err(e));
                    }
                    if this.hdr_have < this.header_need() {
                        continue;
                    }
                }
                if let Err(e) = this.start_frame() {
                    let _ = this.poll_flush_out(cx);
                    return Poll::Ready(Err(e));
                }
                continue;
            }
            // payload
            if this.kind == Kind::Data {
                let want = (buf.remaining() as u64).min(this.left) as usize;
                if want == 0 {
                    return Poll::Ready(Ok(()));
                }
                let dst = buf.initialize_unfilled_to(want);
                let mut rb = ReadBuf::new(dst);
                ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
                let n = rb.filled().len();
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                }
                unmask(&mut dst[..n], this.mask, this.mask_pos);
                this.mask_pos = (this.mask_pos + n) & 3;
                this.left -= n as u64;
                buf.advance(n);
                return Poll::Ready(Ok(()));
            }
            let mut tmp = [0u8; 125];
            let want = this.left as usize;
            let mut rb = ReadBuf::new(&mut tmp[..want]);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
            let n = rb.filled().len();
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
            }
            unmask(&mut tmp[..n], this.mask, this.mask_pos);
            this.mask_pos = (this.mask_pos + n) & 3;
            this.ctl.extend_from_slice(&tmp[..n]);
            this.left -= n as u64;
            if this.left == 0 {
                this.handle_control();
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsStream<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        if this.close_queued {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // at most one chunk is held: the previous one must be gone first
        ready!(this.poll_flush_out(cx))?;
        let n = data.len().min(WRITE_CHUNK);
        this.out.clear();
        this.out_pos = 0;
        push_frame_header(&mut this.out, OP_BINARY, n);
        this.out.extend_from_slice(&data[..n]);
        // start sending now; if the socket is full the rest goes out with the next call, flush or read
        if let Poll::Ready(Err(e)) = this.poll_flush_out(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        ready!(this.poll_flush_out(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if !this.close_queued {
            this.close_with(1000, "");
        }
        ready!(this.poll_flush_out(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------------------------
// Raw protocols over WebSocket
// ---------------------------------------------------------------------------------------------

/// Serves every registered [`RawProtocol`] inside a WebSocket. Core uses it when no registered
/// [`WebSocketProtocol`] wants an upgrade request: it takes requests that offer the subprotocol
/// `amqp` or `AMQPWSB10` (or none), whatever the path, and after the `101` hands the byte stream
/// to the raw protocol whose `sniff` matches the first bytes.
pub(crate) struct RawOverWebSocket {
    pub raw: Vec<Arc<dyn RawProtocol>>,
}

/// Subprotocol names clients use for AMQP over WebSocket: the standard one and Microsoft's.
fn is_amqp_subprotocol(p: &str) -> bool {
    p.eq_ignore_ascii_case("amqp") || p.eq_ignore_ascii_case("AMQPWSB10")
}

impl RawOverWebSocket {
    /// Same rule as [`WebSocketProtocol::accept`].
    pub(crate) fn accept(&self, req: &WsRequest) -> Option<WsAccept> {
        if self.raw.is_empty() {
            return None;
        }
        if req.subprotocols.is_empty() {
            return Some(WsAccept::default());
        }
        req.subprotocols.iter().find(|p| is_amqp_subprotocol(p)).map(|p| WsAccept { subprotocol: Some(p.clone()) })
    }

    pub(crate) async fn serve<S>(&self, ws: WsStream<S>, request: WsRequest)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // the sniffer needs the first bytes; read them through the stream and hand them back
        let (prefix, mut ws) = read_prefix(ws).await;
        let proto = self.raw.iter().find(|p| !prefix.is_empty() && p.sniff(&prefix)).cloned();
        match proto {
            Some(p) => {
                let io = Prefixed::new(Bytes::from(prefix), ws);
                p.serve(RawConn {
                    io: Box::new(io),
                    peer: request.peer,
                    local: request.local,
                    tls: request.tls,
                    server_name: request.server_name,
                })
                .await
            }
            None => {
                tracing::debug!(peer = %request.peer, "WebSocket data matched no raw protocol");
                // 1003: the data is not of a kind this endpoint accepts
                use tokio::io::AsyncWriteExt;
                ws.close_with(1003, "no protocol here understands this data");
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ws.shutdown()).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

    fn masked(op: u8, fin: bool, payload: &[u8], key: [u8; 4]) -> Vec<u8> {
        let mut f = vec![if fin { 0x80 } else { 0 } | op];
        let n = payload.len();
        if n < 126 {
            f.push(0x80 | n as u8);
        } else if n <= 65535 {
            f.push(0x80 | 126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            f.push(0x80 | 127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
        f.extend_from_slice(&key);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
        f
    }

    /// Read one server frame (unmasked) from the peer side.
    async fn server_frame(peer: &mut tokio::io::DuplexStream) -> (u8, bool, Vec<u8>) {
        let mut h = [0u8; 2];
        peer.read_exact(&mut h).await.unwrap();
        assert_eq!(h[1] & 0x80, 0, "server frames are not masked");
        let len = match h[1] & 0x7f {
            126 => {
                let mut b = [0u8; 2];
                peer.read_exact(&mut b).await.unwrap();
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                peer.read_exact(&mut b).await.unwrap();
                u64::from_be_bytes(b) as usize
            }
            n => n as usize,
        };
        let mut p = vec![0u8; len];
        peer.read_exact(&mut p).await.unwrap();
        (h[0] & 0x0f, h[0] & 0x80 != 0, p)
    }

    #[test]
    fn accept_key_matches_the_rfc_example() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[tokio::test]
    async fn data_frames_become_one_stream_and_masks_survive_split_reads() {
        let (mut client, server) = duplex(64);
        let mut ws = WsStream::new(server, 1024);
        let a = masked(OP_BINARY, false, b"hello ", [1, 2, 3, 4]);
        let b = masked(OP_CONT, false, b"wor", [9, 8, 7, 6]);
        let c = masked(OP_CONT, true, b"ld", [5, 5, 5, 5]);
        // an odd-length frame cut in the middle of the header, the mask and the payload
        let mut all = a;
        all.extend(b);
        all.extend(masked(OP_PING, true, b"p", [1, 1, 1, 1]));
        all.extend(c);
        tokio::spawn(async move {
            for chunk in all.chunks(3) {
                client.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
            let mut sink = Vec::new();
            let _ = client.read_to_end(&mut sink).await;
            // the pong for the ping
            assert_eq!(sink, vec![0x80 | OP_PONG, 1, b'p']);
        });
        let mut got = vec![0u8; 11];
        ws.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello world");
    }

    #[tokio::test]
    async fn text_frames_and_empty_frames_are_accepted() {
        let (mut client, server) = duplex(1024);
        let mut ws = WsStream::new(server, 1024);
        let mut all = masked(OP_TEXT, true, b"", [1, 2, 3, 4]);
        all.extend(masked(OP_BINARY, true, b"abc", [4, 3, 2, 1]));
        client.write_all(&all).await.unwrap();
        let mut got = [0u8; 3];
        ws.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"abc");
    }

    #[tokio::test]
    async fn ping_is_answered_with_the_same_payload_even_when_nobody_writes() {
        let (mut client, server) = duplex(1024);
        let mut ws = WsStream::new(server, 1024);
        client.write_all(&masked(OP_PING, true, b"are you there", [7, 7, 7, 7])).await.unwrap();
        client.write_all(&masked(OP_BINARY, true, b"x", [1, 2, 3, 4])).await.unwrap();
        let mut one = [0u8; 1];
        ws.read_exact(&mut one).await.unwrap();
        assert_eq!(&one, b"x");
        let (op, fin, payload) = server_frame(&mut client).await;
        assert_eq!((op, fin, payload.as_slice()), (OP_PONG, true, &b"are you there"[..]));
    }

    #[tokio::test]
    async fn close_is_echoed_and_the_stream_ends() {
        let (mut client, server) = duplex(1024);
        let mut ws = WsStream::new(server, 1024);
        let mut payload = 1001u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"going away");
        client.write_all(&masked(OP_CLOSE, true, &payload, [3, 3, 3, 3])).await.unwrap();
        let mut rest = Vec::new();
        assert_eq!(ws.read_to_end(&mut rest).await.unwrap(), 0);
        let (op, _, p) = server_frame(&mut client).await;
        assert_eq!(op, OP_CLOSE);
        assert_eq!(p, 1001u16.to_be_bytes());
        // no data after a close
        assert!(ws.write_all(b"late").await.is_err());
    }

    #[tokio::test]
    async fn oversize_frame_closes_with_1009() {
        let (mut client, server) = duplex(1 << 16);
        let mut ws = WsStream::new(server, 100);
        client.write_all(&masked(OP_BINARY, true, &[0u8; 101], [1, 2, 3, 4])).await.unwrap();
        let mut b = [0u8; 8];
        assert!(ws.read(&mut b).await.is_err());
        let (op, _, p) = server_frame(&mut client).await;
        assert_eq!(op, OP_CLOSE);
        assert_eq!(&p[..2], 1009u16.to_be_bytes());
    }

    #[tokio::test]
    async fn a_frame_exactly_at_the_limit_is_fine() {
        let (mut client, server) = duplex(1 << 16);
        let mut ws = WsStream::new(server, 100);
        client.write_all(&masked(OP_BINARY, true, &[7u8; 100], [1, 2, 3, 4])).await.unwrap();
        let mut b = [0u8; 100];
        ws.read_exact(&mut b).await.unwrap();
        assert!(b.iter().all(|x| *x == 7));
    }

    #[tokio::test]
    async fn unmasked_client_frames_and_reserved_bits_are_protocol_errors() {
        for bad in [vec![0x82u8, 0x01, 0x41], vec![0xC2u8, 0x81, 1, 2, 3, 4, 0], vec![0x83u8, 0x80, 1, 2, 3, 4]] {
            let (mut client, server) = duplex(1024);
            let mut ws = WsStream::new(server, 100);
            client.write_all(&bad).await.unwrap();
            let mut b = [0u8; 8];
            assert!(ws.read(&mut b).await.is_err(), "{bad:?}");
            let (op, _, p) = server_frame(&mut client).await;
            assert_eq!(op, OP_CLOSE);
            assert_eq!(&p[..2], 1002u16.to_be_bytes());
        }
    }

    #[tokio::test]
    async fn fragmented_control_frame_and_stray_continuation_are_protocol_errors() {
        for bad in [
            masked(OP_PING, false, b"x", [1, 2, 3, 4]),
            masked(OP_PING, true, &[0u8; 126], [1, 2, 3, 4]),
            masked(OP_CONT, true, b"x", [1, 2, 3, 4]),
        ] {
            let (mut client, server) = duplex(1024);
            let mut ws = WsStream::new(server, 1000);
            client.write_all(&bad).await.unwrap();
            let mut b = [0u8; 8];
            assert!(ws.read(&mut b).await.is_err());
        }
        // a new message while a fragmented one is open
        let (mut client, server) = duplex(1024);
        let mut ws = WsStream::new(server, 1000);
        let mut all = masked(OP_BINARY, false, b"a", [1, 2, 3, 4]);
        all.extend(masked(OP_BINARY, true, b"b", [1, 2, 3, 4]));
        client.write_all(&all).await.unwrap();
        let mut b = [0u8; 1];
        ws.read_exact(&mut b).await.unwrap();
        assert!(ws.read(&mut b).await.is_err());
    }

    #[tokio::test]
    async fn eof_without_close_frame_is_a_clean_end_between_frames_only() {
        let (mut client, server) = duplex(1024);
        let mut ws = WsStream::new(server, 100);
        client.write_all(&masked(OP_BINARY, true, b"ab", [1, 2, 3, 4])).await.unwrap();
        drop(client);
        let mut all = Vec::new();
        ws.read_to_end(&mut all).await.unwrap();
        assert_eq!(all, b"ab");

        let (mut client, server) = duplex(1024);
        let mut ws = WsStream::new(server, 100);
        let f = masked(OP_BINARY, true, b"abcd", [1, 2, 3, 4]);
        client.write_all(&f[..f.len() - 2]).await.unwrap();
        drop(client);
        let mut all = Vec::new();
        assert!(ws.read_to_end(&mut all).await.is_err());
    }

    #[tokio::test]
    async fn writes_become_unmasked_binary_frames_of_bounded_size() {
        let (mut client, server) = duplex(1 << 20);
        let mut ws = WsStream::new(server, 100);
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        ws.write_all(&data).await.unwrap();
        ws.flush().await.unwrap();
        let mut got = Vec::new();
        while got.len() < data.len() {
            let (op, fin, p) = server_frame(&mut client).await;
            assert_eq!(op, OP_BINARY);
            assert!(fin);
            assert!(p.len() <= WRITE_CHUNK);
            got.extend(p);
        }
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn shutdown_sends_a_normal_close_and_ends_the_transport() {
        let (mut client, server) = duplex(1024);
        let mut ws = WsStream::new(server, 100);
        ws.shutdown().await.unwrap();
        let (op, _, p) = server_frame(&mut client).await;
        assert_eq!((op, p), (OP_CLOSE, 1000u16.to_be_bytes().to_vec()));
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn a_slow_reader_stalls_the_writer_instead_of_growing_memory() {
        // the transport holds 1 KiB; a 1 MiB write must not complete until the peer reads
        let (mut client, server) = duplex(1024);
        let mut ws = WsStream::new(server, 100);
        let data = vec![1u8; 1 << 20];
        let w = tokio::spawn(async move {
            ws.write_all(&data).await.unwrap();
            ws.flush().await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!w.is_finished());
        let mut total = 0usize;
        while total < (1 << 20) {
            let (_, _, p) = server_frame(&mut client).await;
            total += p.len();
        }
        w.await.unwrap();
    }
}
