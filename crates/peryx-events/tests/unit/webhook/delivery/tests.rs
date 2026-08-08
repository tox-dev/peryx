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

fn event() -> WebhookEvent {
    WebhookEvent {
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
    }
}

#[test]
fn test_emit_skips_disabled_webhooks() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: 1,
    });

    emit(Arc::clone(&host), &event());

    assert_eq!(host.meta().next_webhook_delivery_at().unwrap(), None);
}

#[test]
fn test_emit_skips_targets_not_subscribed_to_the_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = target_config("ci", "https://example.invalid/hook");
    config.events = vec!["delete".to_owned()];
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![config]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: 1,
    });

    emit(Arc::clone(&host), &event());

    assert_eq!(host.meta().next_webhook_delivery_at().unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_emit_enqueues_and_delivers_the_signed_payload() {
    let healthy_url = healthy_server();
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", &healthy_url)]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: 100,
    });

    emit(Arc::clone(&host), &event());

    let delivered = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(delivery) = host.meta().list_webhook_deliveries().unwrap().into_iter().next()
                && delivery.status == WebhookDeliveryStatus::Delivered
            {
                break delivery;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert_eq!(delivered.target, "ci");
    assert_eq!(delivered.event, "upload");
    assert_eq!(delivered.response_status, Some(200));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&delivered.payload).unwrap()["project"],
        "demo"
    );
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

fn status_server(status: u16) -> String {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf);
            let response = format!("HTTP/1.1 {status} Test\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = stream.write_all(response.as_bytes());
        }
    });
    url
}

fn healthy_server() -> String {
    status_server(200)
}

#[tokio::test]
async fn test_kick_notifies_an_active_worker() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: 100,
    });
    host.webhooks.running.store(true, Ordering::Release);

    kick(Arc::clone(&host));

    tokio::time::timeout(Duration::from_secs(1), host.webhooks.notify.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn test_empty_scheduler_wait_resumes_on_notification() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: 100,
    });
    let waiting = tokio::spawn({
        let host = Arc::clone(&host);
        async move { wait_for_work(host.as_ref()).await }
    });
    tokio::task::yield_now().await;

    host.webhooks.notify.notify_one();

    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn test_scheduled_wait_resumes_at_delivery_time() {
    let dir = tempfile::tempdir().unwrap();
    let host = TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: 100,
    };
    enqueue(host.meta(), "ci", 101);

    tokio::time::timeout(Duration::from_secs(2), wait_for_work(&host))
        .await
        .unwrap();

    assert_eq!(host.meta().next_webhook_delivery_at().unwrap(), Some(101));
}

#[tokio::test]
async fn test_scheduled_wait_resumes_early_on_notification() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: 100,
    });
    enqueue(host.meta(), "ci", 1000);
    let waiting = tokio::spawn({
        let host = Arc::clone(&host);
        async move { wait_for_work(host.as_ref()).await }
    });
    tokio::task::yield_now().await;

    host.webhooks.notify.notify_one();

    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn test_delivery_retries_when_its_target_was_removed() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: 100,
    });
    let id = enqueue(host.meta(), "removed", 10);

    deliver_due(&host).await;

    let delivery = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
    assert_eq!(delivery.status, WebhookDeliveryStatus::Pending);
    assert_eq!(delivery.attempts, 1);
    assert_eq!(delivery.last_error.as_deref(), Some("webhook target is not configured"));
}

#[rstest]
#[case::redirect(302, WebhookDeliveryStatus::Failed, "redirect 302")]
#[case::transient(500, WebhookDeliveryStatus::Pending, "http status 500")]
#[tokio::test]
async fn test_delivery_records_http_failures(
    #[case] status: u16,
    #[case] expected: WebhookDeliveryStatus,
    #[case] message: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", &status_server(status))]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: 100,
    });
    let id = enqueue(host.meta(), "ci", 10);

    deliver_due(&host).await;

    let delivery = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
    assert_eq!(delivery.status, expected);
    assert_eq!(delivery.response_status, Some(status));
    assert!(delivery.last_error.as_deref().unwrap().contains(message));
}

#[tokio::test]
async fn test_delivery_retries_network_failures() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    drop(listener);
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", &url)]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: 100,
    });
    let id = enqueue(host.meta(), "ci", 10);

    deliver_due(&host).await;

    let delivery = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
    assert_eq!(delivery.status, WebhookDeliveryStatus::Pending);
    assert_eq!(delivery.attempts, 1);
    assert!(delivery.last_error.is_some());
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
