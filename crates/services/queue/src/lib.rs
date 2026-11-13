//! queue: the Azure Queue Storage REST API.
//!
//! Reachable as `http://{account}.queue.localhost:{port}/{queue}` on the gateway and
//! as `http://127.0.0.1:{port}/{account}/{queue}` on a pinned port (Azurite style).
//! Any account name works without being created first.
//!
//! Layout: `routing` picks the operation, `auth` checks what the credentials allow,
//! `queues`, `messages` and `props` do the work on the `state`, `xml` reads and
//! writes the bodies and `errors` has every error Azure sends.

// Handlers return `Result<_, Response>` and the error is the finished HTTP response, which
// is large; boxing it would only add noise.
#![allow(clippy::result_large_err)]

mod auth;
mod enqueue;
mod errors;
mod messages;
mod persist;
mod props;
mod queues;
mod routing;
mod state;
mod xml;

use std::sync::atomic::{AtomicU64, Ordering};

use azemu_core::http::Method;
use azemu_core::storage;
use azemu_core::{async_trait, Mount, Request, Response, Service};
use parking_lot::Mutex;

use crate::auth::Lookup;
pub use crate::enqueue::EnqueueError;
use crate::routing::{route, Op, Routed, Unrouted};
use crate::state::State;

/// The `Server` header real Azure Queue Storage sends.
pub const SERVER: &str = "Windows-Azure-Queue/1.0 Microsoft-HTTPAPI/2.0";

#[derive(Default)]
pub struct QueueService {
    state: Mutex<State>,
    /// Account keys, for checking signatures (derived from the account name; `reset()`
    /// does not touch them).
    accounts: storage::StorageAccounts,
    /// Raised on every change to the state that a save would write (see `persist_generation`).
    generation: AtomicU64,
}

impl QueueService {
    pub fn new() -> Self {
        Self::default()
    }

    fn dispatch(&self, req: &Request) -> Response {
        let routed = match route(req) {
            Ok(r) => r,
            Err(u) => return unrouted(req, u),
        };
        // The lock is taken and released inside this function, never across an .await.
        let mut state = self.state.lock();
        if let Err(resp) = authorize(&state, &self.accounts, req, &routed) {
            return resp;
        }
        if let Some(name) = &routed.queue {
            if let Err(e) = queues::validate_name(name) {
                return e.response(&req.ctx);
            }
        }
        let queue = routed.queue.as_deref().unwrap_or("");
        let message_id = routed.message_id.as_deref().unwrap_or("");
        let st = &mut *state;
        let resp = match routed.op {
            Op::ListQueues => queues::list(st, req),
            Op::GetServiceProperties => props::get_properties(st, req),
            Op::SetServiceProperties => props::set_properties(st, req),
            Op::GetServiceStats => props::get_stats(req),
            Op::GetUserDelegationKey => props::user_delegation_key(req, &self.accounts),
            Op::CreateQueue => queues::create(st, req, queue),
            Op::DeleteQueue => queues::delete(st, req, queue),
            Op::GetQueueMetadata => queues::get_metadata(st, req, queue),
            Op::SetQueueMetadata => queues::set_metadata(st, req, queue),
            Op::GetQueueAcl => queues::get_acl(st, req, queue),
            Op::SetQueueAcl => queues::set_acl(st, req, queue),
            Op::PutMessage => messages::put(st, req, queue),
            Op::GetMessages => messages::get(st, req, queue),
            Op::PeekMessages => messages::peek(st, req, queue),
            Op::ClearMessages => messages::clear(st, req, queue),
            Op::UpdateMessage => messages::update(st, req, queue, message_id),
            Op::DeleteMessage => messages::delete(st, req, queue, message_id),
        };
        // Reads change nothing a save would write. Get Messages hides what it hands out, and expired messages
        // are dropped lazily (a restart that still has them drops them the same way), so only a Get Messages
        // that returned something counts.
        let changes = match routed.op {
            Op::ListQueues
            | Op::GetServiceProperties
            | Op::GetServiceStats
            | Op::GetUserDelegationKey
            | Op::GetQueueMetadata
            | Op::GetQueueAcl
            | Op::PeekMessages => false,
            Op::GetMessages => resp.body().windows(14).any(|w| w == b"<QueueMessage>"),
            _ => true,
        };
        if changes {
            // inside the lock and after the change: a save that sees the new number also sees the change
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        resp
    }
}

fn authorize(state: &State, accounts: &storage::StorageAccounts, req: &Request, routed: &Routed) -> Result<(), Response> {
    let lookup = match (auth::policy_id(req), routed.queue.as_deref()) {
        (Some(id), Some(queue)) => match state.accounts.get(&req.account).and_then(|a| a.queues.get(queue)) {
            None => Lookup::NoQueue,
            Some(q) => match q.acl.iter().find(|s| s.id == id) {
                Some(policy) => Lookup::Found(policy),
                None => Lookup::Missing,
            },
        },
        _ => Lookup::NotUsed,
    };
    auth::authorize(req, routed.op, lookup, accounts, routed.queue.as_deref())
}

fn unrouted(req: &Request, why: Unrouted) -> Response {
    match why {
        Unrouted::UnsupportedVerb => errors::unsupported_verb(&req.ctx),
        Unrouted::BadUri => errors::invalid_uri(&req.ctx),
        Unrouted::BadQuery(name, value) => errors::invalid_query(&req.ctx, name, &value, ""),
    }
}

#[async_trait]
impl Service for QueueService {
    fn name(&self) -> &'static str {
        "queue"
    }

    fn mounts(&self) -> Vec<Mount> {
        // http://{account}.queue.localhost:4577
        vec![Mount::AccountHost("queue")]
    }

    fn path_style_on_pinned(&self) -> bool {
        true
    }

    fn default_account(&self) -> &'static str {
        storage::DEV_ACCOUNT
    }

    async fn handle(&self, req: Request) -> Response {
        let mut resp = if req.method == Method::OPTIONS {
            // Preflight: no credentials, judged against the service's CORS rules.
            storage::cors::preflight(&req, &props::cors_rules(&self.state.lock(), &req.account))
        } else {
            self.dispatch(&req)
        };
        if req.method != Method::OPTIONS && req.header("origin").is_some() {
            storage::cors::apply(&mut resp, &req, &props::cors_rules(&self.state.lock(), &req.account));
        }
        storage::finish(resp, &req, SERVER)
    }

    fn reset(&self) {
        let mut state = self.state.lock();
        *state = State::default();
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Empty one storage account: its queues, messages and service properties. Persistence sees a change.
    fn reset_account(&self, account: &str) -> bool {
        self.state.lock().accounts.retain(|k, _| !k.eq_ignore_ascii_case(account));
        self.generation.fetch_add(1, Ordering::SeqCst);
        true
    }

    fn persist_generation(&self) -> Option<u64> {
        Some(self.generation.load(Ordering::SeqCst))
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.save_state(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.load_state(dir)
    }
}
