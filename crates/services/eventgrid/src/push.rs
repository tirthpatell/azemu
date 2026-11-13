//! Push delivery: POST each matching event to the subscription's webhook, with the
//! `aeg-*` headers, retries with backoff and dead-lettering; and the validation handshake
//! done when a webhook subscription is created.
//!
//! Every event is delivered by its own tokio task. The state lock is only held to record
//! the outcome of an attempt, never across the HTTP call or a sleep.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{json, Value};

use crate::events::Schema;
use crate::http_client;
use crate::pull::Now;
use crate::state::{now_iso, DeliveryAttempt, PushJob, Validation};
use crate::{Options, Shared};

/// Status codes after which Event Grid does not retry.
fn is_final_status(code: u16) -> bool {
    matches!(code, 400 | 413 | 414)
}

pub fn spawn_jobs(shared: &Arc<Shared>, jobs: Vec<PushJob>) {
    if jobs.is_empty() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("eventgrid: no tokio runtime, {} webhook deliveries dropped", jobs.len());
        return;
    };
    let generation = shared.generation();
    for job in jobs {
        let sh = shared.clone();
        handle.spawn(async move { deliver(sh, job, generation, None).await });
    }
}

/// Start deliveries that were waiting for a retry when the emulator stopped. `resume` is, for each job, the attempts
/// already made and the moment the first attempt was made. They start after a short wait so that every service has
/// loaded its state (a delivery to a Service Bus queue needs the queue).
pub fn resume_jobs(shared: &Arc<Shared>, jobs: Vec<(PushJob, u32, Instant)>) {
    if jobs.is_empty() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("eventgrid: no tokio runtime, {} deliveries that were waiting are dropped", jobs.len());
        return;
    };
    let generation = shared.generation();
    for (job, attempts, started) in jobs {
        let sh = shared.clone();
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            deliver(sh, job, generation, Some((attempts, started))).await
        });
    }
}

/// Wait before retry number `attempt` (the attempt that just failed, starting at 1).
pub fn retry_delay(opts: &Options, intervals_ms: &Option<Vec<u64>>, attempt: u32) -> Duration {
    if let Some(list) = intervals_ms {
        if let Some(last) = list.last() {
            let i = (attempt as usize).saturating_sub(1).min(list.len() - 1);
            return Duration::from_millis(*list.get(i).unwrap_or(last));
        }
    }
    let shift = (attempt.saturating_sub(1)).min(20);
    let ms = opts.retry_base_ms.saturating_mul(1u64 << shift).min(opts.retry_max_ms.max(1));
    Duration::from_millis(ms)
}

fn request_parts(job: &PushJob, attempt: u32) -> (Vec<(String, String)>, Bytes) {
    let (ct, body) = match job.schema {
        Schema::CloudEvents => ("application/cloudevents+json; charset=utf-8", (*job.body).clone()),
        _ => ("application/json; charset=utf-8", Value::Array(vec![(*job.body).clone()])),
    };
    let headers = vec![
        ("Content-Type".to_string(), ct.to_string()),
        ("aeg-subscription-name".to_string(), job.sub_name.to_ascii_uppercase()),
        ("aeg-delivery-count".to_string(), attempt.saturating_sub(1).to_string()),
        ("aeg-data-version".to_string(), job.data_version.clone()),
        ("aeg-metadata-version".to_string(), "1".to_string()),
        ("aeg-event-type".to_string(), "Notification".to_string()),
        ("User-Agent".to_string(), "Azure-EventGrid/1.0".to_string()),
    ];
    (headers, Bytes::from(serde_json::to_vec(&body).unwrap_or_default()))
}

/// Hand the event to the handler registered for the destination kind.
fn run_handler(shared: &Arc<Shared>, job: &PushJob, dest: &crate::state::Dest, attempt: u32) -> Result<(), String> {
    let handler = shared.destinations.read().get(&dest.kind).cloned().ok_or_else(|| {
        format!(
            "No handler is registered for {} destinations (the standalone azemu-eventgrid cannot deliver to them).",
            dest.kind.endpoint_type()
        )
    })?;
    let content_type = match job.schema {
        Schema::CloudEvents => "application/cloudevents+json; charset=utf-8",
        _ => "application/json",
    };
    let properties = vec![
        ("aeg-subscription-name".to_string(), job.sub_name.clone()),
        ("aeg-delivery-count".to_string(), attempt.saturating_sub(1).to_string()),
        ("aeg-event-type".to_string(), "Notification".to_string()),
        ("aeg-metadata-version".to_string(), "1".to_string()),
        ("aeg-data-version".to_string(), job.data_version.clone()),
        ("aeg-output-event-id".to_string(), job.event_id.clone()),
    ];
    let delivery = crate::destination::DestinationDelivery {
        kind: dest.kind,
        target: dest.target.clone(),
        body: Bytes::from(serde_json::to_vec(&*job.body).map_err(|e| e.to_string())?),
        content_type: content_type.to_string(),
        properties,
        event_id: job.event_id.clone(),
        subscription: job.sub_name.clone(),
        attempt,
        ttl_seconds: dest.ttl_seconds,
    };
    handler(&delivery)
}

enum Next {
    Done,
    Retry(Duration),
}

async fn deliver(shared: Arc<Shared>, job: PushJob, generation: u64, resume: Option<(u32, Instant)>) {
    let started = resume.map(|r| r.1).unwrap_or_else(Instant::now);
    let timeout = Duration::from_millis(shared.opts.delivery_timeout_ms.max(1));
    let mut attempt: u32 = resume.map(|r| r.0).unwrap_or(0);
    loop {
        attempt += 1;
        if shared.generation() != generation {
            return;
        }
        // A retry of a delivery whose subscription was emptied or removed since (a reset of one topic) does not go out.
        if attempt > 1 && !shared.state.lock().sub(&job.key).is_some_and(|s| s.pending.contains_key(&job.id)) {
            return;
        }
        // Shed (dead-lettered because the retry queue was full) before or between attempts: stop and free the event.
        tokio::select! {
            biased;
            _ = job.cancel.notified() => return,
            _ = std::future::ready(()) => {}
        }
        let t0 = Instant::now();
        let (success, status, detail) = match &job.dest {
            Some(dest) => match run_handler(&shared, &job, dest, attempt) {
                Ok(()) => {
                    (true, None, format!("Delivered to {} {}/{}", dest.kind.endpoint_type(), dest.target.namespace, dest.target.entity))
                }
                Err(e) => (false, None, e),
            },
            None => {
                let (headers, body) = request_parts(&job, attempt);
                let result = http_client::send("POST", &job.url, &headers, body, timeout, &shared.opts.webhooks).await;
                match &result {
                    Ok(r) if (200..=204).contains(&r.status) => (true, Some(r.status), format!("HTTP {}", r.status)),
                    Ok(r) => (false, Some(r.status), format!("HTTP {}", r.status)),
                    Err(e) => (false, None, e.clone()),
                }
            }
        };
        let duration_ms = t0.elapsed().as_millis() as u64;
        let now = Now::now();
        let next = {
            let mut st = shared.state.lock();
            if shared.generation() != generation {
                return;
            }
            let Some(sub) = st.sub_mut(&job.key) else { return };
            // Shed while the attempt ran: its event is already dead-lettered, nothing more to record.
            let Some(size) = sub.pending.get(&job.id).map(|p| p.bytes) else { return };
            let mk = |outcome: &str, detail: String| DeliveryAttempt {
                time: now_iso(&now),
                event_id: job.event_id.clone(),
                attempt,
                outcome: outcome.to_string(),
                status_code: status,
                detail,
                duration_ms,
            };
            if success {
                sub.finish_pending(job.id);
                sub.delivered += 1;
                sub.log_attempt(mk("Success", detail));
                Next::Done
            } else {
                sub.failed_attempts += 1;
                let final_status = status.map(is_final_status).unwrap_or(false);
                let out_of_attempts = attempt >= job.max_attempts;
                let out_of_time = started.elapsed() >= job.ttl;
                if final_status || out_of_attempts || out_of_time {
                    let reason = if final_status {
                        "NonRetryableStatusCode"
                    } else if out_of_time {
                        "TimeToLiveExpired"
                    } else {
                        "MaxDeliveryAttemptsExceeded"
                    };
                    sub.finish_pending(job.id);
                    sub.queue.dead_letter_sized((*job.body).clone(), size, reason, &format!("Last outcome: {detail}"), attempt, &now);
                    sub.log_attempt(mk("DeadLettered", format!("{detail}; {reason}")));
                    Next::Done
                } else {
                    sub.log_attempt(mk("Failure", detail));
                    if let Some(p) = sub.pending.get_mut(&job.id) {
                        p.attempts = attempt;
                    }
                    Next::Retry(retry_delay(&shared.opts, &job.intervals_ms, attempt))
                }
            }
        };
        match next {
            Next::Done => return,
            Next::Retry(d) => {
                tokio::select! {
                    _ = tokio::time::sleep(d) => {}
                    _ = job.cancel.notified() => return,
                }
            }
        }
    }
}

/// Run the validation handshake against a webhook that is being subscribed.
/// `ce` selects the CloudEvents handshake (an OPTIONS request); otherwise the Event Grid
/// `SubscriptionValidationEvent` is POSTed. `validation_url` is what a manual validation
/// would call.
pub async fn handshake(opts: &Options, url: &str, topic_id: &str, validation_url: &str, ce: bool, code: &str) -> Validation {
    let timeout = Duration::from_millis(opts.delivery_timeout_ms.max(1));
    if ce {
        let headers = vec![
            ("WebHook-Request-Origin".to_string(), "eventgrid.azure.net".to_string()),
            ("WebHook-Request-Callback".to_string(), validation_url.to_string()),
            ("WebHook-Request-Rate".to_string(), "120".to_string()),
            ("User-Agent".to_string(), "Azure-EventGrid/1.0".to_string()),
        ];
        return match http_client::send("OPTIONS", url, &headers, Bytes::new(), timeout, &opts.webhooks).await {
            Err(e) => Validation::new("Failed", &format!("The OPTIONS request failed: {e}")),
            Ok(r) if !(200..=299).contains(&r.status) => {
                Validation::new("Failed", &format!("The endpoint answered the OPTIONS request with HTTP {}.", r.status))
            }
            Ok(r) => match r.header("webhook-allowed-origin") {
                Some(o) if o.split(',').any(|x| matches!(x.trim(), "*" | "eventgrid.azure.net")) => {
                    Validation::new("Succeeded", "The endpoint allowed the origin eventgrid.azure.net.")
                }
                Some(o) => Validation::new("Failed", &format!("WebHook-Allowed-Origin was '{o}', expected 'eventgrid.azure.net' or '*'.")),
                None => Validation::new("Failed", "The OPTIONS answer has no WebHook-Allowed-Origin header."),
            },
        };
    }
    let event = json!([{
        "id": uuid::Uuid::new_v4().to_string(),
        "topic": topic_id,
        "subject": "",
        "data": { "validationCode": code, "validationUrl": validation_url },
        "eventType": "Microsoft.EventGrid.SubscriptionValidationEvent",
        "eventTime": now_iso(&Now::now()),
        "metadataVersion": "1",
        "dataVersion": "2"
    }]);
    let headers = vec![
        ("Content-Type".to_string(), "application/json; charset=utf-8".to_string()),
        ("aeg-event-type".to_string(), "SubscriptionValidation".to_string()),
        ("aeg-subscription-name".to_string(), "VALIDATION".to_string()),
        ("aeg-delivery-count".to_string(), "0".to_string()),
        ("aeg-metadata-version".to_string(), "1".to_string()),
        ("aeg-data-version".to_string(), "2".to_string()),
        ("User-Agent".to_string(), "Azure-EventGrid/1.0".to_string()),
    ];
    let body = Bytes::from(serde_json::to_vec(&event).unwrap_or_default());
    match http_client::send("POST", url, &headers, body, timeout, &opts.webhooks).await {
        Err(e) => Validation::new("Failed", &format!("The validation event could not be delivered: {e}")),
        Ok(r) if !(200..=299).contains(&r.status) => {
            Validation::new("Failed", &format!("The endpoint answered the validation event with HTTP {}.", r.status))
        }
        Ok(r) => {
            let answer: Option<Value> = serde_json::from_slice(&r.body).ok();
            match answer.as_ref().and_then(|v| v.get("validationResponse")).and_then(Value::as_str) {
                Some(c) if c == code => Validation::new("Succeeded", "The endpoint echoed the validation code."),
                Some(_) => Validation::new("Failed", "The endpoint answered with a different validationResponse."),
                None => Validation::new(
                    "AwaitingManualAction",
                    "The endpoint answered without a validationResponse; GET the validationUrl to validate manually.",
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let o = Options { retry_base_ms: 100, retry_max_ms: 500, ..Options::default() };
        let d = |a| retry_delay(&o, &None, a).as_millis();
        assert_eq!((d(1), d(2), d(3), d(4), d(5), d(30)), (100, 200, 400, 500, 500, 500));
    }

    #[test]
    fn explicit_intervals_repeat_the_last() {
        let o = Options::default();
        let list = Some(vec![10, 20]);
        let d = |a| retry_delay(&o, &list, a).as_millis();
        assert_eq!((d(1), d(2), d(3), d(9)), (10, 20, 20, 20));
    }

    #[test]
    fn final_statuses() {
        assert!(is_final_status(400));
        assert!(is_final_status(413));
        assert!(!is_final_status(500));
        assert!(!is_final_status(404));
    }
}
