use std::sync::atomic::{AtomicI64, AtomicUsize};

use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::oneshot;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

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
        record_failure(&host, &delivery, host.now(), None, Some(404), "http status 404", true);
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
    );

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
        log_enqueue_error(Some(&error), &event(), "ci");
        log_update_error(Some(&error));
    });

    let output = std::fs::read_to_string(log).unwrap();
    for message in [
        "webhook delivery succeeded",
        "webhook delivery failed",
        "webhook delivery could not be queued",
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
fn test_emit_skips_disabled_webhooks() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::disabled(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(1),
    });

    emit(host.as_ref(), &event());

    assert_eq!(host.meta().next_webhook_delivery_at().unwrap(), None);
}

#[test]
fn test_emit_skips_targets_not_subscribed_to_the_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = target_config("ci", "https://example.invalid/hook");
    config.events = vec!["resource-delete".to_owned()];
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![config]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(1),
    });

    emit(host.as_ref(), &event());

    assert_eq!(host.meta().next_webhook_delivery_at().unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_emit_enqueues_and_delivers_the_signed_payload() {
    let mut healthy = observed_status_server(200);
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", &healthy.url)]).unwrap(),
        meta: MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        now: AtomicI64::new(100),
    });

    let handle = kick(Arc::clone(&host)).unwrap();
    emit(host.as_ref(), &event());
    let id = host.meta().list_webhook_deliveries().unwrap()[0].id.clone();
    healthy.requests.recv().await.unwrap();
    emit(host.as_ref(), &event());
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

#[tokio::test]
async fn test_worker_reports_read_only_store_failure() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("peryx.redb");
    drop(MetaStore::open(&database).unwrap());
    let host = Arc::new(TestHost {
        webhooks: WebhookRuntime::new(vec![target_config("ci", "https://example.invalid/hook")]).unwrap(),
        meta: MetaStore::open_existing_read_only(database).unwrap(),
        now: AtomicI64::new(100),
    });
    let mut handle = kick(host).unwrap();

    let failure = tokio::time::timeout(Duration::from_secs(1), handle.wait_for_failure())
        .await
        .expect("read-only webhook store did not stop its worker");

    assert_eq!(
        failure.to_string(),
        "webhook delivery storage failed: I/O error: metadata store is read-only"
    );
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

    deliver_due(&host).await.unwrap();

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

    deliver_due(&host).await.unwrap();
    deliver_due(&host).await.unwrap();
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

    deliver_due(&host).await.unwrap();

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

    deliver_due(&host).await.unwrap();

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
