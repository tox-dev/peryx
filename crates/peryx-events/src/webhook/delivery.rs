//! The delivery pipeline: enqueue, drain the queue, sign and POST each delivery, and retry on failure.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use peryx_storage::meta::{
    MetaError, NewWebhookDelivery, WebhookDeliveryAttempt, WebhookDeliveryRecord, WebhookDeliveryStatus,
};

use super::event::WebhookEvent;
use super::host::WebhookHost;
use super::signature::signature;

const MAX_CONCURRENT_DELIVERIES: usize = 16;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
const INITIAL_BACKOFF_SECS: i64 = 5;
const MAX_BACKOFF_SECS: i64 = 300;
const MAX_ATTEMPTS: u16 = 5;

/// Enqueue signed webhook deliveries for `event` to every configured target subscribed to its kind.
///
/// A no-op when no webhooks are configured or none subscribe to the event's kind.
///
/// # Panics
/// Panics only if the aggregation lock is poisoned; the payload is all JSON primitives and cannot
/// fail to serialize.
pub fn emit<H: WebhookHost>(host: Arc<H>, event: &WebhookEvent) {
    if host.webhooks().is_empty() {
        return;
    }
    let targets = host.webhooks().target_names(&event.index, event.kind);
    if targets.is_empty() {
        return;
    }
    let payload = serde_json::to_string(&event.payload()).expect("webhook payload contains JSON primitives");
    let event_name = event.kind.as_str();
    let mut enqueued = 0;
    for target in targets {
        let result = host.meta().enqueue_webhook_delivery(NewWebhookDelivery {
            index: &event.index,
            target: &target,
            event: event_name,
            payload: &payload,
            created_at_unix: event.created_at_unix,
        });
        log_enqueue_error(result.as_ref().err(), event, &target);
        if result.is_ok() {
            enqueued += 1;
        }
    }
    if enqueued > 0 {
        kick(host);
    }
}

pub fn kick<H: WebhookHost>(host: Arc<H>) {
    if host.webhooks().running.swap(true, Ordering::AcqRel) {
        host.webhooks().notify.notify_one();
        return;
    }
    tokio::spawn(delivery_loop(host));
}

async fn delivery_loop<H: WebhookHost>(host: Arc<H>) {
    loop {
        deliver_due(&host).await;
        let result = host.meta().next_webhook_delivery_at();
        log_next_delivery_error(result.as_ref().err());
        let Some(next) = result.ok().flatten() else {
            host.webhooks().notify.notified().await;
            continue;
        };
        let sleep_secs = wait_secs(next, host.now());
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {}
            () = host.webhooks().notify.notified() => {}
        }
    }
}

/// Seconds to wait before the next scheduling wakeup, clamped to at least one.
///
/// A `next` at or behind `now` - clock drift, a stale queue key, or a loop that woke late - must not
/// collapse into a zero-second sleep, or the loop would busy-poll the store. Saturating subtraction
/// also keeps a far-future `next` from overflowing the signed delta.
fn wait_secs(next: i64, now: i64) -> u64 {
    u64::try_from(next.saturating_sub(now)).unwrap_or(0).max(1)
}

/// Drain every due delivery, running targets concurrently under a global in-flight bound while keeping
/// at most one delivery in flight per target.
///
/// A slow or unresponsive target holds only its own slot. The store hands back at most one due record
/// per target and skips targets already being delivered to, so healthy targets keep filling the pool
/// instead of queueing behind a stalled one, and per-target ordering is preserved. The pool never
/// exceeds [`MAX_CONCURRENT_DELIVERIES`] in-flight requests, and each record's store update runs inside
/// its own future so one slow update cannot pin a slot for the rest.
async fn deliver_due<H: WebhookHost>(host: &Arc<H>) {
    let mut in_flight = FuturesUnordered::new();
    let mut busy: HashSet<(String, String)> = HashSet::new();
    loop {
        while in_flight.len() < MAX_CONCURRENT_DELIVERIES {
            let now = host.now();
            let want = MAX_CONCURRENT_DELIVERIES - in_flight.len();
            let result = host.meta().list_due_webhook_deliveries(now, want, &busy);
            log_queue_scan_error(result.as_ref().err());
            let deliveries = result.unwrap_or_default();
            if deliveries.is_empty() {
                break;
            }
            for delivery in deliveries {
                let key = (delivery.index.clone(), delivery.target.clone());
                busy.insert(key.clone());
                in_flight.push(async move {
                    deliver_one(host, delivery).await;
                    key
                });
            }
        }
        let Some(key) = in_flight.next().await else {
            return;
        };
        busy.remove(&key);
    }
}

async fn deliver_one<H: WebhookHost>(host: &Arc<H>, delivery: WebhookDeliveryRecord) {
    let now = host.now();
    let Some(target) = host.webhooks().target(&delivery.index, &delivery.target) else {
        record_failure(
            host.as_ref(),
            &delivery,
            now,
            None,
            "webhook target is not configured",
            true,
        );
        return;
    };
    let signature = signature(&target.secret, now, &delivery.id, delivery.payload.as_bytes());
    let result = host
        .webhooks()
        .client
        .post(target.url)
        .timeout(DELIVERY_TIMEOUT)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::USER_AGENT,
            concat!("peryx/", env!("CARGO_PKG_VERSION")),
        )
        .header("x-peryx-event", delivery.event.as_str())
        .header("x-peryx-delivery", delivery.id.as_str())
        .header("x-peryx-timestamp", now.to_string())
        .header("x-peryx-signature", signature)
        .body(delivery.payload.clone())
        .send()
        .await;
    match result {
        Ok(response) if response.status().is_success() => {
            record_success(host.as_ref(), &delivery, now, response.status().as_u16());
        }
        Ok(response) if response.status().is_redirection() => {
            // The client never follows a redirect, so re-POSTing the signed payload to the Location the
            // target picks cannot happen; a 3xx that would move delivery off the operator-approved origin
            // is a terminal failure, not a transient one to retry against the same endpoint.
            let status = response.status().as_u16();
            record_failure(
                host.as_ref(),
                &delivery,
                now,
                Some(status),
                &format!("webhook target returned redirect {status}; redirects are not followed"),
                false,
            );
        }
        Ok(response) => {
            let status = response.status().as_u16();
            record_failure(
                host.as_ref(),
                &delivery,
                now,
                Some(status),
                &format!("http status {status}"),
                !is_permanent(status),
            );
        }
        Err(err) => {
            record_failure(
                host.as_ref(),
                &delivery,
                now,
                None,
                &err.without_url().to_string(),
                true,
            );
        }
    }
}

fn record_success<H: WebhookHost>(host: &H, delivery: &WebhookDeliveryRecord, now: i64, status: u16) {
    let result = host.meta().update_webhook_delivery(
        &delivery.id,
        WebhookDeliveryAttempt {
            status: WebhookDeliveryStatus::Delivered,
            updated_at_unix: now,
            next_attempt_at_unix: None,
            response_status: Some(status),
            last_error: None,
        },
    );
    log_update_error(result.as_ref().err());
    log_delivery_success(result.as_ref().ok().and_then(Option::as_ref), status);
}

fn log_delivery_success(record: Option<&WebhookDeliveryRecord>, status: u16) {
    if let Some(record) = record {
        tracing::info!(
            target: "peryx::webhook",
            delivery = %record.id,
            index = %record.index,
            target = %record.target,
            event = %record.event,
            attempts = record.attempts,
            status,
            "webhook delivery succeeded"
        );
    }
}

fn record_failure<H: WebhookHost>(
    host: &H,
    delivery: &WebhookDeliveryRecord,
    now: i64,
    response_status: Option<u16>,
    error: &str,
    retriable: bool,
) {
    let attempts = delivery.attempts + 1;
    let (status, next_attempt_at_unix) = if retriable && attempts < MAX_ATTEMPTS {
        (WebhookDeliveryStatus::Pending, Some(now + backoff_secs(attempts)))
    } else {
        (WebhookDeliveryStatus::Failed, None)
    };
    let result = host.meta().update_webhook_delivery(
        &delivery.id,
        WebhookDeliveryAttempt {
            status,
            updated_at_unix: now,
            next_attempt_at_unix,
            response_status,
            last_error: Some(error),
        },
    );
    log_update_error(result.as_ref().err());
    log_delivery_failure(result.as_ref().ok().and_then(Option::as_ref));
}

fn log_delivery_failure(record: Option<&WebhookDeliveryRecord>) {
    if let Some(record) = record {
        tracing::warn!(
            target: "peryx::webhook",
            delivery = %record.id,
            index = %record.index,
            target = %record.target,
            event = %record.event,
            attempts = record.attempts,
            response_status = ?record.response_status,
            next_attempt_at_unix = ?record.next_attempt_at_unix,
            status = ?record.status,
            "webhook delivery failed"
        );
    }
}

fn log_enqueue_error(err: Option<&MetaError>, event: &WebhookEvent, target: &str) {
    if let Some(err) = err {
        let event_name = event.kind.as_str();
        tracing::error!(
            target: "peryx::webhook",
            error = ?err,
            index = %event.index,
            target = %target,
            event = event_name,
            "webhook delivery could not be queued"
        );
    }
}

fn log_next_delivery_error(err: Option<&MetaError>) {
    if let Some(err) = err {
        tracing::error!(target: "peryx::webhook", error = ?err, "webhook queue scheduling failed");
    }
}

fn log_queue_scan_error(err: Option<&MetaError>) {
    if let Some(err) = err {
        tracing::error!(target: "peryx::webhook", error = ?err, "webhook queue scan failed");
    }
}

fn log_update_error(err: Option<&MetaError>) {
    if let Some(err) = err {
        tracing::error!(target: "peryx::webhook", error = ?err, "webhook result update failed");
    }
}

fn backoff_secs(attempts: u16) -> i64 {
    let mut secs = INITIAL_BACKOFF_SECS;
    for _ in 1..attempts {
        secs = (secs * 3).min(MAX_BACKOFF_SECS);
    }
    secs
}

/// Whether an HTTP status rules out any later success, so the delivery fails at once instead of
/// spending its remaining attempts on a response that will not change. A client error other than
/// `408 Request Timeout` and `429 Too Many Requests` is permanent; both of those, every `5xx`, and a
/// transport error stay retriable.
fn is_permanent(status: u16) -> bool {
    (400..500).contains(&status) && status != 408 && status != 429
}

#[cfg(test)]
#[path = "../../tests/unit/webhook/delivery/tests.rs"]
mod tests;
