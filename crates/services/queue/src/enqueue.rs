//! Putting a message into a queue from inside the emulator, for other services (Event Grid
//! delivery to a Storage queue). Same rules as Put Message, without HTTP.

use azemu_core::time::OffsetDateTime;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::state::DEFAULT_TTL_SECS;
use crate::QueueService;

/// Why a message could not be put.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnqueueError {
    /// The queue (or its account) does not exist; Azure answers 404 QueueNotFound.
    QueueNotFound { account: String, queue: String },
    /// The message is over 64 KiB once encoded (Azure answers 400 RequestBodyTooLarge).
    TooLarge { bytes: usize },
}

impl std::fmt::Display for EnqueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnqueueError::QueueNotFound { account, queue } => {
                write!(f, "QueueNotFound: the queue '{queue}' of the storage account '{account}' does not exist")
            }
            EnqueueError::TooLarge { bytes } => write!(f, "RequestBodyTooLarge: the message is {bytes} bytes (limit 65536)"),
        }
    }
}

impl std::error::Error for EnqueueError {}

impl QueueService {
    /// Put `body` into `queue` of `account`, base64 encoded (the form the Azure SDKs use for
    /// binary messages and that Event Grid uses). `ttl_secs`: `None` is the default of seven
    /// days, `Some(-1)` never expires. The queue must exist. The message is visible at once.
    pub fn enqueue_base64(&self, account: &str, queue: &str, body: &[u8], ttl_secs: Option<i64>) -> Result<(), EnqueueError> {
        let text = B64.encode(body);
        if text.len() > crate::state::MAX_MESSAGE_BYTES {
            return Err(EnqueueError::TooLarge { bytes: text.len() });
        }
        let account = account.to_ascii_lowercase();
        let now = OffsetDateTime::now_utc();
        let mut state = self.state.lock();
        let q = state.accounts.get_mut(&account).and_then(|a| a.queues.get_mut(queue));
        match q {
            Some(q) => {
                q.put(text, 0, ttl_secs.unwrap_or(DEFAULT_TTL_SECS), now);
                self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            None => Err(EnqueueError::QueueNotFound { account, queue: queue.to_string() }),
        }
    }
}
