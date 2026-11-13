//! eventgrid: Azure Event Grid. Classic publish (custom topics, domains, partner
//! channels), namespace topics with pull delivery, push delivery to webhooks, and an
//! admin API to declare topics and subscriptions. See `docs/services/eventgrid.md`.
//!
//! Hosts: `https://{name}.eventgrid.localhost:{port}` where `{name}` is a custom topic,
//! a domain or a namespace.

// Errors in this crate are ready-to-send HTTP responses (`Result<_, Response>`).
#![allow(clippy::result_large_err)]

mod admin;
mod admin_subs;
mod auth;
mod config;
mod destination;
mod errors;
mod events;
mod filter;
mod http_client;
mod model;
mod ns;
mod persist;
mod publish;
mod pull;
mod push;
mod router;
mod schemas;
mod sources;
mod state;
mod webhook_policy;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use azemu_core::{async_trait, Mount, Request, Response, Service};
use parking_lot::Mutex;
use tokio::sync::Notify;

pub use destination::{DestinationConfig, DestinationDelivery, DestinationHandler, DestinationKind, DestinationTarget, Resolved};
pub use events::{Event, Schema};
pub use filter::{AdvancedFilter, Filter};
pub use model::{DeliveryMode, InputSchema, RetryPolicy, SubscriptionConfig, TopicKind};
pub use publish::SystemEventOutcome;
pub use sources::{SourceEvent, SourceRef, SubscriptionError, SubscriptionOutcome};
pub use state::PushLimits;
pub use webhook_policy::WebhookPolicy;

/// Settings of the service. `Options::from_env()` reads the `AZEMU_EVENTGRID_*` variables;
/// tests build their own so they do not touch the process environment.
#[derive(Clone, Debug)]
pub struct Options {
    /// Create topics (and namespace subscriptions) on first use. Off as on Azure; `AZEMU_EVENTGRID_AUTOCREATE=1` or
    /// `AZEMU_LENIENT=1` turns it on.
    pub autocreate: bool,
    /// JSON file with topics and subscriptions, loaded at start and after every reset.
    pub config_path: Option<PathBuf>,
    /// First wait before a webhook retry (ms); it doubles each time up to `retry_max_ms`.
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
    /// How long a webhook may take to answer (ms).
    pub delivery_timeout_ms: u64,
    /// How many deliveries waiting for a retry are kept, per subscription and in all (see `state::PushLimits`).
    pub push_limits: state::PushLimits,
    /// Where webhooks may point (see `webhook_policy.rs`): loopback and private addresses, names with no dot and
    /// names the host check serves, plus what `AZEMU_ALLOW_WEBHOOK_HOST` / `--allow-webhook-host` added.
    pub webhooks: WebhookPolicy,
}

impl Default for Options {
    /// As on Azure: nothing is created by being named (`autocreate: false`).
    fn default() -> Self {
        Options {
            autocreate: false,
            config_path: None,
            retry_base_ms: 500,
            retry_max_ms: 10_000,
            delivery_timeout_ms: 10_000,
            push_limits: state::PushLimits::default(),
            webhooks: WebhookPolicy::default(),
        }
    }
}

impl Options {
    /// Read the `AZEMU_EVENTGRID_*` variables (and `AZEMU_ALLOW_WEBHOOK_HOST`) through `azemu_core::settings`.
    /// Call it when the service is built.
    pub fn from_env() -> Options {
        use azemu_core::settings;
        settings::describe("AZEMU_EVENTGRID_AUTOCREATE", "off (on with `AZEMU_LENIENT`)", "Publishing to a custom topic or namespace topic that was never created, or receiving from a namespace subscription that was never created, makes it, instead of the not-found answer Azure gives.");
        settings::describe(
            "AZEMU_EVENTGRID_CONFIG",
            "none",
            "Path of a JSON file with topics, domains, namespaces and subscriptions to create at start and after every reset.",
        );
        settings::describe(
            "AZEMU_EVENTGRID_RETRY_BASE_MS",
            "500",
            "Milliseconds before the first retry of a failed webhook delivery; later retries wait twice as long each time.",
        );
        settings::describe(
            "AZEMU_EVENTGRID_RETRY_MAX_MS",
            "10000",
            "Longest wait in milliseconds between two retries of a webhook delivery.",
        );
        settings::describe(
            "AZEMU_EVENTGRID_DELIVERY_TIMEOUT_MS",
            "10000",
            "Milliseconds a webhook may take to answer before the delivery counts as failed.",
        );
        settings::describe(
            "AZEMU_EVENTGRID_MAX_PENDING",
            "1000",
            "Most deliveries waiting for a retry per subscription; the oldest is dead-lettered when more come.",
        );
        settings::describe("AZEMU_EVENTGRID_MAX_PENDING_MB", "32", "Most megabytes of deliveries waiting for a retry per subscription.");
        settings::describe(
            "AZEMU_EVENTGRID_MAX_PENDING_TOTAL",
            "5000",
            "Most deliveries waiting for a retry in all subscriptions together.",
        );
        settings::describe(
            "AZEMU_EVENTGRID_MAX_PENDING_TOTAL_MB",
            "128",
            "Most megabytes of deliveries waiting for a retry in all subscriptions together.",
        );
        let mut o = Options::default();
        o.autocreate = settings::lenient_flag("AZEMU_EVENTGRID_AUTOCREATE");
        o.config_path = settings::text("AZEMU_EVENTGRID_CONFIG").map(PathBuf::from);
        o.retry_base_ms = settings::number("AZEMU_EVENTGRID_RETRY_BASE_MS", o.retry_base_ms);
        o.retry_max_ms = settings::number("AZEMU_EVENTGRID_RETRY_MAX_MS", o.retry_max_ms);
        o.delivery_timeout_ms = settings::number("AZEMU_EVENTGRID_DELIVERY_TIMEOUT_MS", o.delivery_timeout_ms);
        let l = &mut o.push_limits;
        l.per_subscription = settings::number("AZEMU_EVENTGRID_MAX_PENDING", l.per_subscription as u64) as usize;
        l.per_subscription_bytes = settings::number("AZEMU_EVENTGRID_MAX_PENDING_MB", l.per_subscription_bytes >> 20) << 20;
        l.total = settings::number("AZEMU_EVENTGRID_MAX_PENDING_TOTAL", l.total as u64) as usize;
        l.total_bytes = settings::number("AZEMU_EVENTGRID_MAX_PENDING_TOTAL_MB", l.total_bytes >> 20) << 20;
        let mut served = settings::list("AZEMU_ALLOW_HOST");
        served.extend(settings::list("AZEMU_TLS_NAME"));
        o.webhooks = WebhookPolicy::new(&settings::list("AZEMU_ALLOW_WEBHOOK_HOST"), &served);
        o
    }
}

/// What request handlers and background delivery tasks share.
pub(crate) struct Shared {
    pub state: Mutex<state::State>,
    /// Woken whenever a pull queue may have something new (publish, release, reset).
    pub notify: Notify,
    pub opts: Options,
    /// Bumped by `reset()`; delivery tasks of an older generation stop.
    pub generation: AtomicU64,
    /// Handlers for Storage queue, Service Bus and Event Hubs destinations, registered by the
    /// unified binary (see `destination.rs`). Not cleared by `reset()`.
    pub destinations: parking_lot::RwLock<std::collections::HashMap<DestinationKind, DestinationHandler>>,
}

impl Shared {
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

pub struct EventGridService {
    shared: Arc<Shared>,
    /// What the last save left on disk (see `persist.rs`).
    saved: persist::SaveMemory,
}

impl Default for EventGridService {
    fn default() -> Self {
        Self::new()
    }
}

impl EventGridService {
    /// Settings from the environment.
    pub fn new() -> Self {
        Self::with_options(Options::from_env())
    }

    pub fn with_options(opts: Options) -> Self {
        let mut st = state::State::new(opts.push_limits);
        config::load_into(&opts, &mut st);
        EventGridService {
            saved: Default::default(),
            shared: Arc::new(Shared {
                state: Mutex::new(st),
                notify: Notify::new(),
                opts,
                generation: AtomicU64::new(0),
                destinations: Default::default(),
            }),
        }
    }

    /// Say how events for destinations of `kind` (Storage queue, Service Bus queue or topic,
    /// event hub) are delivered. The handler gets a finished message; `Err` makes the delivery
    /// fail and be retried like a webhook that answered 500. Without a handler a subscription to
    /// such a destination is refused with 501 (and a config-file subscription fails at delivery).
    /// A second call for the same kind replaces the handler. `reset()` keeps handlers.
    pub fn register_destination(&self, kind: DestinationKind, handler: DestinationHandler) {
        self.shared.destinations.write().insert(kind, handler);
    }

    /// Raise an event as if it had been published to the topic `topic` (a custom topic or a
    /// domain; created when it does not exist and auto-creation is on). `event` is an
    /// Event Grid schema event (`id`, `subject`, `eventType`, `eventTime`, `dataVersion`,
    /// `data`; `id`, `eventTime` and `dataVersion` are filled in when missing). Matching
    /// subscriptions get it like any other event.
    ///
    /// This is the entry point for other emulated services, for example Blob Storage raising
    /// `Microsoft.Storage.BlobCreated`. It must be called inside the tokio runtime
    /// (webhook deliveries are spawned as tasks) and never blocks.
    pub fn publish_system_event(&self, topic: &str, event: serde_json::Value) -> SystemEventOutcome {
        publish::system_event(&self.shared, topic, event)
    }
}

#[async_trait]
impl Service for EventGridService {
    fn name(&self) -> &'static str {
        "eventgrid"
    }

    fn mounts(&self) -> Vec<Mount> {
        // https://{topic}.eventgrid.localhost:4577 and the admin API under /_azemu/eventgrid
        vec![Mount::AccountHost("eventgrid"), Mount::PathPrefix("/_azemu/eventgrid")]
    }

    /// The largest body any Event Grid request may have here: the classic publish API (1.5 MB; the namespace API checks its own, smaller, limit).
    fn max_body_bytes(&self, _method: &azemu_core::http::Method, _path: &str) -> Option<usize> {
        Some(publish::MAX_CLASSIC_BODY)
    }

    fn body_too_large(&self, limit: usize) -> Option<Response> {
        Some(errors::payload_too_large(limit))
    }

    async fn handle(&self, req: Request) -> Response {
        let resp = router::route(&self.shared, &req).await;
        if !matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS") {
            // after the work, so that a save that began in between still finds the number moved at its next look
            state::next_revision();
        }
        auth::finish(resp, &req)
    }

    fn reset(&self) {
        let mut st = self.shared.state.lock();
        *st = state::State::new(self.shared.opts.push_limits);
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        config::load_into(&self.shared.opts, &mut st);
        drop(st);
        self.shared.notify.notify_waiters();
        self.saved.0.lock().forget();
        state::next_revision();
    }

    /// Empty one topic host (a custom topic, domain, namespace or event source, by the name in the host name) and put back
    /// what the config file declares for it, as `reset` does for all of them. Deliveries waiting for a retry stop at their
    /// next round, receivers waiting on a pull subscription are woken, and persistence sees a change.
    fn reset_account(&self, account: &str) -> bool {
        let k = state::key(account);
        let mut fresh = state::State::new(self.shared.opts.push_limits);
        config::load_into(&self.shared.opts, &mut fresh);
        let mut st = self.shared.state.lock();
        st.hosts.remove(&k);
        if let Some(h) = fresh.hosts.remove(&k) {
            st.hosts.insert(k, h);
        }
        drop(st);
        self.shared.notify.notify_waiters();
        self.saved.0.lock().forget();
        state::next_revision();
        true
    }

    /// The highest revision any subscription or definition has taken (`state::next_revision`): it moves with every
    /// change to what is saved (a request that is not a read, a delivery attempt), and with nothing else.
    fn persist_generation(&self) -> Option<u64> {
        Some(self.generation())
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.save_dir(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.load_dir(dir)
    }
}
