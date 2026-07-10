//! The delivery pipeline: enqueue, drain the queue, sign and POST each delivery, and retry on failure.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use peryx_storage::meta::{
    MetaError, NewWebhookDelivery, WebhookDeliveryAttempt, WebhookDeliveryRecord, WebhookDeliveryStatus,
};

use super::event::WebhookEvent;
use super::host::WebhookHost;
use super::signature::signature;

const DELIVERY_BATCH: usize = 32;
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
        let now = host.now();
        let sleep_secs = u64::try_from(next - now).unwrap_or(0);
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {}
            () = host.webhooks().notify.notified() => {}
        }
    }
}

async fn deliver_due<H: WebhookHost>(host: &Arc<H>) {
    loop {
        let now = host.now();
        let result = host.meta().list_due_webhook_deliveries(now, DELIVERY_BATCH);
        log_queue_scan_error(result.as_ref().err());
        let deliveries = result.unwrap_or_default();
        if deliveries.is_empty() {
            return;
        }
        for delivery in deliveries {
            deliver_one(host, delivery).await;
        }
    }
}

async fn deliver_one<H: WebhookHost>(host: &Arc<H>, delivery: WebhookDeliveryRecord) {
    let now = host.now();
    let Some(target) = host.webhooks().target(&delivery.index, &delivery.target) else {
        record_failure(host.as_ref(), &delivery, now, None, "webhook target is not configured");
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
        Ok(response) => {
            let status = response.status().as_u16();
            record_failure(
                host.as_ref(),
                &delivery,
                now,
                Some(status),
                &format!("http status {status}"),
            );
        }
        Err(err) => {
            record_failure(host.as_ref(), &delivery, now, None, &err.without_url().to_string());
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
) {
    let attempts = delivery.attempts + 1;
    let (status, next_attempt_at_unix) = if attempts >= MAX_ATTEMPTS {
        (WebhookDeliveryStatus::Failed, None)
    } else {
        (WebhookDeliveryStatus::Pending, Some(now + backoff_secs(attempts)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webhook::WebhookEventKind;

    #[test]
    fn test_backoff_caps() {
        assert_eq!(backoff_secs(1), 5);
        assert_eq!(backoff_secs(3), 45);
        assert_eq!(backoff_secs(10), 300);
    }

    #[test]
    fn test_error_log_helpers_accept_store_errors() {
        let err = MetaError::Decode(serde_json::from_str::<serde_json::Value>("{").unwrap_err());
        let event = WebhookEvent {
            kind: WebhookEventKind::Upload,
            created_at_unix: 1,
            index: "hosted".to_owned(),
            route: "hosted".to_owned(),
            hosted_index: "hosted".to_owned(),
            project: "demo".to_owned(),
            version: None,
            filename: None,
            digest: None,
            count: 1,
            actor: None,
            request_id: None,
        };

        log_enqueue_error(Some(&err), &event, "ci");
        log_next_delivery_error(Some(&err));
        log_queue_scan_error(Some(&err));
        log_update_error(Some(&err));
        log_enqueue_error(None, &event, "ci");
        log_next_delivery_error(None);
        log_queue_scan_error(None);
        log_update_error(None);

        let record = WebhookDeliveryRecord {
            id: "wd_1".to_owned(),
            index: "hosted".to_owned(),
            target: "ci".to_owned(),
            event: "upload".to_owned(),
            payload: "{}".to_owned(),
            status: WebhookDeliveryStatus::Delivered,
            attempts: 1,
            created_at_unix: 1,
            updated_at_unix: 2,
            next_attempt_at_unix: None,
            response_status: Some(204),
            last_error: None,
        };
        log_delivery_success(Some(&record), 204);
        log_delivery_success(None, 204);
        log_delivery_failure(Some(&WebhookDeliveryRecord {
            status: WebhookDeliveryStatus::Pending,
            response_status: Some(500),
            last_error: Some("http status 500".to_owned()),
            ..record
        }));
        log_delivery_failure(None);
    }
}
