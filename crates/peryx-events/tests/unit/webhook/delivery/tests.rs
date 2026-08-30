use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicUsize};

use peryx_storage::meta::{MetaStore, NewWebhookDelivery, WebhookEventIntent};
use rstest::rstest;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::oneshot;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::webhook::{WebhookEnvelope, WebhookRuntime, WebhookTargetConfig};

#[rstest]
#[case::future(1_100, 1_000, 100)]
#[case::equal(1_000, 1_000, 1)]
#[case::past(900, 1_000, 1)]
#[case::far_past(0, i64::MAX, 1)]
#[case::far_future(i64::MAX, i64::MIN, MAX_SCHEDULER_SLEEP_SECS)]
fn test_wait_secs_bounds_scheduler_wakeups(#[case] next: i64, #[case] now: i64, #[case] expected: u64) {
    assert_eq!(wait_secs(next, now), expected);
}

#[test]
fn test_backoff_caps() {
    assert_eq!(backoff_secs(1), 5);
    assert_eq!(backoff_secs(3), 45);
    assert_eq!(backoff_secs(10), 300);
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
    now: AtomicI64,
}

impl WebhookHost for TestHost {
    fn webhooks(&self) -> &WebhookRuntime {
        &self.webhooks
    }
    fn meta(&self) -> &MetaStore {
        &self.meta
    }
    fn now(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

#[rstest]
#[case::permanent(0, false, WebhookDeliveryStatus::Failed, None)]
#[case::transient(0, true, WebhookDeliveryStatus::Pending, Some(1_005))]
#[case::attempt_limit(4, true, WebhookDeliveryStatus::Failed, None)]
fn test_record_failure_only_reschedules_retriable_responses(
    #[case] prior_attempts: u16,
    #[case] retriable: bool,
    #[case] expected: WebhookDeliveryStatus,
    #[case] next_attempt_at_unix: Option<i64>,
) {
    let dir = tempfile::tempdir().unwrap();
    let host = TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(1_000),
    };
    let id = host
        .meta()
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "hosted",
            target: "ci",
            event: "resource-write",
            payload: "{}",
            created_at_unix: 10,
        })
        .unwrap();
    for _ in 0..prior_attempts {
        let delivery = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
        record_failure(&host, &delivery, host.now(), None, Some(404), "http status 404", true).unwrap();
    }

    let delivery = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
    record_failure(
        &host,
        &delivery,
        host.now(),
        None,
        Some(404),
        "http status 404",
        retriable,
    )
    .unwrap();

    let stored = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
    assert_eq!(
        (
            stored.status,
            stored.attempts,
            stored.next_attempt_at_unix,
            stored.response_status,
            stored.last_error.as_deref(),
        ),
        (
            expected,
            prior_attempts + 1,
            next_attempt_at_unix,
            Some(404),
            Some("http status 404")
        )
    );
}

#[test]
fn test_delivery_logs_report_results_and_storage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("webhook.log");
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(std::fs::File::create(&log).unwrap())
        .finish();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let id = enqueue(&meta, "ci", 10);
    let delivery = meta.get_webhook_delivery(&id).unwrap().unwrap();
    let error = MetaStore::open(dir.path()).expect_err("opening a directory as a database succeeded");

    tracing::subscriber::with_default(subscriber, || {
        log_delivery_success(Some(&delivery), 200);
        log_delivery_failure(Some(&delivery));
        log_delivery_success(None, 200);
        log_delivery_failure(None);
        log_update_error(&error);
    });

    let output = std::fs::read_to_string(log).unwrap();
    for message in [
        "webhook delivery succeeded",
        "webhook delivery failed",
        "webhook result update failed",
    ] {
        assert_eq!(output.matches(message).count(), 1, "unexpected log count: {message}");
    }
}

fn target_config(name: &str, url: &str) -> WebhookTargetConfig {
    WebhookTargetConfig {
        index: "hosted".to_owned(),
        name: name.to_owned(),
        url: url.to_owned(),
        secret: "test-webhook-signing-secret-32-bytes".to_owned(),
        events: Vec::new(),
        allowed_events: &["management", "resource-delete", "resource-write"],
    }
}

fn event() -> WebhookEvent {
    WebhookEvent {
        created_at_unix: 1,
        index: "hosted".to_owned(),
        envelope: WebhookEnvelope::new("owner.v1", "resource-write", serde_json::json!({"key": "value"})),
    }
}

#[test]
fn test_prepare_skips_disabled_webhooks() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(1),
    });

    assert_eq!(prepare(host.as_ref(), &event()), None);

    assert_eq!(host.meta().next_webhook_delivery_at().unwrap(), None);
}

#[test]
fn test_prepare_skips_targets_not_subscribed_to_the_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = target_config("ci", "https://example.invalid/hook");
    config.events = vec!["resource-delete".to_owned()];
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![config]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(1),
    });

    assert_eq!(prepare(host.as_ref(), &event()), None);

    assert_eq!(host.meta().next_webhook_delivery_at().unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_committed_event_is_delivered_with_the_signed_payload() {
    let mut healthy = observed_status_server(200);
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", &healthy.url)]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });

    let handle = kick(Arc::clone(&host)).unwrap();
    commit_event(host.as_ref(), &event());
    healthy.requests.recv().await.unwrap();
    let id = host.meta().list_webhook_deliveries().unwrap()[0].id.clone();
    commit_event(host.as_ref(), &event());
    healthy.requests.recv().await.unwrap();

    let delivered = host.meta().get_webhook_delivery(&id).unwrap().unwrap();

    assert_eq!(
        (
            delivered.target.as_str(),
            delivered.event.as_str(),
            delivered.status,
            delivered.attempts,
            delivered.response_status,
            delivered.last_error.as_deref(),
        ),
        (
            "ci",
            "resource-write",
            WebhookDeliveryStatus::Delivered,
            1,
            Some(200),
            None,
        )
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&delivered.payload).unwrap(),
        serde_json::json!({"key": "value"})
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_worker_recovers_an_event_intent_after_store_reopen() {
    let mut audit = observed_status_server(200);
    let mut deploy = observed_status_server(200);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let targets = vec!["audit".to_owned(), "deploy".to_owned()];
    meta.commit_driver_txn(|txn| {
        txn.enqueue_webhook_event(WebhookEventIntent {
            index: "hosted".to_owned(),
            targets,
            event: "resource-write".to_owned(),
            payload: r#"{"key":"value"}"#.to_owned(),
            created_at_unix: 100,
        });
        Ok::<_, peryx_storage::meta::MetaError>(((), Vec::new()))
    })
    .unwrap();
    assert!(meta.next_webhook_event_id().unwrap().is_some());
    assert_eq!(audit.requests.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(deploy.requests.try_recv(), Err(TryRecvError::Empty));
    drop(meta);
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![
            target_config("audit", &audit.url),
            target_config("deploy", &deploy.url),
        ])
        .unwrap(),
        meta: MetaStore::open_existing(path).unwrap(),
        now: AtomicI64::new(100),
    });

    let handle = kick(Arc::clone(&host)).unwrap();
    audit.requests.recv().await.unwrap();
    deploy.requests.recv().await.unwrap();
    let deliveries = host.meta().list_webhook_deliveries().unwrap();

    assert_eq!(deliveries.len(), 2);
    assert_eq!(
        deliveries
            .iter()
            .map(|delivery| delivery.target.as_str())
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from(["audit", "deploy"])
    );
    assert_eq!(host.meta().next_webhook_event_id().unwrap(), None);
    handle.shutdown().await.unwrap();
}

fn commit_event(host: &TestHost, event: &WebhookEvent) {
    let intent = prepare(host, event).unwrap();
    host.meta()
        .commit_driver_txn(|txn| {
            txn.enqueue_webhook_event(intent);
            Ok::<_, peryx_storage::meta::MetaError>(((), Vec::new()))
        })
        .unwrap();
    notify(host);
}

fn enqueue(meta: &MetaStore, target: &str, created_at_unix: i64) -> String {
    meta.enqueue_webhook_delivery(NewWebhookDelivery {
        index: "hosted",
        target,
        event: "resource-write",
        payload: "{}",
        created_at_unix,
    })
    .unwrap()
}

struct HangingServer {
    url: String,
    accepted: Arc<AtomicUsize>,
    accepted_events: UnboundedReceiver<usize>,
    address: std::net::SocketAddr,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HangingServer {
    // A dedicated thread prevents Tokio worker starvation.
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("http://{address}/hook");
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let (accepted_sender, accepted_events) = unbounded_channel();
        let (shutdown, stopped) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if stopped.try_recv().is_ok() {
                    break;
                }
                let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = accepted_sender.send(count);
                held.push(stream);
                debug_assert_eq!(held.len(), count);
            }
        });
        Self {
            url,
            accepted,
            accepted_events,
            address,
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }

    async fn wait_for_accepted(&mut self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.accepted_events.recv().await.expect("hanging server stopped") < count {}
        })
        .await
        .expect("hanging server never accepted enough connections");
    }
}

impl Drop for HangingServer {
    fn drop(&mut self) {
        self.shutdown.take().unwrap().send(()).unwrap();
        std::net::TcpStream::connect(self.address).unwrap();
        self.thread.take().unwrap().join().unwrap();
    }
}

struct StatusServer {
    url: String,
    requests: UnboundedReceiver<()>,
    address: std::net::SocketAddr,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for StatusServer {
    fn drop(&mut self) {
        self.shutdown.take().unwrap().send(()).unwrap();
        std::net::TcpStream::connect(self.address).unwrap();
        self.thread.take().unwrap().join().unwrap();
    }
}

fn observed_status_server(status: u16) -> StatusServer {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/hook");
    let (request_sender, requests) = unbounded_channel();
    let (shutdown, stopped) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            if stopped.try_recv().is_ok() {
                break;
            }
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = request_sender.send(());
            let response = format!("HTTP/1.1 {status} Test\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = stream.write_all(response.as_bytes());
        }
    });
    StatusServer {
        url,
        requests,
        address,
        shutdown: Some(shutdown),
        thread: Some(thread),
    }
}

#[tokio::test]
async fn test_kick_has_zero_runtime_for_disabled_webhooks() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });
    assert!(kick(Arc::clone(&host)).is_none());
    assert!(!host.webhooks.running.load(Ordering::Acquire));
}

#[tokio::test]
async fn test_kick_owns_worker_startup_and_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", "https://example.invalid/hook")]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });

    let handle = kick(Arc::clone(&host)).unwrap();
    assert!(host.webhooks.running.load(Ordering::Acquire));

    handle.shutdown().await.unwrap();
    assert!(!host.webhooks.running.load(Ordering::Acquire));
}

#[tokio::test]
async fn test_worker_failure_reaches_its_owner() {
    struct PanickingHost(TestHost);

    impl WebhookHost for PanickingHost {
        fn webhooks(&self) -> &WebhookRuntime {
            self.0.webhooks()
        }

        fn meta(&self) -> &MetaStore {
            self.0.meta()
        }

        fn now(&self) -> i64 {
            panic!("injected webhook worker failure")
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(PanickingHost(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", "https://example.invalid/hook")]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    }));
    enqueue(host.meta(), "ci", 10);
    let mut handle = kick(host).unwrap();

    let failure = handle.wait_for_failure().await;
    let repeated = handle.wait_for_failure().await;

    assert!(
        failure.to_string().contains("injected webhook worker failure"),
        "{failure}"
    );
    assert_eq!(repeated.to_string(), failure.to_string());
    assert!(handle.shutdown().await.is_err());
}

#[tokio::test]
async fn test_kick_reuses_the_running_worker() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", "https://example.invalid/hook")]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });
    let handle = kick(Arc::clone(&host)).unwrap();

    assert!(kick(Arc::clone(&host)).is_none());

    handle.shutdown().await.unwrap();
    kick(host).unwrap().shutdown().await.unwrap();
}

#[tokio::test]
async fn test_dropped_handle_releases_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", "https://example.invalid/hook")]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });
    drop(kick(Arc::clone(&host)).unwrap());
    tokio::time::timeout(Duration::from_secs(1), host.webhooks.wait_until_idle())
        .await
        .expect("dropped webhook owner kept its worker running");

    kick(host).unwrap().shutdown().await.unwrap();
}

#[tokio::test]
async fn test_worker_discards_malformed_delivery_and_reaches_valid_work() {
    let mut healthy = observed_status_server(200);
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&database).unwrap();
    let damaged = enqueue(&meta, "broken", 10);
    enqueue(&meta, "healthy", 11);
    drop(meta);
    let raw = redb::Database::open(&database).unwrap();
    let write = raw.begin_write().unwrap();
    write
        .open_table(redb::TableDefinition::<&str, &[u8]>::new("webhook_delivery"))
        .unwrap()
        .insert(damaged.as_str(), b"{".as_slice())
        .unwrap();
    write.commit().unwrap();
    drop(raw);
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("healthy", &healthy.url)]).unwrap(),
        meta: MetaStore::open_existing(database).unwrap(),
        now: AtomicI64::new(100),
    });
    let handle = kick(Arc::clone(&host)).unwrap();

    tokio::time::timeout(Duration::from_secs(1), healthy.requests.recv())
        .await
        .expect("valid webhook remained behind malformed JSON")
        .expect("healthy webhook server stopped");

    assert_eq!(host.meta().get_webhook_delivery(&damaged).unwrap(), None);
    handle.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_worker_retries_deadline_reads_with_positive_capped_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let (host, mut calls) = faulted_host(
        WebhookRuntime::new(vec![target_config("ci", "https://example.invalid/hook")]).unwrap(),
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        8,
        0,
    );
    let handle = kick(Arc::clone(&host)).unwrap();

    assert!(receive_store_call(&mut calls, StoreOperation::Deadline).await.failed);
    for (index, delay) in [100, 200, 400, 800, 1_600, 3_200, 5_000, 5_000]
        .into_iter()
        .map(Duration::from_millis)
        .enumerate()
    {
        tokio::time::advance(delay.checked_sub(Duration::from_millis(1)).unwrap()).await;
        tokio::task::yield_now().await;
        assert!(matches!(calls.try_recv(), Err(TryRecvError::Empty)));
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(
            receive_store_call(&mut calls, StoreOperation::Deadline).await.failed,
            index < 7
        );
    }

    assert!(host.webhooks().running.load(Ordering::Acquire));
    handle.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_storage_backoff_log_reports_its_delay() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("webhook.log");
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::fs::File::create(&log).unwrap())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let (host, mut calls) = faulted_host(
        WebhookRuntime::new(vec![target_config("ci", "https://example.invalid/hook")]).unwrap(),
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        1,
        0,
    );
    let handle = kick(Arc::clone(&host)).unwrap();

    assert!(receive_store_call(&mut calls, StoreOperation::Deadline).await.failed);
    notify(host.as_ref());
    assert!(!receive_store_call(&mut calls, StoreOperation::Deadline).await.failed);
    handle.shutdown().await.unwrap();
    drop(guard);

    let output = std::fs::read_to_string(log).unwrap();
    assert!(output.contains("retry_after_ms=100"), "{output}");
}

#[tokio::test(start_paused = true)]
async fn test_notification_interrupts_due_scan_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let (host, mut calls) = faulted_host(
        WebhookRuntime::new(vec![target_config("ci", "https://example.invalid/hook")]).unwrap(),
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        0,
        1,
    );
    let handle = kick(Arc::clone(&host)).unwrap();

    assert!(receive_store_call(&mut calls, StoreOperation::Due).await.failed);
    let before = tokio::time::Instant::now();
    notify(host.as_ref());

    assert!(!receive_store_call(&mut calls, StoreOperation::Due).await.failed);
    assert_eq!(tokio::time::Instant::now(), before);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_failed_status_write_suppresses_resend_and_retains_the_attempt() {
    let unstable = MockServer::start().await;
    let healthy = MockServer::start().await;
    let (unstable_sender, mut unstable_requests) = unbounded_channel();
    Mock::given(method("POST"))
        .respond_with(ObservedResponseSequence {
            statuses: Mutex::new(VecDeque::from([500, 200])),
            requests: unstable_sender,
        })
        .expect(2)
        .mount(&unstable)
        .await;
    let (healthy_sender, mut healthy_requests) = unbounded_channel();
    Mock::given(method("POST"))
        .respond_with(ObservedResponseSequence {
            statuses: Mutex::new(VecDeque::from([200])),
            requests: healthy_sender,
        })
        .expect(1)
        .mount(&healthy)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let unstable_id = enqueue(&meta, "unstable", 10);
    let healthy_id = enqueue(&meta, "healthy", 10);
    let (host, mut calls) = faulted_host(
        WebhookRuntime::new(vec![
            target_config("unstable", &unstable.uri()),
            target_config("healthy", &healthy.uri()),
        ])
        .unwrap(),
        meta,
        0,
        0,
    );
    host.block_update(&unstable_id);
    let handle = kick(Arc::clone(&host)).unwrap();

    assert_eq!(unstable_requests.recv().await.unwrap(), unstable_id);
    assert_eq!(healthy_requests.recv().await.unwrap(), healthy_id);
    let mut initial_updates = receive_updates(&mut calls, [&unstable_id, &healthy_id]).await;
    assert!(initial_updates.remove(&unstable_id).unwrap().failed);
    assert!(!initial_updates.remove(&healthy_id).unwrap().failed);
    assert_eq!(
        host.meta().get_webhook_delivery(&healthy_id).unwrap().unwrap().status,
        WebhookDeliveryStatus::Delivered
    );
    for _ in 0..2 {
        notify(host.as_ref());
        assert!(receive_update(&mut calls, &unstable_id).await.failed);
        assert_eq!(unstable_requests.try_recv(), Err(TryRecvError::Empty));
    }

    host.inner.now.store(104, Ordering::SeqCst);
    host.unblock_update();
    notify(host.as_ref());
    assert!(!receive_update(&mut calls, &unstable_id).await.failed);
    let delivery = host.meta().get_webhook_delivery(&unstable_id).unwrap().unwrap();
    assert_eq!(
        (
            delivery.status,
            delivery.attempts,
            delivery.updated_at_unix,
            delivery.next_attempt_at_unix,
            delivery.response_status,
            delivery.last_error.as_deref(),
        ),
        (
            WebhookDeliveryStatus::Pending,
            1,
            100,
            Some(105),
            Some(500),
            Some("http status 500"),
        )
    );
    assert_eq!(unstable_requests.try_recv(), Err(TryRecvError::Empty));

    host.inner.now.store(105, Ordering::SeqCst);
    notify(host.as_ref());
    assert_eq!(unstable_requests.recv().await.unwrap(), unstable_id);
    assert!(!receive_update(&mut calls, &unstable_id).await.failed);
    let delivery = host.meta().get_webhook_delivery(&unstable_id).unwrap().unwrap();
    assert_eq!(
        (delivery.status, delivery.attempts),
        (WebhookDeliveryStatus::Delivered, 2)
    );

    handle.shutdown().await.unwrap();
    notify(host.as_ref());
    tokio::task::yield_now().await;
    assert_eq!(unstable_requests.try_recv(), Err(TryRecvError::Empty));
    unstable.verify().await;
    healthy.verify().await;
}

#[tokio::test]
async fn test_delivery_records_a_target_removed_from_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let id = enqueue(&meta, "removed", 10);
    let (host, mut calls) = faulted_host(
        WebhookRuntime::new(vec![target_config("configured", "https://example.invalid/hook")]).unwrap(),
        meta,
        0,
        0,
    );
    let handle = kick(Arc::clone(&host)).unwrap();

    let update = receive_update(&mut calls, &id).await;
    assert!(!update.failed);
    assert!(!update.missing);
    let delivery = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
    assert_eq!(
        (
            delivery.status,
            delivery.attempts,
            delivery.next_attempt_at_unix,
            delivery.last_error.as_deref(),
        ),
        (
            WebhookDeliveryStatus::Failed,
            1,
            None,
            Some("webhook target is not configured"),
        )
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_worker_accepts_a_delivery_removed_before_its_status_write() {
    let server = MockServer::start().await;
    let (request_sender, mut requests) = unbounded_channel();
    Mock::given(method("POST"))
        .respond_with(ObservedResponseSequence {
            statuses: Mutex::new(VecDeque::from([200])),
            requests: request_sender,
        })
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let id = enqueue(&meta, "ci", 10);
    let (host, mut calls) = faulted_host(
        WebhookRuntime::new(vec![target_config("ci", &server.uri())]).unwrap(),
        meta,
        0,
        0,
    );
    host.remove_on_update(&id);
    let handle = kick(Arc::clone(&host)).unwrap();

    assert_eq!(requests.recv().await.unwrap(), id);
    let update = receive_update(&mut calls, &id).await;
    assert!(!update.failed);
    assert!(update.missing);
    assert!(!receive_store_call(&mut calls, StoreOperation::Deadline).await.failed);
    notify(host.as_ref());
    assert!(!receive_store_call(&mut calls, StoreOperation::Deadline).await.failed);
    assert_eq!(requests.try_recv(), Err(TryRecvError::Empty));

    handle.shutdown().await.unwrap();
    server.verify().await;
}

#[tokio::test]
async fn test_wait_for_work_consumes_a_pending_notification() {
    let dir = tempfile::tempdir().unwrap();
    let host = TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    };
    host.webhooks.notify.notify_one();

    wait_for_work(&host).await.unwrap();
}

#[tokio::test]
async fn test_empty_scheduler_wait_resumes_on_notification() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });
    let (entered_wait, entered) = oneshot::channel();
    let (completed, mut completion) = oneshot::channel();
    let waiting = tokio::spawn({
        let host = Arc::clone(&host);
        async move {
            let result = wait_for_work_after(host.as_ref(), || entered_wait.send(()).unwrap()).await;
            completed.send(()).unwrap();
            result
        }
    });

    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completion.try_recv(), Err(oneshot::error::TryRecvError::Empty));
    host.webhooks.notify.notify_one();

    completion.await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_scheduled_wait_resumes_at_delivery_time() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });
    enqueue(host.meta(), "ci", 101);
    let (entered_wait, entered) = oneshot::channel();
    let waiting = tokio::spawn({
        let host = Arc::clone(&host);
        async move { wait_for_work_after(host.as_ref(), || entered_wait.send(()).unwrap()).await }
    });

    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap()
        .unwrap();
    assert!(!waiting.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;

    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_scheduled_wait_caps_a_far_future_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });
    enqueue(host.meta(), "ci", i64::MAX);
    let (entered_wait, entered) = oneshot::channel();
    let waiting = tokio::spawn({
        let host = Arc::clone(&host);
        async move { wait_for_work_after(host.as_ref(), || entered_wait.send(()).unwrap()).await }
    });
    entered.await.unwrap();

    tokio::time::advance(Duration::from_secs(MAX_SCHEDULER_SLEEP_SECS)).await;

    waiting.await.unwrap().unwrap();
    assert_eq!(host.meta().next_webhook_delivery_at().unwrap(), Some(i64::MAX));
}

#[tokio::test]
async fn test_scheduled_wait_resumes_early_on_notification() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });
    enqueue(host.meta(), "ci", 1000);
    let (entered_wait, entered) = oneshot::channel();
    let (completed, mut completion) = oneshot::channel();
    let waiting = tokio::spawn({
        let host = Arc::clone(&host);
        async move {
            let result = wait_for_work_after(host.as_ref(), || entered_wait.send(()).unwrap()).await;
            completed.send(()).unwrap();
            result
        }
    });

    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completion.try_recv(), Err(oneshot::error::TryRecvError::Empty));
    host.webhooks.notify.notify_one();

    completion.await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[rstest]
#[case::redirect(
    302,
    WebhookDeliveryStatus::Failed,
    "webhook target returned redirect 302; redirects are not followed"
)]
#[case::transient(500, WebhookDeliveryStatus::Pending, "http status 500")]
#[tokio::test]
async fn test_delivery_records_http_failures(
    #[case] status: u16,
    #[case] expected: WebhookDeliveryStatus,
    #[case] message: &str,
) {
    let server = observed_status_server(status);
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", &server.url)]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });
    let id = enqueue(host.meta(), "ci", 10);

    deliver_due(&host, &mut DeliveryState::default()).await.unwrap();

    let delivery = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
    assert_eq!(delivery.status, expected);
    assert_eq!(delivery.response_status, Some(status));
    assert_eq!(delivery.last_error.as_deref(), Some(message));
}

#[rstest]
#[case::delay_seconds("120", 1_120)]
#[case::http_date("Thu, 01 Jan 1970 00:18:20 GMT", 1_100)]
#[case::local_backoff_wins("1", 1_005)]
#[case::past_http_date("Thu, 01 Jan 1970 00:15:00 GMT", 1_005)]
#[case::invalid("soon", 1_005)]
#[case::overflow("18446744073709551615", i64::MAX)]
#[tokio::test]
async fn test_retry_after_persists_the_later_deadline(#[case] header: &str, #[case] expected: i64) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", header))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("peryx.redb");
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", &server.uri())]).unwrap(),
        meta: MetaStore::open(&database).unwrap(),
        now: AtomicI64::new(1_000),
    });
    let id = enqueue(host.meta(), "ci", 10);

    let mut state = DeliveryState::default();
    deliver_due(&host, &mut state).await.unwrap();
    deliver_due(&host, &mut state).await.unwrap();
    server.verify().await;
    drop(host);

    let meta = MetaStore::open_existing(database).unwrap();
    let delivery = meta.get_webhook_delivery(&id).unwrap().unwrap();
    assert_eq!(
        (
            delivery.status,
            delivery.attempts,
            delivery.next_attempt_at_unix,
            delivery.response_status,
        ),
        (WebhookDeliveryStatus::Pending, 1, Some(expected), Some(429))
    );
}

#[tokio::test]
async fn test_retry_after_uses_the_response_time_clock() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", &server.uri())]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(1_000),
    });
    let response_host = Arc::clone(&host);
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            response_host.now.store(1_100, Ordering::SeqCst);
            ResponseTemplate::new(429).insert_header("retry-after", "Thu, 01 Jan 1970 00:17:30 GMT")
        })
        .expect(1)
        .mount(&server)
        .await;
    let id = enqueue(host.meta(), "ci", 10);

    deliver_due(&host, &mut DeliveryState::default()).await.unwrap();

    let delivery = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
    assert_eq!(delivery.updated_at_unix, 1_100);
    assert_eq!(delivery.next_attempt_at_unix, Some(1_105));
    server.verify().await;
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
        now: AtomicI64::new(100),
    });
    let id = enqueue(host.meta(), "ci", 10);

    deliver_due(&host, &mut DeliveryState::default()).await.unwrap();

    let delivery = host.meta().get_webhook_delivery(&id).unwrap().unwrap();
    assert_eq!(delivery.status, WebhookDeliveryStatus::Pending);
    assert_eq!(delivery.attempts, 1);
    assert!(delivery.last_error.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_slow_target_does_not_block_a_healthy_one() {
    let mut slow = HangingServer::start();
    let mut healthy = observed_status_server(200);
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    for created_at in 10..13 {
        enqueue(&meta, "slow", created_at);
    }
    enqueue(&meta, "healthy", 20);
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![
            target_config("slow", &slow.url),
            target_config("healthy", &healthy.url),
        ])
        .unwrap(),
        meta,
        now: AtomicI64::new(100),
    });

    let handle = kick(host.clone()).unwrap();

    slow.wait_for_accepted(1).await;
    tokio::time::timeout(Duration::from_secs(5), healthy.requests.recv())
        .await
        .expect("healthy target was blocked")
        .expect("healthy server stopped");
    assert_eq!(slow.accepted.load(Ordering::SeqCst), 1);
    assert!(
        host.meta()
            .list_webhook_deliveries()
            .unwrap()
            .into_iter()
            .filter(|record| record.target == "slow")
            .all(|record| record.status == WebhookDeliveryStatus::Pending && record.attempts == 0),
        "no slow delivery advanced while its target hung"
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_in_flight_requests_stay_within_the_global_bound() {
    let mut slow = HangingServer::start();
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
        now: AtomicI64::new(100),
    });

    let handle = kick(host.clone()).unwrap();

    slow.wait_for_accepted(MAX_CONCURRENT_DELIVERIES).await;
    assert_eq!(slow.accepted.load(Ordering::SeqCst), MAX_CONCURRENT_DELIVERIES);
    assert_eq!(slow.accepted_events.try_recv(), Err(TryRecvError::Empty));
    handle.shutdown().await.unwrap();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreOperation {
    Deadline,
    Due,
    Update,
}

#[derive(Debug)]
struct StoreCall {
    operation: StoreOperation,
    failed: bool,
    update: Option<UpdateCall>,
}

#[derive(Debug)]
struct UpdateCall {
    delivery_id: String,
    failed: bool,
    missing: bool,
}

enum UpdateFault {
    None,
    Block(String),
    Vanish { delivery_id: String, removed: bool },
}

struct FaultHost {
    inner: TestHost,
    deadline_failures: AtomicUsize,
    due_failures: AtomicUsize,
    update_fault: Mutex<UpdateFault>,
    calls: tokio::sync::mpsc::UnboundedSender<StoreCall>,
}

impl FaultHost {
    fn block_update(&self, delivery_id: &str) {
        *self.update_fault.lock().unwrap() = UpdateFault::Block(delivery_id.to_owned());
    }

    fn unblock_update(&self) {
        *self.update_fault.lock().unwrap() = UpdateFault::None;
    }

    fn remove_on_update(&self, delivery_id: &str) {
        *self.update_fault.lock().unwrap() = UpdateFault::Vanish {
            delivery_id: delivery_id.to_owned(),
            removed: false,
        };
    }
}

impl WebhookHost for FaultHost {
    fn webhooks(&self) -> &WebhookRuntime {
        self.inner.webhooks()
    }

    fn meta(&self) -> &MetaStore {
        self.inner.meta()
    }

    fn now(&self) -> i64 {
        self.inner.now()
    }

    fn list_due_webhook_deliveries(
        &self,
        now_unix: i64,
        limit: usize,
        excluded: &HashSet<(String, String)>,
    ) -> Result<Vec<WebhookDeliveryRecord>, MetaError> {
        let failed = take_failure(&self.due_failures);
        let mut result = if failed {
            Err(injected_storage_error("due scan"))
        } else {
            self.meta().list_due_webhook_deliveries(now_unix, limit, excluded)
        };
        if let (
            Ok(deliveries),
            UpdateFault::Vanish {
                delivery_id,
                removed: true,
            },
        ) = (&mut result, &*self.update_fault.lock().unwrap())
        {
            deliveries.retain(|delivery| delivery.id != delivery_id.as_str());
        }
        let _ = self.calls.send(StoreCall {
            operation: StoreOperation::Due,
            failed,
            update: None,
        });
        result
    }

    fn next_webhook_delivery_at(&self) -> Result<Option<i64>, MetaError> {
        let failed = take_failure(&self.deadline_failures);
        let result = if failed {
            Err(injected_storage_error("deadline read"))
        } else if matches!(
            &*self.update_fault.lock().unwrap(),
            UpdateFault::Vanish { removed: true, .. }
        ) {
            Ok(None)
        } else {
            self.meta().next_webhook_delivery_at()
        };
        let _ = self.calls.send(StoreCall {
            operation: StoreOperation::Deadline,
            failed,
            update: None,
        });
        result
    }

    fn update_webhook_delivery(
        &self,
        id: &str,
        attempt: WebhookDeliveryAttempt<'_>,
    ) -> Result<Option<WebhookDeliveryRecord>, MetaError> {
        let result = match &mut *self.update_fault.lock().unwrap() {
            UpdateFault::Block(delivery_id) if delivery_id == id => Err(injected_storage_error("status write")),
            UpdateFault::Vanish { delivery_id, removed } if delivery_id == id => {
                *removed = true;
                Ok(None)
            }
            UpdateFault::None | UpdateFault::Block(_) | UpdateFault::Vanish { .. } => {
                self.meta().update_webhook_delivery(id, attempt)
            }
        };
        let failed = result.is_err();
        let missing = matches!(&result, Ok(None));
        let _ = self.calls.send(StoreCall {
            operation: StoreOperation::Update,
            failed,
            update: Some(UpdateCall {
                delivery_id: id.to_owned(),
                failed,
                missing,
            }),
        });
        result
    }
}

struct ObservedResponseSequence {
    statuses: Mutex<VecDeque<u16>>,
    requests: tokio::sync::mpsc::UnboundedSender<String>,
}

impl Respond for ObservedResponseSequence {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.requests
            .send(request.headers["x-peryx-delivery"].to_str().unwrap().to_owned())
            .unwrap();
        ResponseTemplate::new(self.statuses.lock().unwrap().pop_front().unwrap())
    }
}

fn faulted_host(
    webhooks: WebhookRuntime,
    meta: MetaStore,
    deadline_failures: usize,
    due_failures: usize,
) -> (Arc<FaultHost>, UnboundedReceiver<StoreCall>) {
    let (calls, receiver) = unbounded_channel();
    (
        Arc::new(FaultHost {
            inner: TestHost {
                webhooks,
                meta,
                now: AtomicI64::new(100),
            },
            deadline_failures: AtomicUsize::new(deadline_failures),
            due_failures: AtomicUsize::new(due_failures),
            update_fault: Mutex::new(UpdateFault::None),
            calls,
        }),
        receiver,
    )
}

async fn receive_store_call(calls: &mut UnboundedReceiver<StoreCall>, operation: StoreOperation) -> StoreCall {
    loop {
        let call = calls
            .recv()
            .await
            .expect("webhook worker stopped reporting store calls");
        if call.operation == operation {
            return call;
        }
    }
}

async fn receive_update(calls: &mut UnboundedReceiver<StoreCall>, delivery_id: &str) -> UpdateCall {
    loop {
        if let Some(update) = calls
            .recv()
            .await
            .expect("webhook worker stopped reporting store calls")
            .update
            && update.delivery_id == delivery_id
        {
            return update;
        }
    }
}

async fn receive_updates<const N: usize>(
    calls: &mut UnboundedReceiver<StoreCall>,
    delivery_ids: [&String; N],
) -> HashMap<String, UpdateCall> {
    let mut updates = HashMap::new();
    while updates.len() < delivery_ids.len() {
        if let Some(update) = calls
            .recv()
            .await
            .expect("webhook worker stopped reporting store calls")
            .update
            && delivery_ids.contains(&&update.delivery_id)
        {
            updates.insert(update.delivery_id.clone(), update);
        }
    }
    updates
}

fn take_failure(remaining: &AtomicUsize) -> bool {
    remaining
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| count.checked_sub(1))
        .is_ok()
}

fn injected_storage_error(operation: &str) -> MetaError {
    redb::StorageError::from(std::io::Error::other(format!("injected webhook {operation} failure"))).into()
}
