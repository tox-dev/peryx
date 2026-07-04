use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use axum::http::{HeaderValue, header};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tower::ServiceExt as _;
use velodex_storage::blob::{BlobStore, Digest};
use velodex_storage::meta::{MetaStore, NewWebhookDelivery, WebhookDeliveryRecord, WebhookDeliveryStatus};

use super::http_tests::{fixture_wheel, multipart_body, request, upload_auth, upload_velodexpkg};
use crate::router;
use crate::state::{AppState, Index, IndexKind};
use crate::webhook::{self, WebhookRuntime, WebhookTargetConfig};
use velodex_policy::Policy;

const SECRET: &str = "hook-secret";

struct Harness {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
    clock: Arc<AtomicI64>,
}

impl Harness {
    fn new(url: String, events: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        let clock = Arc::new(AtomicI64::new(1000));
        let ticks = clock.clone();
        let webhooks = WebhookRuntime::new(vec![WebhookTargetConfig {
            index: "local".to_owned(),
            name: "ci".to_owned(),
            url,
            secret: SECRET.to_owned(),
            events: events.iter().map(|event| (*event).to_owned()).collect(),
        }])
        .unwrap();
        let state = Arc::new(AppState::with_clock_and_webhooks(
            meta,
            blobs,
            60,
            vec![Index {
                name: "local".to_owned(),
                route: "local".to_owned(),
                kind: IndexKind::Hosted {
                    upload_token: Some("s3cret".to_owned()),
                    volatile: true,
                },
                policy: Policy::default(),
            }],
            Arc::new(move || ticks.load(Ordering::Relaxed)),
            webhooks,
        ));
        Self {
            _dir: dir,
            state,
            clock,
        }
    }
}

#[derive(Debug, Clone)]
struct CapturedRequest {
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone)]
struct WebhookSink {
    url: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl WebhookSink {
    async fn start(statuses: Vec<u16>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(accept_webhooks(
            listener,
            Arc::new(Mutex::new(VecDeque::from(statuses))),
            requests.clone(),
        ));
        Self { url, requests }
    }

    async fn wait_for_requests(&self, count: usize) -> Vec<CapturedRequest> {
        for _ in 0..200 {
            let requests = self.requests.lock().expect("request lock").clone();
            if requests.len() >= count {
                return requests;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {count} webhook requests");
    }

    fn request_count(&self) -> usize {
        self.requests.lock().expect("request lock").len()
    }
}

async fn accept_webhooks(
    listener: TcpListener,
    statuses: Arc<Mutex<VecDeque<u16>>>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
) {
    while let Ok((mut socket, _)) = listener.accept().await {
        let request = read_request(&mut socket).await;
        requests.lock().expect("request lock").push(request);
        let status = statuses.lock().expect("status lock").pop_front().unwrap_or(200);
        let response = format!(
            "HTTP/1.1 {status} {}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            reason(status)
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut bytes = Vec::new();
    let mut buf = [0_u8; 2048];
    loop {
        let read = socket.read(&mut buf).await.unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..read]);
        let Some(header_end) = header_end(&bytes) else {
            continue;
        };
        let body_start = header_end + 4;
        let content_length = content_length(&bytes[..header_end]);
        if bytes.len() >= body_start + content_length {
            let headers = headers(&bytes[..header_end]);
            let body = bytes[body_start..body_start + content_length].to_vec();
            return CapturedRequest { headers, body };
        }
    }
    panic!("connection closed before request completed");
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0)
}

fn headers(bytes: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect()
}

const fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

async fn wait_for_delivery(state: &AppState, status: WebhookDeliveryStatus, attempts: u16) -> WebhookDeliveryRecord {
    for _ in 0..200 {
        let deliveries = state.meta.list_webhook_deliveries().unwrap();
        if let Some(delivery) = deliveries
            .into_iter()
            .find(|delivery| delivery.status == status && delivery.attempts == attempts)
        {
            return delivery;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected delivery with status {status:?} and {attempts} attempts");
}

async fn wait_for_delivery_id(state: &AppState, id: &str, status: WebhookDeliveryStatus) -> WebhookDeliveryRecord {
    for _ in 0..200 {
        let deliveries = state.meta.list_webhook_deliveries().unwrap();
        if let Some(delivery) = deliveries
            .into_iter()
            .find(|delivery| delivery.id == id && delivery.status == status)
        {
            return delivery;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected delivery {id} with status {status:?}");
}

#[tokio::test]
async fn test_upload_webhook_is_signed_and_skips_duplicate_upload() {
    let sink = WebhookSink::start(vec![200]).await;
    let h = Harness::new(sink.url.clone(), &["upload"]);
    let wheel = fixture_wheel();

    assert_eq!(upload_velodexpkg(&h.state, "/local/", &wheel).await, StatusCode::OK);

    let request = sink.wait_for_requests(1).await.remove(0);
    let delivery = wait_for_delivery(&h.state, WebhookDeliveryStatus::Delivered, 1).await;
    assert_eq!(delivery.event, "upload");
    assert_signed(&request, &delivery.id, "upload", 1000);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
        json!({
            "event": "upload",
            "created_at": 1000,
            "index": "local",
            "route": "local",
            "local_index": "local",
            "project": "velodexpkg",
            "version": "1.0",
            "file": {
                "filename": "velodexpkg-1.0-py3-none-any.whl",
                "sha256": Digest::of(&wheel).as_str(),
            },
            "count": 1,
            "actor": "__token__",
        })
    );

    assert_eq!(upload_velodexpkg(&h.state, "/local/", &wheel).await, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(sink.request_count(), 1);
    assert_eq!(h.state.meta.list_webhook_deliveries().unwrap().len(), 1);
}

#[tokio::test]
async fn test_upload_webhook_ignores_invalid_request_id() {
    let sink = WebhookSink::start(vec![200]).await;
    let h = Harness::new(sink.url.clone(), &["upload"]);

    assert_eq!(
        upload_with_request_id(&h.state, &fixture_wheel(), b"\xff").await,
        StatusCode::OK
    );

    let request = sink.wait_for_requests(1).await.remove(0);
    let body = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();
    assert_eq!(body.get("request_id"), None);
}

#[tokio::test]
async fn test_webhook_worker_wakes_after_idle() {
    let sink = WebhookSink::start(vec![200, 200]).await;
    let h = Harness::new(sink.url.clone(), &["upload"]);

    assert_eq!(
        upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await,
        StatusCode::OK
    );
    sink.wait_for_requests(1).await;
    wait_for_delivery(&h.state, WebhookDeliveryStatus::Delivered, 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let id = h
        .state
        .meta
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "local",
            target: "ci",
            event: "upload",
            payload: r#"{"event":"upload"}"#,
            created_at_unix: 1000,
        })
        .unwrap();
    webhook::kick(h.state.clone());

    sink.wait_for_requests(2).await;
    let delivered = wait_for_delivery_id(&h.state, &id, WebhookDeliveryStatus::Delivered).await;
    assert_eq!(delivered.attempts, 1);
    assert_eq!(delivered.response_status, Some(200));
}

#[tokio::test]
async fn test_webhook_delivery_retries_failed_request() {
    let sink = WebhookSink::start(vec![500, 204]).await;
    let h = Harness::new(sink.url.clone(), &["upload"]);

    assert_eq!(
        upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await,
        StatusCode::OK
    );

    sink.wait_for_requests(1).await;
    let pending = wait_for_delivery(&h.state, WebhookDeliveryStatus::Pending, 1).await;
    assert_eq!(pending.response_status, Some(500));
    assert_eq!(pending.next_attempt_at_unix, Some(1005));

    h.clock.store(1005, Ordering::Relaxed);
    webhook::kick(h.state.clone());
    let requests = sink.wait_for_requests(2).await;
    let delivered = wait_for_delivery(&h.state, WebhookDeliveryStatus::Delivered, 2).await;

    assert_eq!(delivered.id, pending.id);
    assert_eq!(delivered.response_status, Some(204));
    assert_signed(&requests[1], &delivered.id, "upload", 1005);
}

#[tokio::test]
async fn test_webhook_delivery_marks_terminal_failure() {
    let sink = WebhookSink::start(vec![500; 5]).await;
    let h = Harness::new(sink.url.clone(), &["upload"]);

    assert_eq!(
        upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await,
        StatusCode::OK
    );

    sink.wait_for_requests(1).await;
    for count in 2..=5 {
        let pending = wait_for_delivery(&h.state, WebhookDeliveryStatus::Pending, count - 1).await;
        h.clock.store(
            pending.next_attempt_at_unix.expect("scheduled retry"),
            Ordering::Relaxed,
        );
        webhook::kick(h.state.clone());
        sink.wait_for_requests(count as usize).await;
    }

    let failed = wait_for_delivery(&h.state, WebhookDeliveryStatus::Failed, 5).await;
    assert_eq!(failed.response_status, Some(500));
    assert_eq!(failed.next_attempt_at_unix, None);
    assert_eq!(failed.last_error.as_deref(), Some("http status 500"));
}

#[tokio::test]
async fn test_webhook_delivery_records_request_error() {
    let h = Harness::new(closed_url().await, &["upload"]);

    assert_eq!(
        upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await,
        StatusCode::OK
    );

    let pending = wait_for_delivery(&h.state, WebhookDeliveryStatus::Pending, 1).await;
    assert_eq!(pending.response_status, None);
    assert_eq!(pending.next_attempt_at_unix, Some(1005));
    assert!(pending.last_error.as_deref().is_some_and(|err| !err.contains("/hook")));
}

#[tokio::test]
async fn test_webhook_delivery_records_removed_target() {
    let sink = WebhookSink::start(Vec::new()).await;
    let h = Harness::new(sink.url.clone(), &["upload"]);
    let id = h
        .state
        .meta
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "local",
            target: "removed",
            event: "upload",
            payload: r#"{"event":"upload"}"#,
            created_at_unix: 1000,
        })
        .unwrap();

    webhook::kick(h.state.clone());

    let pending = wait_for_delivery(&h.state, WebhookDeliveryStatus::Pending, 1).await;
    assert_eq!(pending.id, id);
    assert_eq!(pending.response_status, None);
    assert_eq!(pending.next_attempt_at_unix, Some(1005));
    assert_eq!(pending.last_error.as_deref(), Some("webhook target is not configured"));
    assert_eq!(sink.request_count(), 0);
}

#[tokio::test]
async fn test_delete_webhook_emits_index_change() {
    let sink = WebhookSink::start(vec![200]).await;
    let h = Harness::new(sink.url.clone(), &["delete"]);

    assert_eq!(
        upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await,
        StatusCode::OK
    );
    assert_eq!(sink.request_count(), 0);
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/", Some(&upload_auth())).await,
        StatusCode::OK
    );

    let request = sink.wait_for_requests(1).await.remove(0);
    let delivery = wait_for_delivery(&h.state, WebhookDeliveryStatus::Delivered, 1).await;
    assert_eq!(delivery.event, "delete");
    assert_signed(&request, &delivery.id, "delete", 1000);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
        json!({
            "event": "delete",
            "created_at": 1000,
            "index": "local",
            "route": "local",
            "local_index": "local",
            "project": "velodexpkg",
            "count": 1,
            "actor": "__token__",
        })
    );
}

async fn closed_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    drop(listener);
    url
}

async fn upload_with_request_id(state: &Arc<AppState>, wheel: &[u8], request_id: &[u8]) -> StatusCode {
    let fields = [
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("pyversion", "py3"),
        ("filetype", "bdist_wheel"),
        ("requires_python", ">=3.8"),
    ];
    let (content_type, body) = multipart_body(&fields, Some(("velodexpkg-1.0-py3-none-any.whl", wheel)));
    let mut request = axum::http::Request::builder()
        .uri("/local/")
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

fn assert_signed(request: &CapturedRequest, delivery: &str, event: &str, timestamp: i64) {
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(request.headers["x-velodex-event"], event);
    assert_eq!(request.headers["x-velodex-delivery"], delivery);
    assert_eq!(request.headers["x-velodex-timestamp"], timestamp.to_string());
    assert_eq!(
        request.headers["x-velodex-signature"],
        webhook::signature(SECRET, timestamp, delivery, &request.body)
    );
}
