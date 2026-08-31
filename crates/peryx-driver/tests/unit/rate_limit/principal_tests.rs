use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::{Router, middleware, routing::get};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::Ecosystem;
use peryx_identity::{PasswordPolicy, SESSION_COOKIE, ServerUser, SessionSealer};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use tower::ServiceExt as _;
use tracing_subscriber::layer::SubscriberExt as _;

use super::{ActorKey, RateLimitConfig, RateLimiter, RouteClass, RouteLimit};
use crate::serving::IndexCredentialDriver;
use crate::state::AppState;
use crate::users::UserService;
use crate::{RouteDescriptor, RouteMethod, RoutePosture, RouteRateLimit};

const PASSWORD: &str = "correct horse battery staple";
const SESSION_KEY: &[u8] = b"a-token-realm-signing-secret-here";
const NAT: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 7)), 44_100);

#[tokio::test]
async fn test_two_administrators_behind_one_address_hold_separate_buckets() {
    let (_dir, state) = app(RouteLimit::new(1, 60), 2);
    enroll(&state, "Ada").await;
    enroll(&state, "Grace").await;
    let router = router(state);

    assert_eq!(send(&router, credential("Ada")).await, StatusCode::NO_CONTENT);
    assert_eq!(send(&router, credential("Grace")).await, StatusCode::NO_CONTENT);
    assert_eq!(
        send(&router, credential("Ada")).await,
        StatusCode::TOO_MANY_REQUESTS,
        "each administrator spends only their own allowance"
    );
    assert_eq!(
        send_anonymous(&router).await,
        StatusCode::NO_CONTENT,
        "neither of them charged the address they share"
    );
}

#[tokio::test]
async fn test_an_invalid_credential_flood_stops_deriving_at_the_address_limit() {
    let (_dir, state) = app(RouteLimit::new(2, 60), 2);
    let router = router(state);
    let derivations = Derivations::default();
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(derivations.clone()));

    for _ in 0..2 {
        assert_eq!(send(&router, credential("nobody")).await, StatusCode::NO_CONTENT);
    }
    let spent = derivations.count();
    for _ in 0..3 {
        assert_eq!(send(&router, credential("nobody")).await, StatusCode::TOO_MANY_REQUESTS);
    }

    assert_eq!(spent, 2, "an unverifiable credential charges the address it came from");
    assert_eq!(
        derivations.count(),
        2,
        "the address limit refuses the flood before it derives anything"
    );
}

#[tokio::test]
async fn test_an_index_credential_keeps_the_address_bucket_without_a_derivation() {
    let (_dir, mut state) = app(RouteLimit::new(1, 60), 2);
    state.register_capabilities(|registrar| {
        registrar.register_index_credentials(Ecosystem::new("example"), Arc::new(TokenCredentials));
    });
    let router = router(state);
    let derivations = Derivations::default();
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(derivations.clone()));

    assert_eq!(send(&router, credential("__token__")).await, StatusCode::NO_CONTENT);
    assert_eq!(
        send(&router, credential("__token__")).await,
        StatusCode::TOO_MANY_REQUESTS
    );

    assert_eq!(
        derivations.count(),
        0,
        "an index credential resolves against an index ACL, not the user store"
    );
}

#[rstest::rstest]
#[case::bearer(HeaderValue::from_static("Bearer opaque"))]
#[case::unreadable(HeaderValue::from_bytes(b"Basic \xff").unwrap())]
#[tokio::test]
async fn test_a_credential_the_user_store_cannot_read_keeps_the_address_bucket(#[case] value: HeaderValue) {
    let (_dir, state) = app(RouteLimit::new(1, 60), 2);
    let router = router(state);

    assert_eq!(send(&router, value.clone()).await, StatusCode::NO_CONTENT);
    assert_eq!(send(&router, value).await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_a_route_that_checks_no_password_keeps_its_address_bucket() {
    let (_dir, state) = app(RouteLimit::new(1, 60), 2);
    enroll(&state, "Ada").await;
    enroll(&state, "Grace").await;
    let router = router(state);
    let request = |user: &str| {
        let mut request = management_request(Some(credential(user)));
        request.extensions_mut().insert(descriptor());
        request
    };

    assert_eq!(dispatch(&router, request("Ada")).await, StatusCode::NO_CONTENT);
    assert_eq!(dispatch(&router, request("Grace")).await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_an_unavailable_password_check_leaves_the_request_on_its_address_bucket() {
    let (_dir, state) = app(RouteLimit::new(1, 60), 0);
    enroll_without_password(&state, "Ada");
    let router = router(state);

    assert_eq!(send(&router, credential("Ada")).await, StatusCode::NO_CONTENT);
    assert_eq!(send(&router, credential("Ada")).await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_a_browser_session_buckets_by_the_account_it_holds() {
    let (_dir, mut state) = app(RouteLimit::new(1, 60), 2);
    state.set_session_sealer(SessionSealer::new(SESSION_KEY)).unwrap();
    let ada = enroll(&state, "Ada").await;
    let grace = enroll(&state, "Grace").await;
    let expires_at = (state.serving.clock)() + 3_600;
    let router = router(state);

    assert_eq!(send(&router, session(&ada, expires_at)).await, StatusCode::NO_CONTENT);
    assert_eq!(send(&router, session(&grace, expires_at)).await, StatusCode::NO_CONTENT);
    assert_eq!(
        send(&router, session(&ada, expires_at)).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn test_a_handler_reads_the_limiters_verdict_instead_of_deriving_again() {
    let (_dir, state) = app(RouteLimit::new(4, 60), 2);
    let ada = enroll(&state, "Ada").await;
    let state = Arc::new(state);
    let router = Router::new()
        .fallback(get(resolved_account))
        .with_state(Arc::clone(&state))
        .layer(middleware::from_fn_with_state(state, super::enforce));
    let derivations = Derivations::default();
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(derivations.clone()));

    assert_eq!(account(&router, credential("Ada")).await, ada.id.as_str());
    assert_eq!(account(&router, credential("nobody")).await, "");

    assert_eq!(
        derivations.count(),
        2,
        "each request checks its password once, in the limiter"
    );
}

#[tokio::test]
async fn test_a_handler_on_an_unclassified_route_checks_the_password_itself() {
    let (_dir, state) = app(RouteLimit::new(4, 60), 2);
    let ada = enroll(&state, "Ada").await;
    let state = Arc::new(state);
    let router = Router::new()
        .fallback(get(resolved_account))
        .with_state(Arc::clone(&state))
        .layer(middleware::from_fn_with_state(state, super::enforce));
    let mut request = management_request(Some(credential("Ada")));
    request.extensions_mut().insert(descriptor());

    let response = router.oneshot(request).await.unwrap();

    assert_eq!(body(response).await, ada.id.as_str());
}

#[test]
fn test_a_probe_reads_a_bucket_without_charging_it() {
    let limiter = RateLimiter::new(RateLimitConfig {
        admin: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });
    let actor = ActorKey::User(7);

    assert!(limiter.probe(RouteClass::Admin, actor).is_ok());
    assert!(limiter.probe(RouteClass::Admin, actor).is_ok());
    assert!(limiter.check(RouteClass::Admin, actor).is_ok());
    let retry_after = limiter.probe(RouteClass::Admin, actor).unwrap_err().retry_after;

    assert!((1..=60).contains(&retry_after));
    assert_eq!(
        limiter
            .counters()
            .into_iter()
            .find_map(|snapshot| (snapshot.class == "admin").then_some((snapshot.allowed, snapshot.denied))),
        Some((1, 1))
    );
}

#[test]
fn test_a_probe_readmits_an_exhausted_bucket_once_its_window_passes() {
    let millis = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let handle = Arc::clone(&millis);
    let limiter = RateLimiter::with_clock(
        RateLimitConfig {
            admin: RouteLimit::new(1, 1),
            ..RateLimitConfig::enabled_defaults()
        },
        Arc::new(move || Duration::from_millis(handle.load(Ordering::SeqCst))),
    );
    let actor = ActorKey::User(7);
    assert!(limiter.check(RouteClass::Admin, actor).is_ok());
    assert!(limiter.probe(RouteClass::Admin, actor).is_err());

    millis.store(1_001, Ordering::SeqCst);

    assert!(limiter.probe(RouteClass::Admin, actor).is_ok());
}

/// Answers with the account the request resolved to, checking its password only if the limiter has
/// not already left the verdict behind.
async fn resolved_account(State(state): State<Arc<AppState>>, headers: HeaderMap, extensions: Extensions) -> String {
    let value = headers[header::AUTHORIZATION].to_str().unwrap();
    let credentials = peryx_identity::parse_basic(value).unwrap();
    let verdict = state
        .serving
        .users
        .authenticate_request(&extensions, &credentials)
        .await
        .unwrap();
    verdict.map_or_else(String::new, |user| user.as_str().to_owned())
}

struct TokenCredentials;

impl IndexCredentialDriver for TokenCredentials {
    fn recognizes(&self, authorization: &str) -> bool {
        peryx_identity::parse_basic(authorization).is_some_and(|credentials| credentials.user == "__token__")
    }
}

/// Counts the password derivations a request admitted, which is where the cost the address limit
/// guards is actually spent.
#[derive(Clone, Default)]
struct Derivations {
    admitted: Arc<AtomicUsize>,
}

impl Derivations {
    fn count(&self) -> usize {
        self.admitted.load(Ordering::SeqCst)
    }
}

impl<Subscriber: tracing::Subscriber> tracing_subscriber::Layer<Subscriber> for Derivations {
    fn on_event(&self, event: &tracing::Event<'_>, _context: tracing_subscriber::layer::Context<'_, Subscriber>) {
        if event.metadata().target() == "peryx_driver::users::password_derivation_admitted" {
            self.admitted.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn app(admin: RouteLimit, checks: usize) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::with_rate_limits(
        meta.clone(),
        blobs,
        60,
        Vec::new(),
        RateLimitConfig {
            admin,
            ..RateLimitConfig::enabled_defaults()
        },
        [],
    );
    // A recommended policy would spend seconds per request on work this suite only has to count.
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), checks);
    (dir, state)
}

async fn enroll(state: &AppState, name: &str) -> ServerUser {
    let user = enroll_without_password(state, name);
    state.serving.users.set_password(&user.id, PASSWORD).await.unwrap();
    user
}

fn enroll_without_password(state: &AppState, name: &str) -> ServerUser {
    state.serving.users.create(name).unwrap()
}

fn router(state: AppState) -> Router {
    Router::new()
        .fallback(get(|| async { StatusCode::NO_CONTENT }))
        .layer(middleware::from_fn_with_state(Arc::new(state), super::enforce))
}

fn credential(user: &str) -> HeaderValue {
    let basic = format!("Basic {}", STANDARD.encode(format!("{user}:{PASSWORD}")));
    HeaderValue::from_str(&basic).unwrap()
}

fn session(user: &ServerUser, expires_at: i64) -> HeaderValue {
    let sealed = SessionSealer::new(SESSION_KEY).seal_session(user, expires_at);
    HeaderValue::from_str(&format!("{SESSION_COOKIE}={sealed}")).unwrap()
}

/// Every request arrives from one address, the way two administrators behind one NAT do.
fn management_request(authorization: Option<HeaderValue>) -> Request<Body> {
    let mut request = Request::get("/+tokens").body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(NAT));
    if let Some(authorization) = authorization {
        let name = if authorization
            .to_str()
            .is_ok_and(|value| value.starts_with(SESSION_COOKIE))
        {
            header::COOKIE
        } else {
            header::AUTHORIZATION
        };
        request.headers_mut().insert(name, authorization);
    }
    request
}

fn descriptor() -> RouteDescriptor {
    RouteDescriptor::new(
        RouteMethod::Get,
        "/+tokens",
        RoutePosture::Read,
        RouteRateLimit::Class(RouteClass::Admin),
    )
}

async fn send(router: &Router, authorization: HeaderValue) -> StatusCode {
    let mut request = management_request(Some(authorization));
    request
        .extensions_mut()
        .insert(descriptor().authenticating_local_user());
    dispatch(router, request).await
}

async fn send_anonymous(router: &Router) -> StatusCode {
    let mut request = management_request(None);
    request
        .extensions_mut()
        .insert(descriptor().authenticating_local_user());
    dispatch(router, request).await
}

async fn dispatch(router: &Router, request: Request<Body>) -> StatusCode {
    router.clone().oneshot(request).await.unwrap().status()
}

async fn account(router: &Router, authorization: HeaderValue) -> String {
    let mut request = management_request(Some(authorization));
    request
        .extensions_mut()
        .insert(descriptor().authenticating_local_user());
    body(router.clone().oneshot(request).await.unwrap()).await
}

async fn body(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}
