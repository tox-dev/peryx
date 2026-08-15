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

/// # Panics
/// Panics if JSON serialization fails.
pub fn emit<H: WebhookHost>(host: &H, event: &WebhookEvent) {
    if host.webhooks().is_empty() {
        return;
    }
    let targets = host.webhooks().target_names(&event.index, event.envelope.event);
    if targets.is_empty() {
        return;
    }
    let payload = serde_json::to_string(&event.envelope.data).expect("webhook payload is valid JSON");
    let event_name = event.envelope.event;
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
        host.webhooks().notify.notify_one();
    }
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct WebhookLifecycleError {
    message: String,
}

pub struct WebhookHandle {
    cancellation: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), WebhookLifecycleError>>,
    completed: Option<Result<(), WebhookLifecycleError>>,
}

struct RunningGuard {
    running: Arc<std::sync::atomic::AtomicBool>,
    stopped: tokio::sync::watch::Sender<u64>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.stopped.send_modify(|generation| *generation += 1);
    }
}

impl WebhookHandle {
    pub async fn wait_for_failure(&mut self) -> WebhookLifecycleError {
        if let Some(result) = self.completed.clone() {
            return webhook_failure(result);
        }
        let result = joined_webhook_task(&mut self.task).await;
        self.completed = Some(result.clone());
        webhook_failure(result)
    }

    /// # Errors
    /// Returns the delivery worker's storage failure or panic.
    pub async fn shutdown(mut self) -> Result<(), WebhookLifecycleError> {
        drop(self.cancellation.take());
        if let Some(result) = self.completed.take() {
            return result;
        }
        joined_webhook_task(&mut self.task).await
    }
}

fn webhook_failure(result: Result<(), WebhookLifecycleError>) -> WebhookLifecycleError {
    result.expect_err("a live delivery worker cannot stop cleanly")
}

impl Drop for WebhookHandle {
    fn drop(&mut self) {
        drop(self.cancellation.take());
        self.task.abort();
    }
}

#[must_use]
pub fn kick<H: WebhookHost>(host: Arc<H>) -> Option<WebhookHandle> {
    if host.webhooks().is_empty() {
        return None;
    }
    if host.webhooks().running.swap(true, Ordering::AcqRel) {
        host.webhooks().notify.notify_one();
        return None;
    }
    let (cancellation, cancelled) = tokio::sync::oneshot::channel();
    let running = RunningGuard {
        running: Arc::clone(&host.webhooks().running),
        stopped: host.webhooks().stopped.clone(),
    };
    Some(WebhookHandle {
        cancellation: Some(cancellation),
        task: tokio::spawn(delivery_loop(host, cancelled, running)),
        completed: None,
    })
}

async fn delivery_loop<H: WebhookHost>(
    host: Arc<H>,
    mut cancelled: tokio::sync::oneshot::Receiver<()>,
    _running: RunningGuard,
) -> Result<(), WebhookLifecycleError> {
    loop {
        tokio::select! {
            biased;
            _ = &mut cancelled => return Ok(()),
            result = delivery_cycle(&host) => result.map_err(|error| WebhookLifecycleError {
                message: format!("webhook delivery storage failed: {error}"),
            })?,
        }
    }
}

async fn delivery_cycle<H: WebhookHost>(host: &Arc<H>) -> Result<(), MetaError> {
    deliver_due(host).await?;
    wait_for_work(host.as_ref()).await
}

async fn wait_for_work<H: WebhookHost>(host: &H) -> Result<(), MetaError> {
    wait_for_work_after(host, || {}).await
}

async fn wait_for_work_after<H: WebhookHost>(host: &H, entered_wait: impl FnOnce()) -> Result<(), MetaError> {
    let next = host.meta().next_webhook_delivery_at()?;
    entered_wait();
    if let Some(next) = next {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(wait_secs(next, host.now()))) => {}
            () = host.webhooks().notify.notified() => {}
        }
    } else {
        host.webhooks().notify.notified().await;
    }
    Ok(())
}

/// Clamping prevents busy-polling after clock drift or a late wakeup.
fn wait_secs(next: i64, now: i64) -> u64 {
    u64::try_from(next.saturating_sub(now)).unwrap_or(0).max(1)
}

/// Per-target serialization preserves order without letting one target block another.
async fn deliver_due<H: WebhookHost>(host: &Arc<H>) -> Result<(), MetaError> {
    let mut in_flight = FuturesUnordered::new();
    let mut busy: HashSet<(String, String)> = HashSet::new();
    loop {
        while in_flight.len() < MAX_CONCURRENT_DELIVERIES {
            let now = host.now();
            let want = MAX_CONCURRENT_DELIVERIES - in_flight.len();
            let deliveries = host.meta().list_due_webhook_deliveries(now, want, &busy)?;
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
            return Ok(());
        };
        busy.remove(&key);
    }
}

async fn joined_webhook_task(
    task: impl std::future::Future<Output = Result<Result<(), WebhookLifecycleError>, tokio::task::JoinError>>,
) -> Result<(), WebhookLifecycleError> {
    task.await.unwrap_or_else(|error| {
        Err(WebhookLifecycleError {
            message: format!("webhook delivery task failed: {error}"),
        })
    })
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
        .header(
            "x-peryx-signature",
            signature(&target.secret, now, &delivery.id, delivery.payload.as_bytes()),
        )
        .body(delivery.payload.clone())
        .send()
        .await;
    match result {
        Ok(response) if response.status().is_success() => {
            record_success(host.as_ref(), &delivery, now, response.status().as_u16());
        }
        Ok(response) if response.status().is_redirection() => {
            // Retrying a redirect could expose the signed payload outside the configured origin.
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
        tracing::error!(
            target: "peryx::webhook",
            error = ?err,
            index = %event.index,
            target = %target,
            schema = event.envelope.schema,
            event = event.envelope.event,
            "webhook delivery could not be queued"
        );
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

/// Timeouts and rate limits can recover; other client errors cannot.
fn is_permanent(status: u16) -> bool {
    (400..500).contains(&status) && status != 408 && status != 429
}

#[cfg(test)]
#[path = "../../tests/unit/webhook/delivery/tests.rs"]
mod tests;
