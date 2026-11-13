//! Answers that are produced while the client reads them.
//!
//! [`Response`](crate::Response) holds its whole body as `Bytes`, which is right for almost every service
//! answer. A service whose answer can be very large, or slow to produce (a query over a big blob), instead
//! gives core a [`BodyStream`]: core asks it for the next chunk only when the connection can take it, so the
//! memory in use stays near one chunk however big the answer is (back-pressure), and the service learns at once
//! when the client has gone (the stream is dropped, and the request's [`CancelToken`](crate::CancelToken) fires).
//!
//! The contract is additive: a service that returns a plain `Response` is not affected. A streamed answer is a
//! `Response` with an empty body and the stream attached as an extension ([`attach`], or `Resp::stream`), so code
//! that passes a `Response` along without looking at its body (CORS headers, logging) keeps working. Only core
//! looks inside. There is no `Content-Length` unless the service sets one: HTTP/1.1 then uses chunked framing,
//! HTTP/2 simply sends data frames. A `HEAD` request drops the stream without reading it.
//!
//! Time limits: a client that does not take data for the body idle time-out (`EmulatorBuilder::body_timeout`,
//! 60 s) loses its connection, and with it the stream. An error from the stream (`Err`) ends the answer
//! abruptly (the client sees a broken body), which is the right result for a failure after the status line was sent.

use std::io;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;

use crate::Response;

/// A source of body chunks, polled by core.
pub trait BodyStream: Send + 'static {
    /// The next chunk: `Ready(Some(Ok(bytes)))` a chunk, `Ready(None)` the end, `Ready(Some(Err(_)))` a failure that
    /// aborts the answer. Like `Stream::poll_next`, `Pending` must have arranged for `cx`'s waker to be woken.
    /// Empty chunks are skipped.
    fn poll_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Result<Bytes>>>;
}

/// A bounded channel is the usual way to feed a stream from a blocking thread (`blocking_send` waits when the
/// client is slow: that is the back-pressure).
impl BodyStream for tokio::sync::mpsc::Receiver<io::Result<Bytes>> {
    fn poll_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Result<Bytes>>> {
        self.poll_recv(cx)
    }
}

/// The stream travels in the response's extensions, which need `Clone`; core takes it out once.
#[derive(Clone)]
pub(crate) struct Slot(Arc<parking_lot::Mutex<Option<Box<dyn BodyStream>>>>);

/// Make `resp` a streamed answer: its body is replaced by `stream`.
pub fn attach(resp: &mut Response, stream: impl BodyStream) {
    *resp.body_mut() = Bytes::new();
    resp.extensions_mut().insert(Slot(Arc::new(parking_lot::Mutex::new(Some(Box::new(stream))))));
}

/// Core: take the stream out of a response, if it has one.
pub(crate) fn take(resp: &mut Response) -> Option<Box<dyn BodyStream>> {
    resp.extensions_mut().remove::<Slot>().and_then(|s| s.0.lock().take())
}

/// True when the response is streamed (not yet taken by core).
pub fn is_streamed(resp: &Response) -> bool {
    resp.extensions().get::<Slot>().is_some_and(|s| s.0.lock().is_some())
}

/// Support for tests: read a streamed answer to the end without a connection.
#[cfg(any(test, feature = "testing"))]
pub async fn collect(resp: &mut Response) -> Option<io::Result<Bytes>> {
    let mut s = take(resp)?;
    let mut out = bytes::BytesMut::new();
    loop {
        let next = std::future::poll_fn(|cx| s.poll_chunk(cx)).await;
        match next {
            None => return Some(Ok(out.freeze())),
            Some(Ok(b)) => out.extend_from_slice(&b),
            Some(Err(e)) => return Some(Err(e)),
        }
    }
}
