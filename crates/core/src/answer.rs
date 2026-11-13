//! The body of an answer as hyper reads it: bytes held in memory, or a [`BodyStream`](crate::stream::BodyStream).

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};
use tokio::task::JoinHandle;

use crate::cancel::CancelGuard;
use crate::stream::BodyStream;

/// A task that is aborted when this handle is dropped (a `JoinHandle` only detaches the task).
pub(crate) struct AbortOnDrop<T>(pub JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) enum AnswerBody {
    Full(Option<Bytes>),
    Stream(Box<Streamed>),
}

/// When did we last hand hyper a chunk that it has not come back from? hyper asks for the next chunk only when
/// the connection can take more, so a long gap means the client is not reading.
struct Watch {
    start: Instant,
    /// Milliseconds since `start` plus one when the last chunk was handed out; 0 while hyper is waiting for us.
    handed_out: AtomicU64,
}

impl Watch {
    fn stalled_for(&self) -> Option<Duration> {
        match self.handed_out.load(Ordering::Acquire) {
            0 => None,
            t => Some(self.start.elapsed().saturating_sub(Duration::from_millis(t - 1))),
        }
    }
}

pub(crate) struct Streamed {
    src: Box<dyn BodyStream>,
    finished: bool,
    /// Cancels the request's token if the body is dropped before its end.
    cancel: Option<CancelGuard>,
    watch: Arc<Watch>,
    /// Held for as long as the body lives (the connection's count of requests in progress).
    _hold: Box<dyn Send>,
    /// Dropping it stops the watchdog.
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl AnswerBody {
    /// A streamed answer. `kill` is notified when the client takes no data for `idle`; the owner of the connection
    /// closes it then.
    pub(crate) fn stream(
        src: Box<dyn BodyStream>,
        cancel: Option<CancelGuard>,
        hold: impl Send + 'static,
        idle: Duration,
        kill: Arc<tokio::sync::Notify>,
    ) -> AnswerBody {
        let watch = Arc::new(Watch { start: Instant::now(), handed_out: AtomicU64::new(0) });
        let (stop, stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(watchdog(watch.clone(), idle, kill, stopped));
        AnswerBody::Stream(Box::new(Streamed { src, finished: false, cancel, watch, _hold: Box::new(hold), _stop: stop }))
    }
}

async fn watchdog(watch: Arc<Watch>, idle: Duration, kill: Arc<tokio::sync::Notify>, mut stopped: tokio::sync::oneshot::Receiver<()>) {
    loop {
        let wait = match watch.stalled_for() {
            Some(d) if d >= idle => {
                tracing::debug!(seconds = idle.as_secs_f32(), "a client stopped reading an answer");
                kill.notify_one();
                return;
            }
            Some(d) => idle - d,
            None => idle,
        };
        tokio::select! {
            _ = &mut stopped => return,
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

impl Body for AnswerBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match self.get_mut() {
            AnswerBody::Full(b) => Poll::Ready(b.take().filter(|b| !b.is_empty()).map(|b| Ok(Frame::data(b)))),
            AnswerBody::Stream(s) => {
                if s.finished {
                    return Poll::Ready(None);
                }
                s.watch.handed_out.store(0, Ordering::Release);
                loop {
                    match s.src.poll_chunk(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(None) => {
                            s.finished = true;
                            if let Some(g) = s.cancel.as_mut() {
                                g.disarm();
                            }
                            return Poll::Ready(None);
                        }
                        Poll::Ready(Some(Ok(b))) if b.is_empty() => continue,
                        Poll::Ready(Some(Ok(b))) => {
                            let t = s.watch.start.elapsed().as_millis() as u64 + 1;
                            s.watch.handed_out.store(t, Ordering::Release);
                            return Poll::Ready(Some(Ok(Frame::data(b))));
                        }
                        Poll::Ready(Some(Err(e))) => {
                            s.finished = true;
                            return Poll::Ready(Some(Err(e)));
                        }
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            AnswerBody::Full(b) => b.as_ref().is_none_or(Bytes::is_empty),
            AnswerBody::Stream(s) => s.finished,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            AnswerBody::Full(b) => SizeHint::with_exact(b.as_ref().map_or(0, |b| b.len() as u64)),
            AnswerBody::Stream(_) => SizeHint::default(),
        }
    }
}
