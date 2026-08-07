use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::get;
use axum::{Json as AxumJson, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::{
    AppState, CommandOutcome, CommandReceipt, ControlCommand, ControlError, ControlPlane, MembershipControl,
};
use peryx_driver::users::UserService;
use peryx_ha_distributed::{ChangePage, PROTOCOL_VERSION};
use peryx_identity::{GrantScope, PasswordPolicy, Role};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt as _;

use crate::availability::{AvailabilityPosture, FrontierSource, RosterFrontierSource, TransferCoordinator, router};
use crate::config::{AvailabilityConfig, ReplicationConfig, SecretSource};

const ADMIN: &str = "Alice";
const OPERATOR: &str = "Olivia";
const PASSWORD: &str = "local password";

async fn app() -> (tempfile::TempDir, Arc<AppState>) {
    build_app(false, false).await
}

async fn build_app(break_identity_store: bool, break_serial: bool) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let authorization = AuthorizationService::new(meta.clone());
    for (name, role) in [(ADMIN, Role::Administrator), (OPERATOR, Role::Operator)] {
        let user = users.create(name).unwrap();
        users.set_password(&user.id, PASSWORD).await.unwrap();
        authorization.grant(&user.id, role, GrantScope::Server).unwrap();
    }
    drop(authorization);
    drop(users);
    drop(meta);
    if break_identity_store {
        let database = redb::Database::open(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .delete_table(redb::TableDefinition::<&str, &[u8]>::new("server_user_verifier"))
            .unwrap();
        transaction
            .open_table(redb::TableDefinition::<&str, u64>::new("server_user_verifier"))
            .unwrap();
        transaction.commit().unwrap();
    }
    if break_serial {
        let database = redb::Database::open(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .delete_table(redb::TableDefinition::<&str, u64>::new("serial"))
            .unwrap();
        transaction
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("serial"))
            .unwrap();
        transaction.commit().unwrap();
    }
    let meta = MetaStore::open_existing(path).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, Vec::new());
    state.users = UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, Arc::new(state))
}

fn dc_writer() -> AvailabilityPosture {
    AvailabilityPosture::from_config(&AvailabilityConfig::Dc(ReplicationConfig::Primary {
        source: "primary-a".to_owned(),
        token: SecretSource::Literal("secret".to_owned()),
    }))
    .expect("dc primary posture")
}

fn ha_replica() -> AvailabilityPosture {
    AvailabilityPosture::from_config(&AvailabilityConfig::Ha(ReplicationConfig::Replica {
        upstream: "https://primary.example/".to_owned(),
        token: SecretSource::Literal("secret".to_owned()),
        poll_interval: Duration::from_secs(1),
        page_size: NonZeroUsize::MIN,
    }))
    .expect("ha replica posture")
}

fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
}

/// A coordinator over an empty roster, for the routes that never drive a transfer.
fn coordinator() -> Arc<TransferCoordinator> {
    Arc::new(TransferCoordinator::new(Arc::new(RosterFrontierSource::new(
        Vec::new(),
        "token",
    ))))
}

async fn request(
    state: &Arc<AppState>,
    posture: AvailabilityPosture,
    path: &str,
    auth: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().uri(path);
    if let Some(value) = auth {
        builder = builder.header(header::AUTHORIZATION, value);
    }
    let response = router(state.clone(), posture, coordinator())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// An ownership group double reporting a fixed consensus snapshot, so the status resource's consensus
/// block is exercised without a live Raft node.
struct FixedGroup;

#[async_trait::async_trait]
impl peryx_driver::state::OwnershipAuthority for FixedGroup {
    async fn has_home(&self, _authority: &str) -> bool {
        false
    }

    async fn claim_home(
        &self,
        _authority: &str,
    ) -> Result<peryx_driver::state::HomeClaim, peryx_driver::state::OwnershipError> {
        Ok(peryx_driver::state::HomeClaim::AlreadyHomed)
    }

    fn cluster_status(&self) -> peryx_driver::state::ClusterStatus {
        peryx_driver::state::ClusterStatus {
            leader: Some("east".to_owned()),
            term: 2,
            voters: vec!["east".to_owned(), "west".to_owned()],
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        0
    }

    async fn admit_epoch(&self, _authority: &str, _presented: u64) -> bool {
        true
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<peryx_driver::state::TransferOutcome>, peryx_driver::state::OwnershipError> {
        Ok(None)
    }
}

#[test]
fn test_posture_absent_for_single_node_none() {
    assert!(AvailabilityPosture::from_config(&AvailabilityConfig::None).is_none());
}

#[tokio::test]
async fn test_status_reports_the_consensus_group_when_one_runs() {
    let (_dir, state) = app().await;
    state.set_ownership_authority(Arc::new(FixedGroup));

    let (status, _, body) = request(
        &state,
        ha_replica(),
        "/availability/v1/status",
        Some(&basic(ADMIN, PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["consensus"]["leader"], "east");
    assert_eq!(body["consensus"]["term"], 2);
    assert_eq!(body["consensus"]["voters"], serde_json::json!(["east", "west"]));
}

#[tokio::test]
async fn test_status_reports_writer_posture_for_an_administrator() {
    let (_dir, state) = app().await;

    let (status, _, body) = request(
        &state,
        dc_writer(),
        "/availability/v1/status",
        Some(&basic(ADMIN, PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["protocol_version"], 2);
    assert_eq!(body["mode"], "dc");
    assert_eq!(body["role"], "writer");
    assert_eq!(body["read_only"], false);
}

#[tokio::test]
async fn test_status_is_never_cached() {
    let (_dir, state) = app().await;

    let (status, headers, _) = request(
        &state,
        dc_writer(),
        "/availability/v1/status",
        Some(&basic(ADMIN, PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "no-store",
        "an authenticated posture must not be stored by a shared cache",
    );
}

#[tokio::test]
async fn test_status_reports_replica_role_in_ha_mode() {
    let (_dir, state) = app().await;

    let (status, _, body) = request(
        &state,
        ha_replica(),
        "/availability/v1/status",
        Some(&basic(ADMIN, PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mode"], "ha");
    assert_eq!(body["role"], "replica");
}

#[rstest]
#[case::missing(None)]
#[case::not_basic(Some("Bearer some-token".to_owned()))]
#[case::malformed_basic(Some("Basic not+valid+base64+@@".to_owned()))]
#[case::wrong_password(Some(basic(ADMIN, "wrong password")))]
#[tokio::test]
async fn test_status_rejects_missing_or_invalid_credentials(#[case] auth: Option<String>) {
    let (_dir, state) = app().await;

    let (status, headers, _) = request(&state, dc_writer(), "/availability/v1/status", auth.as_deref()).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers.contains_key(header::WWW_AUTHENTICATE));
}

#[tokio::test]
async fn test_status_forbids_a_non_administrator() {
    let (_dir, state) = app().await;

    let (status, _, _) = request(
        &state,
        dc_writer(),
        "/availability/v1/status",
        Some(&basic(OPERATOR, PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_credential_rotation_rejects_the_old_password() {
    let (_dir, state) = app().await;
    let id = state.users.authenticate(ADMIN, PASSWORD).await.unwrap().unwrap();
    state.users.set_password(&id, "rotated password").await.unwrap();

    let (old, _, _) = request(
        &state,
        dc_writer(),
        "/availability/v1/status",
        Some(&basic(ADMIN, PASSWORD)),
    )
    .await;
    let (new, _, _) = request(
        &state,
        dc_writer(),
        "/availability/v1/status",
        Some(&basic(ADMIN, "rotated password")),
    )
    .await;

    assert_eq!(old, StatusCode::UNAUTHORIZED);
    assert_eq!(new, StatusCode::OK);
}

#[tokio::test]
async fn test_revoking_administration_forbids_a_prior_administrator() {
    let (_dir, state) = app().await;
    let id = state.users.authenticate(ADMIN, PASSWORD).await.unwrap().unwrap();
    state
        .authorization
        .revoke(&id, Role::Administrator, &GrantScope::Server)
        .unwrap();

    let (status, _, _) = request(
        &state,
        dc_writer(),
        "/availability/v1/status",
        Some(&basic(ADMIN, PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_identity_store_failure_is_service_unavailable() {
    let (_dir, state) = build_app(true, false).await;

    let (status, _, _) = request(
        &state,
        dc_writer(),
        "/availability/v1/status",
        Some(&basic(ADMIN, PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_unknown_path_is_not_found_without_authenticating() {
    let (_dir, state) = app().await;

    let (status, _, _) = request(&state, dc_writer(), "/availability/v1/unknown", None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_listener_serves_then_drains_on_shutdown() {
    let (_dir, state) = app().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let signal = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state, dc_writer(), coordinator()).into_make_service())
            .with_graceful_shutdown(async move { signal.cancelled().await })
            .await
            .unwrap();
    });

    let client = reqwest::Client::new();
    let url = format!("http://{address}/availability/v1/status");
    let serving = client
        .get(&url)
        .header(header::AUTHORIZATION, basic(ADMIN, PASSWORD))
        .send()
        .await
        .unwrap();
    assert_eq!(serving.status(), StatusCode::OK);

    shutdown.cancel();
    server.await.unwrap();
    assert!(client.get(&url).send().await.is_err());
}

/// A control double returning a fixed result and counting its submissions, so a test can drive the
/// command surface without a live Raft node and prove that a replay never resubmits.
struct FakeControl {
    result: Result<CommandReceipt, ControlError>,
    calls: std::sync::Mutex<usize>,
}

#[async_trait::async_trait]
impl MembershipControl for FakeControl {
    async fn submit(&self, _command: ControlCommand) -> Result<CommandReceipt, ControlError> {
        *self.calls.lock().unwrap() += 1;
        self.result.clone()
    }
}

fn with_control(state: &Arc<AppState>, result: Result<CommandReceipt, ControlError>) -> Arc<FakeControl> {
    let control = Arc::new(FakeControl {
        result,
        calls: std::sync::Mutex::new(0),
    });
    state.set_control_plane(Arc::new(ControlPlane::new(control.clone(), Arc::new(|| 0))));
    control
}

fn transfer_body() -> Value {
    serde_json::json!({ "type": "transfer_authority", "authority": "proj", "new_home": "west" })
}

fn committed(index: u64) -> CommandReceipt {
    CommandReceipt {
        term: 5,
        index,
        outcome: CommandOutcome::Committed,
        old_voters: Vec::new(),
        new_voters: Vec::new(),
    }
}

async fn post_command(
    state: &Arc<AppState>,
    auth: Option<&str>,
    key: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/availability/v1/commands")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = auth {
        builder = builder.header(header::AUTHORIZATION, value);
    }
    if let Some(value) = key {
        builder = builder.header("idempotency-key", value);
    }
    let response = router(state.clone(), dc_writer(), coordinator())
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn test_a_command_commits_and_returns_its_receipt() {
    let (_dir, state) = app().await;
    with_control(&state, Ok(committed(9)));

    let (status, body) = post_command(&state, Some(&basic(ADMIN, PASSWORD)), None, transfer_body()).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["term"], 5);
    assert_eq!(body["index"], 9);
    assert_eq!(body["outcome"], "committed");
}

#[tokio::test]
async fn test_a_command_forbids_a_non_writer() {
    let (_dir, state) = app().await;
    with_control(&state, Ok(committed(1)));

    // The operator authenticates but holds neither administration scope, so the write command is refused.
    let (status, _) = post_command(&state, Some(&basic(OPERATOR, PASSWORD)), None, transfer_body()).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_a_command_rejects_a_missing_credential() {
    let (_dir, state) = app().await;
    with_control(&state, Ok(committed(1)));

    let (status, _) = post_command(&state, None, None, transfer_body()).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_a_command_without_a_consensus_group_is_unavailable() {
    let (_dir, state) = app().await;

    // No control plane is registered, so the node runs no group to command.
    let (status, _) = post_command(&state, Some(&basic(ADMIN, PASSWORD)), None, transfer_body()).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_a_repeated_idempotency_key_returns_one_committed_result() {
    let (_dir, state) = app().await;
    let control = with_control(&state, Ok(committed(9)));
    let auth = basic(ADMIN, PASSWORD);

    let (first, first_body) = post_command(&state, Some(&auth), Some("k1"), transfer_body()).await;
    let (second, second_body) = post_command(&state, Some(&auth), Some("k1"), transfer_body()).await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::OK));
    assert_eq!(first_body, second_body);
    assert_eq!(
        *control.calls.lock().unwrap(),
        1,
        "the replay never reached the consensus group"
    );
}

#[rstest]
#[case::not_leader(ControlError::NotLeader { leader: Some("east.internal:4460".to_owned()) }, StatusCode::SERVICE_UNAVAILABLE)]
#[case::unavailable(ControlError::Unavailable("log gone".to_owned()), StatusCode::SERVICE_UNAVAILABLE)]
#[case::invalid(ControlError::Invalid("same home".to_owned()), StatusCode::CONFLICT)]
#[case::key_reuse(ControlError::KeyReuse, StatusCode::CONFLICT)]
#[case::overloaded(ControlError::Overloaded, StatusCode::TOO_MANY_REQUESTS)]
#[tokio::test]
async fn test_a_command_failure_maps_to_its_status(#[case] error: ControlError, #[case] expected: StatusCode) {
    let (_dir, state) = app().await;
    with_control(&state, Err(error));

    let (status, _) = post_command(&state, Some(&basic(ADMIN, PASSWORD)), None, transfer_body()).await;

    assert_eq!(status, expected);
}

#[tokio::test]
async fn test_a_malformed_command_body_is_rejected() {
    let (_dir, state) = app().await;
    with_control(&state, Ok(committed(1)));

    let (status, _) = post_command(
        &state,
        Some(&basic(ADMIN, PASSWORD)),
        None,
        serde_json::json!({ "type": "no_such_command" }),
    )
    .await;

    assert!(
        status.is_client_error(),
        "an unknown command shape is a client error, got {status}"
    );
}

#[tokio::test]
async fn test_status_reports_command_metrics_when_a_plane_runs() {
    let (_dir, state) = app().await;
    with_control(&state, Ok(committed(9)));
    post_command(&state, Some(&basic(ADMIN, PASSWORD)), None, transfer_body()).await;

    let (status, _, body) = request(
        &state,
        dc_writer(),
        "/availability/v1/status",
        Some(&basic(ADMIN, PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["commands"]["completed"], 1);
    assert_eq!(body["commands"]["p50_ms"], 0);
}

/// A change-feed peer reporting a fixed applied serial, so a transfer can probe a real target frontier.
struct ChangeFeed {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl ChangeFeed {
    async fn start(current_serial: u64) -> Self {
        let router = Router::new().route(
            "/+replication/v1/changes",
            get(move || async move {
                AxumJson(ChangePage {
                    version: PROTOCOL_VERSION,
                    source: "west".to_owned(),
                    after: 0,
                    current_serial,
                    changes: Vec::new(),
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url: format!("http://{address}/"),
            task,
        }
    }
}

impl Drop for ChangeFeed {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A frontier reporting a fixed applied serial and signalling each probe, so a test can hold a transfer
/// active while it drives a second request through the listener.
struct GatedFrontier {
    probed: Arc<Notify>,
    applied: u64,
}

#[async_trait::async_trait]
impl FrontierSource for GatedFrontier {
    async fn applied_frontier(&self, _datacenter: &str) -> anyhow::Result<Option<u64>> {
        self.probed.notify_one();
        Ok(Some(self.applied))
    }
}

fn gated_frontier(applied: u64) -> (Arc<dyn FrontierSource>, Arc<Notify>) {
    let probed = Arc::new(Notify::new());
    (
        Arc::new(GatedFrontier {
            probed: probed.clone(),
            applied,
        }),
        probed,
    )
}

/// A coordinator that probes `frontier` without wall-clock delay, keeping the concurrency tests fast.
fn coordinator_over(frontier: Arc<dyn FrontierSource>) -> Arc<TransferCoordinator> {
    Arc::new(TransferCoordinator::with_schedule(frontier, Duration::ZERO, 3))
}

/// A coordinator resolving datacenter `west` to `url`'s change feed.
fn coordinator_for(url: &str) -> Arc<TransferCoordinator> {
    coordinator_over(Arc::new(RosterFrontierSource::new(
        vec![("west".to_owned(), url.to_owned())],
        "token",
    )))
}

fn transfer_request_body() -> Value {
    json!({ "authority": "proj", "source": "east", "target": "west", "reason": "drain east" })
}

async fn post_transfer(
    state: &Arc<AppState>,
    coordinator: Arc<TransferCoordinator>,
    auth: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    post_keyed_transfer(state, coordinator, auth, None, body).await
}

/// Post a transfer carrying an optional `Idempotency-Key`, so a replay across a retry collapses to the
/// one committed move the first request booked.
async fn post_keyed_transfer(
    state: &Arc<AppState>,
    coordinator: Arc<TransferCoordinator>,
    auth: Option<&str>,
    key: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/availability/v1/transfers")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = auth {
        builder = builder.header(header::AUTHORIZATION, value);
    }
    if let Some(value) = key {
        builder = builder.header("idempotency-key", value);
    }
    let response = router(state.clone(), dc_writer(), coordinator)
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn delete_transfer(
    state: &Arc<AppState>,
    coordinator: Arc<TransferCoordinator>,
    auth: Option<&str>,
    authority: &str,
) -> StatusCode {
    let mut builder = Request::builder()
        .method("DELETE")
        .uri(format!("/availability/v1/transfers/{authority}"));
    if let Some(value) = auth {
        builder = builder.header(header::AUTHORIZATION, value);
    }
    router(state.clone(), dc_writer(), coordinator)
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

/// A node running a consensus group and control plane, so a transfer can commit and seal an audit.
fn with_consensus(state: &Arc<AppState>, result: Result<CommandReceipt, ControlError>) {
    state.set_ownership_authority(Arc::new(FixedGroup));
    with_control(state, result);
}

#[tokio::test]
async fn test_a_transfer_drives_through_the_change_feed_to_a_sealed_audit() {
    let (_dir, state) = app().await;
    with_consensus(&state, Ok(committed(9)));
    let feed = ChangeFeed::start(5).await;

    let (status, body) = post_transfer(
        &state,
        coordinator_for(&feed.url),
        Some(&basic(ADMIN, PASSWORD)),
        transfer_request_body(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["target"], "west");
    assert_eq!(body["reason"], "drain east");
    assert_eq!(body["commit_index"], 9);
    assert_eq!(body["epoch"], 0);
    // The committed move sealed a durable audit under the authority.
    assert_eq!(state.meta.transfer_audits("proj").unwrap().len(), 1);
}

#[tokio::test]
async fn test_a_repeated_transfer_idempotency_key_commits_one_move() {
    let (_dir, state) = app().await;
    state.set_ownership_authority(Arc::new(FixedGroup));
    let control = with_control(&state, Ok(committed(9)));
    let feed = ChangeFeed::start(5).await;
    let coordinator = coordinator_for(&feed.url);
    let auth = basic(ADMIN, PASSWORD);

    let (first, _) = post_keyed_transfer(
        &state,
        coordinator.clone(),
        Some(&auth),
        Some("k1"),
        transfer_request_body(),
    )
    .await;
    let (second, _) = post_keyed_transfer(&state, coordinator, Some(&auth), Some("k1"), transfer_request_body()).await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::OK));
    assert_eq!(
        *control.calls.lock().unwrap(),
        1,
        "the replay never reached the consensus group"
    );
}

#[tokio::test]
async fn test_a_transfer_forbids_a_non_administrator() {
    let (_dir, state) = app().await;
    let feed = ChangeFeed::start(5).await;

    let (status, _) = post_transfer(
        &state,
        coordinator_for(&feed.url),
        Some(&basic(OPERATOR, PASSWORD)),
        transfer_request_body(),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_a_transfer_without_a_consensus_group_is_unavailable() {
    let (_dir, state) = app().await;
    let feed = ChangeFeed::start(5).await;

    // Neither an ownership group nor a control plane is registered, so the node has nothing to commit through.
    let (status, _) = post_transfer(
        &state,
        coordinator_for(&feed.url),
        Some(&basic(ADMIN, PASSWORD)),
        transfer_request_body(),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_a_transfer_to_an_unreadable_target_is_unavailable() {
    let (_dir, state) = app().await;
    with_consensus(&state, Ok(committed(9)));

    // The target resolves to an unusable address, so the frontier probe fails and the move cannot proceed.
    let coordinator = coordinator_over(Arc::new(RosterFrontierSource::new(
        vec![("west".to_owned(), "not a url".to_owned())],
        "token",
    )));
    let (status, _) = post_transfer(
        &state,
        coordinator,
        Some(&basic(ADMIN, PASSWORD)),
        transfer_request_body(),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_a_transfer_that_never_reaches_the_barrier_times_out() {
    let (_dir, state) = app().await;
    with_consensus(&state, Ok(committed(9)));
    // Raise this node's serial above the target's, so the target never reaches the barrier.
    for _ in 0..10 {
        state.meta.next_serial().unwrap();
    }
    let feed = ChangeFeed::start(5).await;

    let (status, _) = post_transfer(
        &state,
        coordinator_for(&feed.url),
        Some(&basic(ADMIN, PASSWORD)),
        transfer_request_body(),
    )
    .await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(state.meta.transfer_audits("proj").unwrap().is_empty());
}

#[tokio::test]
async fn test_a_transfer_with_an_unreadable_barrier_is_unavailable() {
    let (_dir, state) = build_app(false, true).await;
    with_consensus(&state, Ok(committed(9)));
    let feed = ChangeFeed::start(5).await;

    // The node runs a consensus group, but its serial cannot be read, so the barrier is unavailable.
    let (status, _) = post_transfer(
        &state,
        coordinator_for(&feed.url),
        Some(&basic(ADMIN, PASSWORD)),
        transfer_request_body(),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_a_transfer_whose_commit_is_refused_maps_the_control_error() {
    let (_dir, state) = app().await;
    with_consensus(&state, Err(ControlError::NotLeader { leader: None }));
    let feed = ChangeFeed::start(5).await;

    let (status, _) = post_transfer(
        &state,
        coordinator_for(&feed.url),
        Some(&basic(ADMIN, PASSWORD)),
        transfer_request_body(),
    )
    .await;

    // The target caught up but the commit was refused, so the move resolves as the command would.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(state.meta.transfer_audits("proj").unwrap().is_empty());
}

#[tokio::test]
async fn test_cancelling_an_unregistered_transfer_is_not_found() {
    let (_dir, state) = app().await;

    let status = delete_transfer(&state, coordinator(), Some(&basic(ADMIN, PASSWORD)), "ghost").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_cancelling_a_transfer_forbids_a_non_administrator() {
    let (_dir, state) = app().await;

    let status = delete_transfer(&state, coordinator(), Some(&basic(OPERATOR, PASSWORD)), "proj").await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_cancelling_a_committed_transfer_is_refused() {
    let (_dir, state) = app().await;
    with_consensus(&state, Ok(committed(9)));
    let feed = ChangeFeed::start(5).await;
    let coordinator = coordinator_for(&feed.url);
    let (ok, _) = post_transfer(
        &state,
        coordinator.clone(),
        Some(&basic(ADMIN, PASSWORD)),
        transfer_request_body(),
    )
    .await;
    assert_eq!(ok, StatusCode::OK);

    // The committed transfer stays registered, so a later cancel is refused rather than lost.
    let status = delete_transfer(&state, coordinator, Some(&basic(ADMIN, PASSWORD)), "proj").await;

    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test(start_paused = true)]
async fn test_a_transfer_for_a_running_authority_conflicts() {
    let (_dir, state) = app().await;
    with_consensus(&state, Ok(committed(9)));
    for _ in 0..10 {
        state.meta.next_serial().unwrap();
    }
    let (frontier, probed) = gated_frontier(5);
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        frontier,
        Duration::from_secs(30),
        10,
    ));
    let running = tokio::spawn({
        let state = state.clone();
        let coordinator = coordinator.clone();
        async move {
            post_transfer(
                &state,
                coordinator,
                Some(&basic(ADMIN, PASSWORD)),
                transfer_request_body(),
            )
            .await
        }
    });
    probed.notified().await;

    // The first transfer holds the authority, so a second for the same authority conflicts.
    let (status, _) = post_transfer(
        &state,
        coordinator,
        Some(&basic(ADMIN, PASSWORD)),
        transfer_request_body(),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    running.abort();
}

#[tokio::test(start_paused = true)]
async fn test_cancelling_an_active_transfer_resolves_its_run_as_a_conflict() {
    let (_dir, state) = app().await;
    with_consensus(&state, Ok(committed(9)));
    for _ in 0..10 {
        state.meta.next_serial().unwrap();
    }
    let (frontier, probed) = gated_frontier(5);
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        frontier,
        Duration::from_millis(10),
        5,
    ));
    let running = tokio::spawn({
        let state = state.clone();
        let coordinator = coordinator.clone();
        async move {
            post_transfer(
                &state,
                coordinator,
                Some(&basic(ADMIN, PASSWORD)),
                transfer_request_body(),
            )
            .await
        }
    });
    probed.notified().await;

    // Cancelling the waiting transfer abandons its plan.
    let cancelled = delete_transfer(&state, coordinator.clone(), Some(&basic(ADMIN, PASSWORD)), "proj").await;
    assert_eq!(cancelled, StatusCode::NO_CONTENT);

    // Its run then observes the cancelled plan and resolves as a conflict rather than a committed move.
    tokio::time::advance(Duration::from_millis(20)).await;
    let (status, _) = running.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(state.meta.transfer_audits("proj").unwrap().is_empty());
}
