//! Configuration types: what a topic or subscription is declared as. They are the JSON
//! bodies of the admin API and the entries of the config file.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::destination::{DestinationConfig, Resolved};
use crate::events::Schema;
use crate::filter::Filter;

/// Default lock duration of a pull subscription.
pub const DEFAULT_LOCK_SECS: f64 = 60.0;
/// Default max delivery count of a pull subscription.
pub const DEFAULT_MAX_DELIVERY: u32 = 10;
/// Default event time to live of a pull subscription (one day).
pub const DEFAULT_TTL_SECS: f64 = 86_400.0;
/// Default retry policy of a push subscription (Event Grid: 30 attempts, 1440 minutes).
pub const DEFAULT_MAX_ATTEMPTS: u32 = 30;
pub const DEFAULT_TTL_MINUTES: f64 = 1440.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryMode {
    /// Consumers call receive / acknowledge (namespace topics only).
    #[serde(alias = "queue")]
    Pull,
    /// The emulator POSTs each event to a webhook.
    #[serde(alias = "webhook")]
    Push,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TopicKind {
    #[serde(rename = "custom", alias = "Custom")]
    Custom,
    #[serde(rename = "domain", alias = "Domain")]
    Domain,
    #[serde(rename = "namespace", alias = "Namespace")]
    Namespace,
    /// An event source of another emulated service (a storage account). Never created through
    /// the topic API; see `sources.rs`.
    #[serde(rename = "source", alias = "Source")]
    Source,
}

impl TopicKind {
    pub fn label(self) -> &'static str {
        match self {
            TopicKind::Custom => "custom",
            TopicKind::Domain => "domain",
            TopicKind::Namespace => "namespace",
            TopicKind::Source => "source",
        }
    }
}

/// The input schema of a custom topic or domain. `Auto` accepts all three and works out
/// which one each request uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputSchema {
    #[default]
    #[serde(rename = "Auto", alias = "auto")]
    Auto,
    #[serde(rename = "EventGridSchema", alias = "EventGrid", alias = "eventgrid")]
    EventGrid,
    #[serde(rename = "CloudEventSchemaV1_0", alias = "CloudEvents", alias = "cloudevents")]
    CloudEvents,
    #[serde(rename = "CustomEventSchema", alias = "CustomInputSchema", alias = "Custom", alias = "custom")]
    Custom,
}

impl InputSchema {
    pub fn schema(self) -> Option<Schema> {
        match self {
            InputSchema::Auto => None,
            InputSchema::EventGrid => Some(Schema::EventGrid),
            InputSchema::CloudEvents => Some(Schema::CloudEvents),
            InputSchema::Custom => Some(Schema::Custom),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    /// Attempts before the event is dead-lettered (1..=30 on Azure; any value >= 1 here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delivery_attempts: Option<u32>,
    /// How long the event may stay undelivered (1..=1440 minutes on Azure; fractions allowed here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_time_to_live_in_minutes: Option<f64>,
    /// Emulator extension: wait before retry n (the last value repeats). Overrides the
    /// service-wide backoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_intervals_ms: Option<Vec<u64>>,
}

/// A subscription as the user declares it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionConfig {
    /// `pull` or `push`. Default: `push` when `webhookUrl` is given, else `pull`.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "deliveryMode")]
    pub delivery_mode: Option<DeliveryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "endpoint", alias = "url")]
    pub webhook_url: Option<String>,
    /// Push to a Storage queue, a Service Bus queue or topic, or an event hub (or a webhook
    /// written ARM style). See `destination.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<DestinationConfig>,
    /// Shape of delivered events. Default: as published (namespace topics: CloudEvents).
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "eventDeliverySchema")]
    pub delivery_schema: Option<Schema>,
    #[serde(default, alias = "filtersConfiguration")]
    pub filter: Filter,
    /// Pull: how long a received event stays locked (Azure: 60 to 300; any value > 0 here).
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "lockDurationInSeconds")]
    pub lock_duration_seconds: Option<f64>,
    /// Pull: deliveries before the event is dead-lettered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delivery_count: Option<u32>,
    /// Pull: how long an event is kept.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "eventTimeToLiveInSeconds")]
    pub event_time_to_live_seconds: Option<f64>,
    #[serde(default)]
    pub retry_policy: RetryPolicy,
    /// Push: do not run the validation handshake when the subscription is created.
    #[serde(default)]
    pub skip_validation: bool,
}

/// Values with defaults applied.
#[derive(Clone, Copy, Debug)]
pub struct PullParams {
    pub lock_duration: Duration,
    pub max_delivery: u32,
    pub ttl: Duration,
}

impl SubscriptionConfig {
    pub fn mode(&self) -> DeliveryMode {
        self.delivery_mode.unwrap_or(if self.webhook_url.is_some() || self.destination.is_some() {
            DeliveryMode::Push
        } else {
            DeliveryMode::Pull
        })
    }

    /// A webhook written as `destination: {endpointType: WebHook, ...}` becomes `webhookUrl`.
    /// Call before `validate`. `Err` is the text of a 400.
    pub fn normalize(&mut self) -> Result<(), String> {
        if let Some(d) = &self.destination {
            if let Resolved::WebHook(url) = d.resolve()? {
                if self.webhook_url.is_some() {
                    return Err("Give either webhookUrl or a destination, not both.".into());
                }
                self.webhook_url = Some(url);
                self.destination = None;
            }
        }
        Ok(())
    }

    /// The checked destination of a subscription that delivers to a queue, topic or hub.
    pub fn handler_destination(&self) -> Option<Resolved> {
        match self.destination.as_ref()?.resolve() {
            Ok(r @ Resolved::Handler { .. }) => Some(r),
            _ => None,
        }
    }

    pub fn pull_params(&self) -> PullParams {
        PullParams {
            lock_duration: secs(self.lock_duration_seconds.unwrap_or(DEFAULT_LOCK_SECS)),
            max_delivery: self.max_delivery_count.unwrap_or(DEFAULT_MAX_DELIVERY).max(1),
            ttl: secs(self.event_time_to_live_seconds.unwrap_or(DEFAULT_TTL_SECS)),
        }
    }

    /// Check a subscription that is being created, with the default rule for webhook hosts (loopback and private
    /// addresses and local names only). `Err` is the text of a 400.
    pub fn validate(&self, kind: TopicKind) -> Result<(), String> {
        self.validate_with(kind, &crate::WebhookPolicy::default())
    }

    /// [`validate`](Self::validate) with the webhook rule of a running service.
    pub fn validate_with(&self, kind: TopicKind, policy: &crate::WebhookPolicy) -> Result<(), String> {
        let mode = self.mode();
        if mode == DeliveryMode::Pull && kind != TopicKind::Namespace {
            return Err("Pull delivery is only available for namespace topics.".into());
        }
        if let Some(d) = &self.destination {
            d.resolve()?;
            if self.webhook_url.is_some() {
                return Err("Give either webhookUrl or a destination, not both.".into());
            }
        }
        if mode == DeliveryMode::Push && self.destination.is_none() {
            match &self.webhook_url {
                None => return Err("A push subscription needs a webhookUrl or a destination.".into()),
                Some(u) => crate::http_client::check_url(u, policy)?,
            }
        }
        if mode == DeliveryMode::Pull && (self.webhook_url.is_some() || self.destination.is_some()) {
            return Err("A pull subscription cannot have a webhookUrl or a destination.".into());
        }
        if let Some(v) = self.lock_duration_seconds {
            if !(v > 0.0 && v.is_finite()) {
                return Err("lockDurationSeconds must be greater than zero.".into());
            }
        }
        if let Some(v) = self.event_time_to_live_seconds {
            if !(v > 0.0 && v.is_finite()) {
                return Err("eventTimeToLiveSeconds must be greater than zero.".into());
            }
        }
        if self.max_delivery_count == Some(0) {
            return Err("maxDeliveryCount must be at least 1.".into());
        }
        if self.retry_policy.max_delivery_attempts == Some(0) {
            return Err("retryPolicy.maxDeliveryAttempts must be at least 1.".into());
        }
        if let Some(v) = self.retry_policy.event_time_to_live_in_minutes {
            if !(v > 0.0 && v.is_finite()) {
                return Err("retryPolicy.eventTimeToLiveInMinutes must be greater than zero.".into());
            }
        }
        self.filter.validate()
    }
}

pub fn secs(v: f64) -> Duration {
    Duration::try_from_secs_f64(v.max(0.0)).unwrap_or(Duration::from_secs(u32::MAX as u64))
}

/// A topic declared in the admin API or the config file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TopicKind>,
    #[serde(default)]
    pub input_schema: InputSchema,
    /// The ARM resource id events of this custom topic carry as `topic` (the ARM layer sends it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
}
