//! Cancelling a request whose client has gone away.
//!
//! Core runs each service call in its own task. When the connection closes, or the HTTP/2 stream is reset,
//! before the answer is complete, core drops that task, so the handler's future stops at its next `.await`.
//! Work that is not at an `.await` (a loop on a blocking thread, started with `spawn_blocking`) cannot be
//! dropped; it checks a [`CancelToken`] instead. A service gets the token of its request from
//! [`RequestCtx::cancel_token`](crate::RequestCtx::cancel_token), in the handler, before it moves the work
//! to another thread.
//!
//! The token is cancelled when the request ends before its answer is complete: the client went away, or (for
//! an answer that is streamed, see [`crate::stream`]) the body was dropped before its last chunk. It is not
//! cancelled when the answer was sent normally.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

#[derive(Debug, Default)]
struct Inner {
    flag: AtomicBool,
    notify: Notify,
}

/// A cheap handle (an `Arc`) that says whether the request it belongs to has been cancelled. Clones share the state.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<Inner>);

impl CancelToken {
    /// A token that nobody has cancelled yet.
    pub fn new() -> CancelToken {
        CancelToken::default()
    }

    /// A token that is never cancelled (what a handler sees when it is called without core, in a unit test).
    pub fn never() -> CancelToken {
        CancelToken::default()
    }

    /// True once the request was cancelled. A relaxed atomic load: cheap enough for every loop turn.
    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::Relaxed)
    }

    /// Cancel. Safe to call more than once.
    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    /// Completes when the token is cancelled (at once when it already is).
    pub async fn cancelled(&self) {
        loop {
            let waiting = self.0.notify.notified();
            if self.is_cancelled() {
                return;
            }
            waiting.await;
        }
    }

    /// A guard that cancels this token when it is dropped, unless [`CancelGuard::disarm`] is called first.
    pub fn drop_guard(&self) -> CancelGuard {
        CancelGuard(Some(self.clone()))
    }
}

/// Cancels its token when dropped. Core keeps one for every request until the answer is complete.
#[derive(Debug)]
pub struct CancelGuard(Option<CancelToken>);

impl CancelGuard {
    /// The request ended normally: dropping the guard no longer cancels.
    pub fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(t) = self.0.take() {
            t.cancel();
        }
    }
}

tokio::task_local! {
    static CURRENT: CancelToken;
}

/// Run `fut` with `token` as the cancellation token that [`current`] returns inside it.
pub async fn scope<F: Future>(token: CancelToken, fut: F) -> F::Output {
    CURRENT.scope(token, fut).await
}

/// The token of the request being handled by the current task; [`CancelToken::never`] outside a request.
pub fn current() -> CancelToken {
    CURRENT.try_with(CancelToken::clone).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_token_is_cancelled_once_and_waiters_wake() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        let t2 = t.clone();
        let w = tokio::spawn(async move { t2.cancelled().await });
        tokio::task::yield_now().await;
        t.cancel();
        w.await.unwrap();
        assert!(t.is_cancelled());
        t.cancelled().await; // returns at once
    }

    #[tokio::test]
    async fn the_guard_cancels_on_drop_unless_disarmed() {
        let t = CancelToken::new();
        let mut g = t.drop_guard();
        g.disarm();
        drop(g);
        assert!(!t.is_cancelled());
        drop(t.drop_guard());
        assert!(t.is_cancelled());
    }

    #[tokio::test]
    async fn current_is_the_scoped_token_and_never_outside() {
        assert!(!current().is_cancelled());
        let t = CancelToken::new();
        scope(t.clone(), async {
            assert!(!current().is_cancelled());
            t.cancel();
            assert!(current().is_cancelled());
        })
        .await;
        assert!(!current().is_cancelled());
    }
}
