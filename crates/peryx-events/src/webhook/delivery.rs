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
mod tests {
    use std::sync::atomic::AtomicUsize;

    use peryx_storage::meta::MetaStore;
    use rstest::rstest;

    use super::*;
    use crate::webhook::{WebhookEventKind, WebhookRuntime, WebhookTargetConfig};

    #[rstest]
    #[case::future(1_100, 1_000, 100)]
    #[case::equal(1_000, 1_000, 1)]
    #[case::past(900, 1_000, 1)]
    #[case::far_past(0, i64::MAX, 1)]
    #[case::overflow_saturates(i64::MAX, i64::MIN, 9_223_372_036_854_775_807)]
    fn test_wait_secs_never_yields_a_zero_delay_wakeup(#[case] next: i64, #[case] now: i64, #[case] expected: u64) {
        assert_eq!(wait_secs(next, now), expected);
    }

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

    #[rstest]
    #[case(400, true)]
    #[case(404, true)]
    #[case(410, true)]
    #[case(422, true)]
    #[case(408, false)]
    #[case(429, false)]
    #[case(500, false)]
    #[case(503, false)]
    fn test_is_permanent_flags_only_non_retriable_client_errors(#[case] status: u16, #[case] permanent: bool) {
        assert_eq!(is_permanent(status), permanent);
    }

    struct TestHost {
        webhooks: WebhookRuntime,
        meta: MetaStore,
        now: i64,
    }

    impl WebhookHost for TestHost {
        fn webhooks(&self) -> &WebhookRuntime {
            &self.webhooks
        }
        fn meta(&self) -> &MetaStore {
            &self.meta
        }
        fn now(&self) -> i64 {
            self.now
        }
    }

    #[rstest]
    #[case::permanent(false, WebhookDeliveryStatus::Failed, false)]
    #[case::transient(true, WebhookDeliveryStatus::Pending, true)]
    fn test_record_failure_only_reschedules_retriable_responses(
        #[case] retriable: bool,
        #[case] expected: WebhookDeliveryStatus,
        #[case] rescheduled: bool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let host = TestHost {
            webhooks: WebhookRuntime::disabled(),
            meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
            now: 1_000,
        };
        assert!(host.webhooks().is_empty(), "no target is needed to record a failure");
        let id = host
            .meta()
            .enqueue_webhook_delivery(NewWebhookDelivery {
                index: "hosted",
                target: "ci",
                event: "upload",
                payload: "{}",
                created_at_unix: 10,
            })
            .unwrap();
        let delivery = host.meta().get_webhook_delivery(&id).unwrap().unwrap();

        record_failure(&host, &delivery, host.now(), Some(404), "http status 404", retriable);

        let stored = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
        assert_eq!(stored.status, expected);
        assert_eq!(stored.attempts, 1);
        assert_eq!(stored.next_attempt_at_unix.is_some(), rescheduled);
    }

    fn target_config(name: &str, url: &str) -> WebhookTargetConfig {
        WebhookTargetConfig {
            index: "hosted".to_owned(),
            name: name.to_owned(),
            url: url.to_owned(),
            secret: "secret".to_owned(),
            events: Vec::new(),
        }
    }

    fn enqueue(meta: &MetaStore, target: &str, created_at_unix: i64) -> String {
        meta.enqueue_webhook_delivery(NewWebhookDelivery {
            index: "hosted",
            target,
            event: "upload",
            payload: "{}",
            created_at_unix,
        })
        .unwrap()
    }

    async fn wait_for_status(host: &Arc<TestHost>, id: &str, status: WebhookDeliveryStatus) -> WebhookDeliveryRecord {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(record) = host.meta().get_webhook_delivery(id).unwrap()
                    && record.status == status
                {
                    return record;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("webhook delivery never reached the expected status")
    }

    /// A server that accepts connections and never answers, counting how many it holds open at once.
    struct HangingServer {
        url: String,
        accepted: Arc<AtomicUsize>,
    }

    impl HangingServer {
        // A blocking std listener on its own thread accepts and holds each connection. Running the accept
        // loop as ordinary thread code, rather than a spawned future, keeps its body on lines the x86
        // line-coverage gate attributes; an async task body only executes inside the runtime's poll and
        // is left uncovered.
        fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/hook", listener.local_addr().unwrap());
            let accepted = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&accepted);
            std::thread::spawn(move || {
                // The vec is never read; its only job is to keep each accepted socket alive so the server
                // holds the connection open and never answers.
                #[allow(
                    clippy::collection_is_never_read,
                    reason = "the vec exists only to hold sockets open"
                )]
                let mut held = Vec::new();
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    counter.fetch_add(1, Ordering::SeqCst);
                    held.push(stream);
                }
            });
            Self { url, accepted }
        }

        async fn wait_for_accepted(&self, count: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.accepted.load(Ordering::SeqCst) < count {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("hanging server never accepted enough connections");
        }
    }

    fn healthy_server() -> String {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            }
        });
        url
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_slow_target_does_not_block_a_healthy_one() {
        let slow = HangingServer::start();
        let healthy_url = healthy_server();
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        for created_at in 10..13 {
            enqueue(&meta, "slow", created_at);
        }
        let healthy_id = enqueue(&meta, "healthy", 20);
        let host = Arc::new(TestHost {
            webhooks: WebhookRuntime::new(vec![
                target_config("slow", &slow.url),
                target_config("healthy", &healthy_url),
            ])
            .unwrap(),
            meta,
            now: 100,
        });

        kick(host.clone());

        let delivered = wait_for_status(&host, &healthy_id, WebhookDeliveryStatus::Delivered).await;
        assert_eq!(delivered.attempts, 1);
        assert_eq!(delivered.response_status, Some(200));
        // The healthy delivery finished while the slow target still holds its one connection open, so one
        // per-target slot is all a stalled endpoint ever takes from the pool.
        assert_eq!(slow.accepted.load(Ordering::SeqCst), 1);
        let slow_pending = host
            .meta()
            .list_webhook_deliveries()
            .unwrap()
            .into_iter()
            .filter(|record| record.target == "slow")
            .all(|record| record.status == WebhookDeliveryStatus::Pending && record.attempts == 0);
        assert!(slow_pending, "no slow delivery advanced while its target hung");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_in_flight_requests_stay_within_the_global_bound() {
        let slow = HangingServer::start();
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        let mut configs = Vec::new();
        for index in 0..MAX_CONCURRENT_DELIVERIES + 4 {
            let name = format!("t{index}");
            enqueue(&meta, &name, 10);
            configs.push(target_config(&name, &slow.url));
        }
        let host = Arc::new(TestHost {
            webhooks: WebhookRuntime::new(configs).unwrap(),
            meta,
            now: 100,
        });

        kick(host.clone());

        slow.wait_for_accepted(MAX_CONCURRENT_DELIVERIES).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(slow.accepted.load(Ordering::SeqCst), MAX_CONCURRENT_DELIVERIES);
    }
}
