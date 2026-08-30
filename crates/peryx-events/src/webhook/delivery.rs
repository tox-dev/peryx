use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, UNIX_EPOCH};

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use peryx_storage::meta::{
    MetaError, WebhookDeliveryAttempt, WebhookDeliveryRecord, WebhookDeliveryStatus, WebhookEventIntent,
};

use super::event::WebhookEvent;
use super::host::WebhookHost;
use super::signature::signature;

const MAX_CONCURRENT_DELIVERIES: usize = 16;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
const INITIAL_BACKOFF_SECS: i64 = 5;
const MAX_BACKOFF_SECS: i64 = 300;
const MAX_ATTEMPTS: u16 = 5;
const MAX_SCHEDULER_SLEEP_SECS: u64 = 60 * 60;
const INITIAL_STORAGE_BACKOFF: Duration = Duration::from_millis(100);
const MAX_STORAGE_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum RetryAfter {
    DelaySeconds(u64),
    AtUnix(i64),
}

/// # Panics
/// Panics if JSON serialization fails.
pub fn prepare<H: WebhookHost>(host: &H, event: &WebhookEvent) -> Option<WebhookEventIntent> {
    if host.webhooks().is_empty() {
        return None;
    }
    let targets = host.webhooks().target_names(&event.index, event.envelope.event);
    if targets.is_empty() {
        return None;
    }
    Some(WebhookEventIntent {
        index: event.index.clone(),
        targets,
        event: event.envelope.event.to_owned(),
        payload: serde_json::to_string(&event.envelope.data).expect("webhook payload is valid JSON"),
        created_at_unix: event.created_at_unix,
    })
}

pub fn notify<H: WebhookHost>(host: &H) {
    host.webhooks().notify.notify_one();
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
    stopped: tokio::sync::watch::Sender<()>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.stopped.send_replace(());
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
    let mut state = DeliveryState::default();
    let mut storage_backoff = INITIAL_STORAGE_BACKOFF;
    loop {
        let result = tokio::select! {
            biased;
            _ = &mut cancelled => return Ok(()),
            result = delivery_cycle(&host, &mut state) => result,
        };
        match result {
            Ok(()) => storage_backoff = INITIAL_STORAGE_BACKOFF,
            Err(error) => {
                tracing::warn!(
                    target: "peryx::webhook",
                    error = ?error,
                    retry_after_ms = storage_backoff.as_millis(),
                    "webhook storage access failed; retrying"
                );
                tokio::select! {
                    biased;
                    _ = &mut cancelled => return Ok(()),
                    () = host.webhooks().notify.notified() => {}
                    () = tokio::time::sleep(storage_backoff) => {}
                }
                storage_backoff = next_storage_backoff(storage_backoff);
            }
        }
    }
}

#[derive(Default)]
struct DeliveryState {
    pending_updates: HashMap<(String, String), PendingUpdate>,
}

#[derive(Debug)]
struct FailedUpdate {
    pending: PendingUpdate,
    error: MetaError,
}

#[derive(Debug)]
struct PendingUpdate {
    delivery_id: String,
    status: WebhookDeliveryStatus,
    updated_at_unix: i64,
    next_attempt_at_unix: Option<i64>,
    response_status: Option<u16>,
    last_error: Option<String>,
}

impl PendingUpdate {
    fn attempt(&self) -> WebhookDeliveryAttempt<'_> {
        WebhookDeliveryAttempt {
            status: self.status,
            updated_at_unix: self.updated_at_unix,
            next_attempt_at_unix: self.next_attempt_at_unix,
            response_status: self.response_status,
            last_error: self.last_error.as_deref(),
        }
    }
}

async fn delivery_cycle<H: WebhookHost>(host: &Arc<H>, state: &mut DeliveryState) -> Result<(), MetaError> {
    recover_webhook_events(host.as_ref())?;
    deliver_due(host, state).await?;
    wait_for_work(host.as_ref()).await
}

fn recover_webhook_events<H: WebhookHost>(host: &H) -> Result<(), MetaError> {
    while let Some(id) = host.meta().next_webhook_event_id()? {
        host.meta().fan_out_webhook_event(&id)?;
    }
    Ok(())
}

async fn wait_for_work<H: WebhookHost>(host: &H) -> Result<(), MetaError> {
    wait_for_work_after(host, || {}).await
}

async fn wait_for_work_after<H: WebhookHost>(host: &H, entered_wait: impl FnOnce()) -> Result<(), MetaError> {
    let next = host.next_webhook_delivery_at()?;
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

/// Bounded sleeps re-read persisted deadlines after clock changes without overflowing `Instant`.
fn wait_secs(next: i64, now: i64) -> u64 {
    u64::try_from(next.saturating_sub(now))
        .unwrap_or(0)
        .clamp(1, MAX_SCHEDULER_SLEEP_SECS)
}

fn next_storage_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_STORAGE_BACKOFF)
}

/// Per-target serialization preserves order without letting one target block another.
async fn deliver_due<H: WebhookHost>(host: &Arc<H>, state: &mut DeliveryState) -> Result<(), MetaError> {
    let mut failure = retry_pending_updates(host.as_ref(), state);
    let mut in_flight = FuturesUnordered::new();
    let mut busy = state.pending_updates.keys().cloned().collect::<HashSet<_>>();
    let mut scan_failed = false;
    loop {
        while !scan_failed {
            let now = host.now();
            let want = MAX_CONCURRENT_DELIVERIES.saturating_sub(in_flight.len());
            let deliveries = match host.list_due_webhook_deliveries(now, want, &busy) {
                Ok(deliveries) => deliveries,
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                    scan_failed = true;
                    break;
                }
            };
            if deliveries.is_empty() {
                break;
            }
            for delivery in deliveries {
                let key = (delivery.index.clone(), delivery.target.clone());
                busy.insert(key.clone());
                in_flight.push(async move {
                    let result = deliver_one(host, delivery).await;
                    (key, result)
                });
            }
        }
        let Some((key, result)) = in_flight.next().await else {
            return failure.map_or(Ok(()), Err);
        };
        match result {
            Ok(()) => {
                busy.remove(&key);
            }
            Err(update) => {
                let update = *update;
                if failure.is_none() {
                    failure = Some(update.error);
                }
                state.pending_updates.insert(key, update.pending);
            }
        }
    }
}

fn retry_pending_updates<H: WebhookHost>(host: &H, state: &mut DeliveryState) -> Option<MetaError> {
    let mut failure = None;
    for (key, pending) in std::mem::take(&mut state.pending_updates) {
        match persist_attempt(host, &pending) {
            Ok(()) => {}
            Err(error) => {
                if failure.is_none() {
                    failure = Some(error);
                }
                state.pending_updates.insert(key, pending);
            }
        }
    }
    failure
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

async fn deliver_one<H: WebhookHost>(host: &Arc<H>, delivery: WebhookDeliveryRecord) -> Result<(), Box<FailedUpdate>> {
    let sent_at = host.now();
    let Some(target) = host.webhooks().target(&delivery.index, &delivery.target) else {
        return record_failure(
            host.as_ref(),
            &delivery,
            sent_at,
            None,
            None,
            "webhook target is not configured",
            false,
        );
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
        .header("x-peryx-timestamp", sent_at.to_string())
        .header(
            "x-peryx-signature",
            signature(&target.secret, sent_at, &delivery.id, delivery.payload.as_bytes()),
        )
        .body(delivery.payload.clone())
        .send()
        .await;
    let completed_at = host.now();
    match result {
        Ok(response) if response.status().is_success() => {
            record_success(host.as_ref(), &delivery, completed_at, response.status().as_u16())
        }
        Ok(response) if response.status().is_redirection() => {
            // Retrying a redirect could expose the signed payload outside the configured origin.
            let status = response.status().as_u16();
            record_failure(
                host.as_ref(),
                &delivery,
                completed_at,
                None,
                Some(status),
                &format!("webhook target returned redirect {status}; redirects are not followed"),
                false,
            )
        }
        Ok(response) => {
            let status = response.status().as_u16();
            let retry_after = retry_after(response.headers());
            record_failure(
                host.as_ref(),
                &delivery,
                completed_at,
                retry_after,
                Some(status),
                &format!("http status {status}"),
                !is_permanent(status),
            )
        }
        Err(err) => record_failure(
            host.as_ref(),
            &delivery,
            completed_at,
            None,
            None,
            &err.without_url().to_string(),
            true,
        ),
    }
}

fn record_success<H: WebhookHost>(
    host: &H,
    delivery: &WebhookDeliveryRecord,
    now: i64,
    status: u16,
) -> Result<(), Box<FailedUpdate>> {
    persist_new_attempt(
        host,
        PendingUpdate {
            delivery_id: delivery.id.clone(),
            status: WebhookDeliveryStatus::Delivered,
            updated_at_unix: now,
            next_attempt_at_unix: None,
            response_status: Some(status),
            last_error: None,
        },
    )
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
    retry_after: Option<RetryAfter>,
    response_status: Option<u16>,
    error: &str,
    retriable: bool,
) -> Result<(), Box<FailedUpdate>> {
    let attempts = delivery.attempts + 1;
    let (status, next_attempt_at_unix) = if retriable && attempts < MAX_ATTEMPTS {
        let local_deadline = now.saturating_add(backoff_secs(attempts));
        let server_deadline = retry_after.map_or(now, |retry_after| match retry_after {
            RetryAfter::DelaySeconds(seconds) => i64::try_from(seconds)
                .ok()
                .and_then(|seconds| now.checked_add(seconds))
                .unwrap_or(i64::MAX),
            RetryAfter::AtUnix(deadline) => deadline,
        });
        (
            WebhookDeliveryStatus::Pending,
            Some(local_deadline.max(server_deadline)),
        )
    } else {
        (WebhookDeliveryStatus::Failed, None)
    };
    persist_new_attempt(
        host,
        PendingUpdate {
            delivery_id: delivery.id.clone(),
            status,
            updated_at_unix: now,
            next_attempt_at_unix,
            response_status,
            last_error: Some(error.to_owned()),
        },
    )
}

fn persist_new_attempt<H: WebhookHost>(host: &H, pending: PendingUpdate) -> Result<(), Box<FailedUpdate>> {
    persist_attempt(host, &pending).map_err(|error| Box::new(FailedUpdate { pending, error }))
}

fn persist_attempt<H: WebhookHost>(host: &H, pending: &PendingUpdate) -> Result<(), MetaError> {
    let result = host.update_webhook_delivery(&pending.delivery_id, pending.attempt());
    match result.as_ref() {
        Ok(record) if pending.status == WebhookDeliveryStatus::Delivered => {
            log_delivery_success(
                record.as_ref(),
                pending
                    .response_status
                    .expect("a successful webhook attempt has a response status"),
            );
        }
        Ok(record) => log_delivery_failure(record.as_ref()),
        Err(error) => log_update_error(Some(error)),
    }
    result.map(drop)
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<RetryAfter> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();
    value.parse().ok().map(RetryAfter::DelaySeconds).or_else(|| {
        let seconds = httpdate::parse_http_date(value)
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs();
        Some(RetryAfter::AtUnix(i64::try_from(seconds).unwrap_or(i64::MAX)))
    })
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
