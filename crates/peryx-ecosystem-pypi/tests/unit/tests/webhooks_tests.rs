use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use axum::http::{HeaderValue, header};
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::{MetaStore, NewWebhookDelivery, WebhookDeliveryStatus};
use rstest::rstest;
use serde_json::json;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tower::ServiceExt as _;
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WiremockRequest, Respond, ResponseTemplate};

use super::http::{fixture_wheel, multipart_body, request, upload_auth, upload_peryxpkg};
use peryx_driver::state::AppState;
use peryx_events::webhook::{self, WebhookRuntime, WebhookTargetConfig};
use peryx_http::router;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;

const SECRET: &str = "test-webhook-signing-secret-32-bytes";

struct Harness {
    _dir: tempfile::TempDir,
    _delivery_observer: tracing::dispatcher::DefaultGuard,
    _webhook: webhook::WebhookHandle,
    state: Arc<AppState>,
    clock: Arc<AtomicI64>,
    observed: Arc<Mutex<Vec<ObservedDelivery>>>,
    delivery_updates: AsyncMutex<mpsc::UnboundedReceiver<()>>,
}

/// A terminal delivery leaves no row behind, so its outcome is read back from the structured log.
#[derive(Clone, Debug)]
struct ObservedDelivery {
    id: String,
    event: String,
    attempts: u16,
    status: WebhookDeliveryStatus,
    response_status: Option<u16>,
    next_attempt_at_unix: Option<i64>,
    last_error: Option<String>,
}

struct DeliveryLayer {
    observed: Arc<Mutex<Vec<ObservedDelivery>>>,
    updates: mpsc::UnboundedSender<()>,
}

impl<S: Subscriber> Layer<S> for DeliveryLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        if event.metadata().target() != "peryx::webhook" {
            return;
        }
        let mut visitor = DeliveryVisitor(HashMap::new());
        event.record(&mut visitor);
        if let Some(delivery) = observed_delivery(&visitor.0) {
            self.observed.lock().expect("observed deliveries").push(delivery);
            let _ = self.updates.send(());
        }
    }
}

struct DeliveryVisitor(HashMap<&'static str, String>);

impl tracing::field::Visit for DeliveryVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name(), format!("{value:?}"));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name(), value.to_string());
    }
}

fn observed_delivery(fields: &HashMap<&'static str, String>) -> Option<ObservedDelivery> {
    Some(ObservedDelivery {
        id: fields.get("delivery")?.clone(),
        event: fields["event"].clone(),
        attempts: fields["attempts"].parse().expect("attempt count"),
        status: match fields["status"].as_str() {
            "Pending" => WebhookDeliveryStatus::Pending,
            "Delivered" => WebhookDeliveryStatus::Delivered,
            _ => WebhookDeliveryStatus::Failed,
        },
        response_status: logged_some(&fields["response_status"]).map(|value| value.parse().expect("http status")),
        next_attempt_at_unix: logged_some(&fields["next_attempt_at_unix"])
            .map(|value| value.parse().expect("retry deadline")),
        last_error: logged_some(&fields["last_error"]).map(|value| value.trim_matches('"').to_owned()),
    })
}

fn logged_some(value: &str) -> Option<&str> {
    value.strip_prefix("Some(").and_then(|inner| inner.strip_suffix(')'))
}

impl Harness {
    fn new(url: String, events: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(AtomicI64::new(1000));
        let webhooks = WebhookRuntime::new(vec![WebhookTargetConfig {
            index: "hosted".to_owned(),
            name: "ci".to_owned(),
            url,
            secret: SECRET.to_owned(),
            events: events.iter().map(|event| (*event).to_owned()).collect(),
            allowed_events: crate::registration().registration.webhook_events(),
        }])
        .unwrap();
        let (delivery_updates, delivery_receiver) = mpsc::unbounded_channel();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let delivery_observer = tracing::subscriber::set_default(tracing_subscriber::registry().with(DeliveryLayer {
            observed: Arc::clone(&observed),
            updates: delivery_updates,
        }));
        let state = webhook_state(&dir, &clock, webhooks);
        let webhook = webhook::kick(state.serving.clone()).expect("configured webhook worker");
        Self {
            _dir: dir,
            _delivery_observer: delivery_observer,
            _webhook: webhook,
            state,
            clock,
            observed,
            delivery_updates: AsyncMutex::new(delivery_receiver),
        }
    }
}

fn webhook_state(dir: &tempfile::TempDir, clock: &Arc<AtomicI64>, webhooks: WebhookRuntime) -> Arc<AppState> {
    let ticks = Arc::clone(clock);
    let mut state = AppState::with_clock_and_webhooks(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        vec![Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: crate::tests::writer_acl("s3cret".to_owned()),
        }],
        Arc::new(move || ticks.load(Ordering::Relaxed)),
        webhooks,
    );
    super::http::install_distributed_services(&mut state);
    super::wired_distributed(state)
}

struct ResponseSequence {
    statuses: Mutex<VecDeque<u16>>,
    location: Option<String>,
}

impl Respond for ResponseSequence {
    fn respond(&self, _request: &WiremockRequest) -> ResponseTemplate {
        let status = self
            .statuses
            .lock()
            .expect("response sequence lock")
            .pop_front()
            .unwrap_or(200);
        let response = ResponseTemplate::new(status);
        if let Some(location) = &self.location {
            response.insert_header("location", location.as_str())
        } else {
            response
        }
    }
}

async fn wait_for_delivery(harness: &Harness, status: WebhookDeliveryStatus, attempts: u16) -> ObservedDelivery {
    wait_for_outcome(harness, |delivery| {
        delivery.status == status && delivery.attempts == attempts
    })
    .await
}

async fn wait_for_delivery_id(harness: &Harness, id: &str, status: WebhookDeliveryStatus) -> ObservedDelivery {
    wait_for_outcome(harness, |delivery| delivery.id == id && delivery.status == status).await
}

async fn wait_for_outcome(
    harness: &Harness,
    matches: impl Fn(&ObservedDelivery) -> bool + Send + Sync,
) -> ObservedDelivery {
    let mut updates = harness.delivery_updates.lock().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let found = harness
                .observed
                .lock()
                .expect("observed deliveries")
                .iter()
                .find(|delivery| matches(delivery))
                .cloned();
            if let Some(delivery) = found {
                return delivery;
            }
            updates.recv().await.expect("delivery observer remains open");
        }
    })
    .await
    .expect("no webhook delivery outcome matched")
}

#[tokio::test]
async fn test_upload_webhook_is_signed_and_skips_duplicate_upload() {
    let server = webhook_server([200]).await;
    let h = Harness::new(webhook_url(&server), &["upload"]);
    let wheel = fixture_wheel();

    assert_eq!(upload_peryxpkg(&h.state, "/hosted/", &wheel).await, StatusCode::OK);

    let delivery = wait_for_delivery(&h, WebhookDeliveryStatus::Delivered, 1).await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(delivery.event, "upload");
    assert_signed(&requests[0], &delivery.id, "upload", 1000);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap(),
        json!({
            "event": "upload",
            "created_at": 1000,
            "index": "hosted",
            "route": "hosted",
            "hosted_index": "hosted",
            "project": "peryxpkg",
            "version": "1.0",
            "file": {
                "filename": "peryxpkg-1.0-py3-none-any.whl",
                "sha256": Digest::of(&wheel).as_str(),
            },
            "count": 1,
            "actor": "uploader",
        })
    );

    assert_eq!(upload_peryxpkg(&h.state, "/hosted/", &wheel).await, StatusCode::OK);
    assert!(h.state.serving.meta.list_webhook_deliveries().unwrap().is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_upload_event_recovers_every_target_after_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let webhooks = WebhookRuntime::new(
        [("audit", 1), ("deploy", 2)]
            .map(|(name, port)| WebhookTargetConfig {
                index: "hosted".to_owned(),
                name: name.to_owned(),
                url: format!("http://127.0.0.1:{port}/hook"),
                secret: SECRET.to_owned(),
                events: vec!["upload".to_owned()],
                allowed_events: crate::registration().registration.webhook_events(),
            })
            .to_vec(),
    )
    .unwrap();
    let state = webhook_state(&dir, &Arc::new(AtomicI64::new(1000)), webhooks);

    assert_eq!(
        upload_peryxpkg(&state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    assert!(state.serving.meta.list_webhook_deliveries().unwrap().is_empty());
    let event_id = state.serving.meta.next_webhook_event_id().unwrap().unwrap();
    drop(state);

    let meta = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    assert!(meta.fan_out_webhook_event(&event_id).unwrap());
    assert_eq!(meta.next_webhook_event_id().unwrap(), None);
    let deliveries = meta.list_webhook_deliveries().unwrap();
    assert_eq!(deliveries.len(), 2);
    assert_ne!(deliveries[0].id, deliveries[1].id);
    assert_eq!(
        deliveries
            .iter()
            .map(|delivery| delivery.target.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["audit", "deploy"])
    );
}

#[tokio::test]
async fn test_upload_webhook_ignores_invalid_request_id() {
    let server = webhook_server([200]).await;
    let h = Harness::new(webhook_url(&server), &["upload"]);

    assert_eq!(
        upload_with_request_id(&h.state, &fixture_wheel(), b"\xff").await,
        StatusCode::OK
    );

    wait_for_delivery(&h, WebhookDeliveryStatus::Delivered, 1).await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body = serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap();
    assert_eq!(body.get("request_id"), None);
}

#[tokio::test]
async fn test_webhook_worker_wakes_after_idle() {
    let server = webhook_server([200, 200]).await;
    let h = Harness::new(webhook_url(&server), &["upload"]);

    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    wait_for_delivery(&h, WebhookDeliveryStatus::Delivered, 1).await;
    let id = h
        .state
        .serving
        .meta
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "hosted",
            target: "ci",
            event: "upload",
            payload: r#"{"event":"upload"}"#,
            created_at_unix: 1000,
        })
        .unwrap();
    assert!(webhook::kick(h.state.serving.clone()).is_none());

    let delivered = wait_for_delivery_id(&h, &id, WebhookDeliveryStatus::Delivered).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(delivered.attempts, 1);
    assert_eq!(delivered.response_status, Some(200));
}

#[tokio::test]
async fn test_webhook_delivery_retries_failed_request() {
    let server = webhook_server([500, 204]).await;
    let h = Harness::new(webhook_url(&server), &["upload"]);

    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );

    let pending = wait_for_delivery(&h, WebhookDeliveryStatus::Pending, 1).await;
    assert_eq!(pending.response_status, Some(500));
    assert_eq!(pending.next_attempt_at_unix, Some(1005));

    h.clock.store(1005, Ordering::Relaxed);
    assert!(webhook::kick(h.state.serving.clone()).is_none());
    let delivered = wait_for_delivery(&h, WebhookDeliveryStatus::Delivered, 2).await;
    let requests = server.received_requests().await.unwrap();

    assert_eq!(requests.len(), 2);
    assert_eq!(delivered.id, pending.id);
    assert_eq!(delivered.response_status, Some(204));
    assert_signed(&requests[1], &delivered.id, "upload", 1005);
}

#[tokio::test]
async fn test_webhook_delivery_marks_terminal_failure() {
    let server = webhook_server([500; 5]).await;
    let h = Harness::new(webhook_url(&server), &["upload"]);

    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );

    for count in 2..=5 {
        let pending = wait_for_delivery(&h, WebhookDeliveryStatus::Pending, count - 1).await;
        h.clock.store(
            pending.next_attempt_at_unix.expect("scheduled retry"),
            Ordering::Relaxed,
        );
        assert!(webhook::kick(h.state.serving.clone()).is_none());
    }

    let failed = wait_for_delivery(&h, WebhookDeliveryStatus::Failed, 5).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 5);
    assert_eq!(failed.response_status, Some(500));
    assert_eq!(failed.next_attempt_at_unix, None);
    assert_eq!(failed.last_error.as_deref(), Some("http status 500"));
}

#[rstest]
#[case(301)]
#[case(302)]
#[case(307)]
#[case(308)]
#[tokio::test]
async fn test_webhook_redirect_is_a_terminal_failure_and_never_reaches_the_redirect_target(#[case] status: u16) {
    let (trap_url, trap) = trap_origin().await;
    let server = redirecting_webhook_server(status, trap_url).await;
    let h = Harness::new(webhook_url(&server), &["upload"]);

    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );

    let failed = wait_for_delivery(&h, WebhookDeliveryStatus::Failed, 1).await;
    assert_eq!(failed.response_status, Some(status));
    assert_eq!(failed.next_attempt_at_unix, None);
    assert_eq!(
        failed.last_error,
        Some(format!(
            "webhook target returned redirect {status}; redirects are not followed"
        ))
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(trap.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_webhook_delivery_records_request_error() {
    let h = Harness::new("http://127.0.0.1:0/hook".to_owned(), &["upload"]);

    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );

    let pending = wait_for_delivery(&h, WebhookDeliveryStatus::Pending, 1).await;
    assert_eq!(pending.response_status, None);
    assert_eq!(pending.next_attempt_at_unix, Some(1005));
    assert!(pending.last_error.as_deref().is_some_and(|err| !err.contains("/hook")));
}

#[tokio::test]
async fn test_webhook_delivery_records_removed_target() {
    let server = webhook_server([200]).await;
    let h = Harness::new(webhook_url(&server), &["upload"]);
    let id = h
        .state
        .serving
        .meta
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "hosted",
            target: "removed",
            event: "upload",
            payload: r#"{"event":"upload"}"#,
            created_at_unix: 1000,
        })
        .unwrap();

    assert!(webhook::kick(h.state.serving.clone()).is_none());

    let failed = wait_for_delivery(&h, WebhookDeliveryStatus::Failed, 1).await;
    assert_eq!(
        (
            failed.id.as_str(),
            failed.response_status,
            failed.next_attempt_at_unix,
            failed.last_error.as_deref(),
        ),
        (id.as_str(), None, None, Some("webhook target is not configured"))
    );
    assert_eq!(h.state.serving.meta.next_webhook_delivery_at().unwrap(), None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_delete_webhook_emits_index_change() {
    let server = webhook_server([200]).await;
    let h = Harness::new(webhook_url(&server), &["delete"]);

    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/", Some(&upload_auth())).await,
        StatusCode::OK
    );

    let delivery = wait_for_delivery(&h, WebhookDeliveryStatus::Delivered, 1).await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(delivery.event, "delete");
    assert_signed(&requests[0], &delivery.id, "delete", 1000);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap(),
        json!({
            "event": "delete",
            "created_at": 1000,
            "index": "hosted",
            "route": "hosted",
            "hosted_index": "hosted",
            "project": "peryxpkg",
            "count": 1,
            "actor": "uploader",
        })
    );
}

async fn webhook_server(statuses: impl IntoIterator<Item = u16>) -> MockServer {
    start_webhook_server(ResponseSequence {
        statuses: Mutex::new(statuses.into_iter().collect()),
        location: None,
    })
    .await
}

async fn redirecting_webhook_server(status: u16, location: String) -> MockServer {
    start_webhook_server(ResponseSequence {
        statuses: Mutex::new(VecDeque::from([status])),
        location: Some(location),
    })
    .await
}

async fn start_webhook_server(responses: ResponseSequence) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(responses)
        .mount(&server)
        .await;
    server
}

fn webhook_url(server: &MockServer) -> String {
    format!("{}/hook", server.uri())
}

async fn trap_origin() -> (String, MockServer) {
    let server = MockServer::start().await;
    (format!("{}/captured", server.uri()), server)
}

async fn upload_with_request_id(state: &Arc<AppState>, wheel: &[u8], request_id: &[u8]) -> StatusCode {
    let fields = [
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("pyversion", "py3"),
        ("filetype", "bdist_wheel"),
        ("requires_python", ">=3.8"),
    ];
    let (content_type, body) = multipart_body(&fields, Some(("peryxpkg-1.0-py3-none-any.whl", wheel)));
    let mut request = axum::http::Request::builder()
        .uri("/hosted/")
        .method("POST")
        .header(header::CONTENT_TYPE, content_type)
        .header(header::AUTHORIZATION, upload_auth())
        .body(Body::from(body))
        .unwrap();
    request
        .headers_mut()
        .insert("x-request-id", HeaderValue::from_bytes(request_id).unwrap());
    router(state.clone()).oneshot(request).await.unwrap().status()
}

fn assert_signed(request: &WiremockRequest, delivery: &str, event: &str, timestamp: i64) {
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(request.headers["x-peryx-event"], event);
    assert_eq!(request.headers["x-peryx-delivery"], delivery);
    assert_eq!(request.headers["x-peryx-timestamp"], timestamp.to_string());
    assert_eq!(
        request.headers["x-peryx-signature"],
        webhook::signature(SECRET, timestamp, delivery, &request.body)
    );
}
