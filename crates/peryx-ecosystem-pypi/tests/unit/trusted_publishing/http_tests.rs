use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use peryx_driver::state::AppState;
use peryx_identity::OidcVerificationError;
use tower::ServiceExt as _;

use super::*;

#[derive(Debug)]
struct StubExchange(ExchangeResult);

#[derive(Debug, Clone, Copy)]
enum ExchangeResult {
    Success,
    InvalidIdentity,
    IssuerUnavailable,
}

impl StubExchange {
    const fn new(result: ExchangeResult) -> Self {
        Self(result)
    }
}

#[async_trait]
impl IdentityExchange for StubExchange {
    fn audience(&self) -> &'static str {
        "packages.example"
    }

    async fn exchange(&self, _token: &str, _now: i64) -> Result<ExchangedToken, ExchangeError> {
        match self.0 {
            ExchangeResult::Success => Ok(ExchangedToken {
                token: "internal.identity.secret".to_owned(),
                token_id: "token-42".to_owned(),
                publisher_id: "release".to_owned(),
                repository: "private".to_owned(),
                expires_at: 123,
            }),
            ExchangeResult::InvalidIdentity => Err(ExchangeError::Verification(OidcVerificationError::InvalidIdentity)),
            ExchangeResult::IssuerUnavailable => {
                Err(ExchangeError::Verification(OidcVerificationError::IssuerUnavailable))
            }
        }
    }
}

thread_local! {
    static ACTIVE_CAPTURE: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

fn install_log_subscriber() -> tracing::dispatcher::DefaultGuard {
    tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(ThreadLocalWriter)
            .finish(),
    )
}

#[derive(Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    fn install(&self) -> CaptureGuard {
        let subscriber = install_log_subscriber();
        ACTIVE_CAPTURE.with(|slot| *slot.borrow_mut() = Some(self.0.clone()));
        CaptureGuard {
            _subscriber: subscriber,
        }
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

struct CaptureGuard {
    _subscriber: tracing::dispatcher::DefaultGuard,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        ACTIVE_CAPTURE.with(|slot| *slot.borrow_mut() = None);
    }
}

struct ThreadLocalWriter;

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for ThreadLocalWriter {
    type Writer = LogWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        LogWriter(ACTIVE_CAPTURE.with(|slot| slot.borrow().clone()))
    }
}

struct LogWriter(Option<Arc<Mutex<Vec<u8>>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(bytes) = &self.0 {
            bytes.lock().unwrap().extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn app_state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    );
    (dir, state)
}

fn state(enabled: bool) -> (tempfile::TempDir, Arc<AppState>) {
    let (dir, mut state) = app_state();
    if enabled {
        state.register_http_routes(Arc::new(TrustedPublishingRoutes::new(Arc::new(StubExchange::new(
            ExchangeResult::InvalidIdentity,
        )))));
    }
    (dir, Arc::new(state))
}

fn state_with_exchange(exchange: impl IdentityExchange + 'static) -> (tempfile::TempDir, Arc<AppState>) {
    assert_eq!(exchange.audience(), "packages.example");
    let (dir, mut state) = app_state();
    state.register_http_routes(Arc::new(TrustedPublishingRoutes::new(Arc::new(exchange))));
    (dir, Arc::new(state))
}

fn successful_exchange() -> StubExchange {
    StubExchange::new(ExchangeResult::Success)
}

#[test]
fn test_log_writer_accepts_flush() {
    assert!(std::io::Write::flush(&mut LogWriter(None)).is_ok());
}

async fn exchange_request(state: Arc<AppState>, identity: &str, request_id: Option<&str>) -> axum::response::Response {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/_/oidc/mint-token")
        .header("content-type", "application/json");
    if let Some(request_id) = request_id {
        request = request.header("x-request-id", request_id);
    }
    peryx_http::router(state)
        .oneshot(
            request
                .body(Body::from(serde_json::json!({"token": identity}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn request(state: Arc<AppState>, method: Method, uri: &str, body: Body) -> (StatusCode, String) {
    let response = peryx_http::router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn test_oidc_routes_are_absent_when_unconfigured() {
    let (_dir, state) = state(false);
    assert_eq!(
        request(state.clone(), Method::GET, "/_/oidc/audience", Body::empty())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            state,
            Method::POST,
            "/_/oidc/mint-token",
            Body::from(r#"{"token":"x"}"#),
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_oidc_audience_uses_the_configured_value() {
    let (_dir, state) = state(true);
    let (status, body) = request(state, Method::GET, "/_/oidc/audience", Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"audience": "packages.example"})
    );
}

#[tokio::test]
async fn test_oidc_exchange_rejects_a_malformed_identity_without_echoing_it() {
    let (_dir, state) = state(true);
    let secret = "header.payload.secret-material";
    let (status, body) = request(
        state,
        Method::POST,
        "/_/oidc/mint-token",
        Body::from(serde_json::json!({"token": secret}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"message": "identity token rejected"})
    );
    assert!(!body.contains(secret));
}

#[tokio::test]
async fn test_oidc_exchange_returns_the_minted_token_without_cache() {
    let (_dir, state) = state_with_exchange(successful_exchange());
    let response = exchange_request(state, "external.identity.secret", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        (
            response.headers()[header::CACHE_CONTROL].to_str().unwrap(),
            response.headers()[header::PRAGMA].to_str().unwrap(),
        ),
        ("no-store", "no-cache")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"token": "internal.identity.secret", "expires": 123})
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_oidc_exchange_logs_stable_ids_without_credentials() {
    let external = "external.identity.secret";
    let minted = "internal.identity.secret";
    let (_dir, state) = state_with_exchange(successful_exchange());
    let logs = LogCapture::default();
    let guard = logs.install();
    let _response = exchange_request(state, external, Some("request-42")).await;

    drop(guard);
    let logs = logs.text();
    assert!(!logs.contains(external));
    assert!(!logs.contains(minted));
    let event = logs
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["fields"]["action"] == "token_mint")
        .unwrap();
    assert_eq!(
        event["fields"],
        serde_json::json!({
            "message": "index security event",
            "security_event": true,
            "event": "index_action",
            "action": "token_mint",
            "result": "success",
            "actor": "release",
            "publisher_id": "release",
            "token_id": "token-42",
            "index": "private",
            "source_index": "",
            "hosted_index": "",
            "project": "",
            "version": "",
            "filename": "",
            "digest": "",
            "count": 0,
            "changed": false,
            "reason": "",
            "request_id": "request-42",
            "user_agent": ""
        })
    );
}

#[tokio::test]
async fn test_oidc_exchange_reports_an_unavailable_issuer_without_echoing_the_identity() {
    let identity = "external.identity.secret";
    let (_dir, state) = state_with_exchange(StubExchange::new(ExchangeResult::IssuerUnavailable));
    let response = exchange_request(state, identity, None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"message": "identity provider unavailable"})
    );
    assert!(!String::from_utf8(body.to_vec()).unwrap().contains(identity));
}

#[tokio::test]
async fn test_oidc_exchange_body_is_bounded() {
    let (_dir, state) = state(true);
    let body = serde_json::json!({"token": "x".repeat(41 * 1024)}).to_string();
    assert_eq!(
        request(state, Method::POST, "/_/oidc/mint-token", Body::from(body))
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}
