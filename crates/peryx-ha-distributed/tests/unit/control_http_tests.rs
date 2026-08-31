use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_driver::authz::{AuthorizationService, Decision};
use peryx_driver::state::{
    AppState, ClusterStatus, CommandOutcome, CommandReceipt, ControlCommand, ControlCommit, ControlError, HomeClaim,
    MembershipControl, OwnershipAuthority, OwnershipError, TransferOutcome,
};
use peryx_driver::users::UserService;
use peryx_identity::{GrantScope, PasswordPolicy, Resource, Role, Scope, parse_basic};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt as _;

use super::*;
use crate::ControlPlane;
use crate::{AuthorityEpoch, FrontierSource, RosterFrontierSource, TransferError};

const ADMIN: &str = "Alice";
const OPERATOR: &str = "Olivia";
const PASSWORD: &str = "local password";

struct TestAuthorizer {
    state: Arc<AppState>,
    actors: Mutex<HashMap<String, peryx_identity::UserId>>,
}

#[async_trait::async_trait]
impl peryx_ha::ControlAuthorizer for TestAuthorizer {
    async fn authenticate(
        &self,
        authorization: Option<&str>,
    ) -> Result<Option<peryx_ha::ControlActor>, peryx_ha::ControlAuthenticationError> {
        let Some(credentials) = authorization.and_then(parse_basic) else {
            return Ok(None);
        };
        let actor = self
            .state
            .serving
            .users
            .authenticate(&credentials.user, &credentials.password)
            .await
            .map_err(|_| peryx_ha::ControlAuthenticationError)?;
        Ok(actor.map(|actor| {
            let key = actor.as_str().to_owned();
            self.actors.lock().unwrap().insert(key.clone(), actor);
            peryx_ha::ControlActor::new(key)
        }))
    }

    fn allows(&self, actor: &peryx_ha::ControlActor, permission: peryx_ha::ControlPermission) -> bool {
        let Some(actor) = self.actors.lock().unwrap().get(actor.as_str()).cloned() else {
            return false;
        };
        let scope = match permission {
            peryx_ha::ControlPermission::Read => Scope::AdministrationRead,
            peryx_ha::ControlPermission::Write => Scope::AdministrationWrite,
        };
        self.state
            .serving
            .authorization
            .authorize_scoped(&actor, scope, &Resource::Operator)
            .decision()
            == Decision::Allow
    }
}

async fn app(break_identity: bool, break_serial: bool) -> (tempfile::TempDir, Arc<AppState>) {
    app_with_password_limit(break_identity, break_serial, 2).await
}

async fn app_with_password_limit(
    break_identity: bool,
    break_serial: bool,
    max_concurrent_checks: usize,
) -> (tempfile::TempDir, Arc<AppState>) {
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
    if break_identity {
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
    let mut state = AppState::new(meta.clone(), BlobStore::new(dir.path().join("blobs")), 60, Vec::new());
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), max_concurrent_checks);
    crate::support::install_distributed_services(&mut state);
    (dir, Arc::new(state))
}

#[tokio::test]
async fn unknown_control_actor_has_no_permission() {
    let (_dir, state) = app(false, false).await;
    let authorizer = TestAuthorizer {
        state,
        actors: Mutex::default(),
    };

    assert!(!peryx_ha::ControlAuthorizer::allows(
        &authorizer,
        &peryx_ha::ControlActor::new("unknown"),
        peryx_ha::ControlPermission::Read,
    ));
}

fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
}

fn posture(mode: DistributedMode, role: AvailabilityPostureRole) -> AvailabilityPosture {
    AvailabilityPosture::new(mode, role)
}

const RETAINED: usize = 4;

fn coordinator() -> Arc<TransferCoordinator> {
    Arc::new(TransferCoordinator::new(Arc::new(RosterFrontierSource::new(
        Vec::new(),
        "token",
    ))))
}

async fn send(
    state: &Arc<AppState>,
    coordinator: Arc<TransferCoordinator>,
    method: Method,
    path: &str,
    auth: Option<&str>,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    send_with(
        harness(state, ControlServices::default(), coordinator),
        method,
        path,
        auth,
        key,
        body,
    )
    .await
}

#[derive(Clone, Default)]
struct ControlServices {
    control: Option<Arc<dyn peryx_ha::ControlExecutor>>,
    ownership: Option<Arc<dyn OwnershipAuthority>>,
}

struct ControlHarness<'a> {
    state: &'a Arc<AppState>,
    services: ControlServices,
    coordinator: Arc<TransferCoordinator>,
}

fn harness(
    state: &Arc<AppState>,
    services: ControlServices,
    coordinator: Arc<TransferCoordinator>,
) -> ControlHarness<'_> {
    ControlHarness {
        state,
        services,
        coordinator,
    }
}

async fn send_with(
    harness: ControlHarness<'_>,
    method: Method,
    path: &str,
    auth: Option<&str>,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    if let Some(key) = key {
        builder = builder.header(&IDEMPOTENCY_KEY, key);
    }
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let body = body.map_or_else(Body::empty, |body| Body::from(body.to_string()));
    let ControlHarness {
        state,
        services,
        coordinator,
    } = harness;
    let response = availability_router(ControlHttpContext {
        authorizer: Arc::new(TestAuthorizer {
            state: state.clone(),
            actors: Mutex::new(HashMap::new()),
        }),
        posture: posture(DistributedMode::Dc, AvailabilityPostureRole::Writer),
        read_only: state.serving.read_only,
        meta: state.serving.meta.clone(),
        control: services.control,
        ownership: services.ownership,
        coordinator,
    })
    .oneshot(builder.body(body).unwrap())
    .await
    .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

struct FixedGroup;

#[async_trait::async_trait]
impl OwnershipAuthority for FixedGroup {
    async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
        Ok(HomeClaim {
            home: "east".to_owned(),
            epoch: 7,
        })
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: Some("east".to_owned()),
            term: 2,
            voters: vec!["east".to_owned(), "west".to_owned()],
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        7
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        presented == 7
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<TransferOutcome>, OwnershipError> {
        Ok(None)
    }
}

struct FixedControl {
    result: Result<CommandReceipt, ControlError>,
    calls: Mutex<usize>,
}

#[async_trait::async_trait]
impl MembershipControl for FixedControl {
    async fn submit(&self, _key: Option<&str>, _command: ControlCommand) -> Result<ControlCommit, ControlError> {
        *self.calls.lock().unwrap() += 1;
        self.result.clone().map(ControlCommit::committed)
    }
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

fn control_services(result: Result<CommandReceipt, ControlError>) -> (ControlServices, Arc<FixedControl>) {
    let control = Arc::new(FixedControl {
        result,
        calls: Mutex::new(0),
    });
    (
        ControlServices {
            control: Some(Arc::new(ControlPlane::new(control.clone(), Arc::new(|| 0)))),
            ownership: None,
        },
        control,
    )
}

fn command_body() -> Value {
    json!({"type": "transfer_authority", "authority": "proj", "new_home": "west"})
}

fn transfer_body() -> Value {
    json!({"authority": "proj", "source": "east", "target": "west", "reason": "drain east"})
}

struct FixedFrontier(Result<Option<u64>, &'static str>);

#[async_trait::async_trait]
impl FrontierSource for FixedFrontier {
    async fn applied_frontier(&self, _datacenter: &str) -> anyhow::Result<Option<u64>> {
        self.0.map_err(anyhow::Error::msg)
    }
}

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

fn consensus_services(result: Result<CommandReceipt, ControlError>) -> ControlServices {
    let (mut services, _) = control_services(result);
    services.ownership = Some(Arc::new(FixedGroup));
    services
}

#[test]
fn posture_reports_mode_and_role() {
    let writer = posture(DistributedMode::Dc, AvailabilityPostureRole::Writer);
    let replica = posture(DistributedMode::Ha, AvailabilityPostureRole::Replica);

    assert_eq!((writer.mode, writer.role), ("dc", "writer"));
    assert_eq!((replica.mode, replica.role), ("ha", "replica"));
}

#[tokio::test]
async fn fixed_group_implements_the_control_contract() {
    let group = FixedGroup;

    assert_eq!(
        group.claim_home("proj").await.unwrap(),
        HomeClaim {
            home: "east".to_owned(),
            epoch: 7,
        }
    );
    assert_eq!(group.committed_epoch("proj").await, 7);
    assert!(group.admit_epoch("proj", 7).await);
    assert_eq!(group.transfer_home("proj", "west").await.unwrap(), None);
}

#[tokio::test]
async fn status_authenticates_and_authorizes_administrators() {
    let (_dir, state) = app(false, false).await;
    let services = consensus_services(Ok(committed(9)));

    let (status, headers, body) = send_with(
        harness(&state, services, coordinator()),
        Method::GET,
        "/availability/v1/status",
        Some(&basic(ADMIN, PASSWORD)),
        None,
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        body,
        json!({
            "protocol_version": AVAILABILITY_PROTOCOL_VERSION,
            "mode": "dc",
            "role": "writer",
            "read_only": false,
            "consensus": {"leader": "east", "term": 2, "voters": ["east", "west"]},
            "commands": {"completed": 0, "p50_ms": 0, "p99_ms": 0},
        })
    );
}

#[tokio::test]
async fn status_rejects_invalid_credentials_and_scope() {
    for auth in [
        None,
        Some("Bearer token".to_owned()),
        Some("Basic not+valid+base64+@@".to_owned()),
        Some(basic(ADMIN, "wrong password")),
    ] {
        let (_dir, state) = app(false, false).await;
        let (status, headers, _) = send(
            &state,
            coordinator(),
            Method::GET,
            "/availability/v1/status",
            auth.as_deref(),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Basic realm=\"peryx-availability\"");
    }

    let (_dir, state) = app(false, false).await;
    assert_eq!(
        send(
            &state,
            coordinator(),
            Method::GET,
            "/availability/v1/status",
            Some(&basic(OPERATOR, PASSWORD)),
            None,
            None,
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn status_maps_identity_failure_and_skips_auth_for_unknown_routes() {
    let (_dir, state) = app(true, false).await;
    assert_eq!(
        send(
            &state,
            coordinator(),
            Method::GET,
            "/availability/v1/status",
            Some(&basic(ADMIN, PASSWORD)),
            None,
            None,
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        send(
            &state,
            coordinator(),
            Method::GET,
            "/availability/v1/unknown",
            None,
            None,
            None,
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn status_maps_password_overload_to_unavailable() {
    let (_dir, state) = app_with_password_limit(false, false, 0).await;
    let (status, headers, _) = send(
        &state,
        coordinator(),
        Method::GET,
        "/availability/v1/status",
        Some(&basic(ADMIN, PASSWORD)),
        None,
        None,
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn commands_commit_and_deduplicate() {
    let (_dir, state) = app(false, false).await;
    let (services, control) = control_services(Ok(committed(9)));
    let auth = basic(ADMIN, PASSWORD);
    let mut responses = Vec::new();
    for _ in 0..2 {
        responses.push(
            send_with(
                harness(&state, services.clone(), coordinator()),
                Method::POST,
                "/availability/v1/commands",
                Some(&auth),
                Some("command-1"),
                Some(command_body()),
            )
            .await,
        );
    }

    assert_eq!(responses[0].0, StatusCode::OK);
    assert_eq!(responses[0].2, responses[1].2);
    assert_eq!(*control.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn commands_require_scope_and_a_control_plane() {
    let (_dir, state) = app(false, false).await;
    assert_eq!(
        send(
            &state,
            coordinator(),
            Method::POST,
            "/availability/v1/commands",
            Some(&basic(OPERATOR, PASSWORD)),
            None,
            Some(command_body()),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &state,
            coordinator(),
            Method::POST,
            "/availability/v1/commands",
            Some(&basic(ADMIN, PASSWORD)),
            None,
            Some(command_body()),
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn command_failures_map_to_http_statuses() {
    for (error, expected) in [
        (
            ControlError::NotLeader { leader: None },
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (ControlError::Invalid("same home".to_owned()), StatusCode::CONFLICT),
        (ControlError::Overloaded, StatusCode::TOO_MANY_REQUESTS),
    ] {
        let (_dir, state) = app(false, false).await;
        let (services, _) = control_services(Err(error));
        assert_eq!(
            send_with(
                harness(&state, services, coordinator()),
                Method::POST,
                "/availability/v1/commands",
                Some(&basic(ADMIN, PASSWORD)),
                None,
                Some(command_body()),
            )
            .await
            .0,
            expected
        );
    }
}

#[tokio::test]
async fn transfer_commits_a_sealed_audit() {
    let (_dir, state) = app(false, false).await;
    let services = consensus_services(Ok(committed(9)));
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        Arc::new(FixedFrontier(Ok(Some(10)))),
        Duration::ZERO,
        1,
        RETAINED,
    ));

    let (status, headers, body) = send_with(
        harness(&state, services, coordinator),
        Method::POST,
        "/availability/v1/transfers",
        Some(&basic(ADMIN, PASSWORD)),
        Some("transfer-1"),
        Some(transfer_body()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(body["target"], "west");
    assert_eq!(body["epoch"], 7);
    assert_eq!(state.serving.meta.transfer_audits("proj").unwrap().len(), 1);
}

#[tokio::test]
async fn transfer_requires_scope_consensus_and_a_readable_barrier() {
    let (_dir, state) = app(false, false).await;
    for (user, expected) in [
        (OPERATOR, StatusCode::FORBIDDEN),
        (ADMIN, StatusCode::SERVICE_UNAVAILABLE),
    ] {
        assert_eq!(
            send(
                &state,
                coordinator(),
                Method::POST,
                "/availability/v1/transfers",
                Some(&basic(user, PASSWORD)),
                None,
                Some(transfer_body()),
            )
            .await
            .0,
            expected
        );
    }

    let (_dir, state) = app(false, true).await;
    let services = consensus_services(Ok(committed(9)));
    assert_eq!(
        send_with(
            harness(&state, services, coordinator()),
            Method::POST,
            "/availability/v1/transfers",
            Some(&basic(ADMIN, PASSWORD)),
            None,
            Some(transfer_body()),
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn transfer_maps_frontier_timeout_and_commit_failures() {
    for (frontier, control, expected) in [
        (
            FixedFrontier(Err("frontier unavailable")),
            Ok(committed(9)),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            FixedFrontier(Ok(Some(0))),
            Ok(committed(9)),
            StatusCode::GATEWAY_TIMEOUT,
        ),
        (
            FixedFrontier(Ok(Some(10))),
            Err(ControlError::NotLeader { leader: None }),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let (_dir, state) = app(false, false).await;
        state.serving.meta.next_serial().unwrap();
        let services = consensus_services(control);
        let coordinator = Arc::new(TransferCoordinator::with_schedule(
            Arc::new(frontier),
            Duration::ZERO,
            1,
            RETAINED,
        ));
        assert_eq!(
            send_with(
                harness(&state, services, coordinator),
                Method::POST,
                "/availability/v1/transfers",
                Some(&basic(ADMIN, PASSWORD)),
                None,
                Some(transfer_body()),
            )
            .await
            .0,
            expected
        );
    }
}

#[tokio::test(start_paused = true)]
async fn active_transfer_conflicts_then_cancels() {
    let (_dir, state) = app(false, false).await;
    let services = consensus_services(Ok(committed(9)));
    state.serving.meta.next_serial().unwrap();
    let probed = Arc::new(Notify::new());
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        Arc::new(GatedFrontier {
            probed: probed.clone(),
            applied: 0,
        }),
        Duration::from_secs(30),
        3,
        RETAINED,
    ));
    let running = tokio::spawn({
        let state = state.clone();
        let services = services.clone();
        let coordinator = coordinator.clone();
        async move {
            send_with(
                harness(&state, services, coordinator),
                Method::POST,
                "/availability/v1/transfers",
                Some(&basic(ADMIN, PASSWORD)),
                None,
                Some(transfer_body()),
            )
            .await
        }
    });
    probed.notified().await;

    assert_eq!(
        send_with(
            harness(&state, services.clone(), coordinator.clone()),
            Method::POST,
            "/availability/v1/transfers",
            Some(&basic(ADMIN, PASSWORD)),
            None,
            Some(transfer_body()),
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        send_with(
            harness(&state, services, coordinator),
            Method::DELETE,
            "/availability/v1/transfers/proj",
            Some(&basic(ADMIN, PASSWORD)),
            None,
            None,
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(running.await.unwrap().0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn cancel_maps_scope_unknown_and_committed_states() {
    let (_dir, state) = app(false, false).await;
    assert_eq!(
        send(
            &state,
            coordinator(),
            Method::DELETE,
            "/availability/v1/transfers/proj",
            Some(&basic(OPERATOR, PASSWORD)),
            None,
            None,
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &state,
            coordinator(),
            Method::DELETE,
            "/availability/v1/transfers/ghost",
            Some(&basic(ADMIN, PASSWORD)),
            None,
            None,
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    let services = consensus_services(Ok(committed(9)));
    let coordinator = Arc::new(TransferCoordinator::with_schedule(
        Arc::new(FixedFrontier(Ok(Some(10)))),
        Duration::ZERO,
        1,
        RETAINED,
    ));
    assert_eq!(
        send_with(
            harness(&state, services.clone(), coordinator.clone()),
            Method::POST,
            "/availability/v1/transfers",
            Some(&basic(ADMIN, PASSWORD)),
            None,
            Some(transfer_body()),
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        send_with(
            harness(&state, services, coordinator),
            Method::DELETE,
            "/availability/v1/transfers/proj",
            Some(&basic(ADMIN, PASSWORD)),
            None,
            None,
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
}

#[test]
fn response_mappers_cover_each_status_class() {
    let audit = TransferAudit {
        authority: AuthorityKey("proj".to_owned()),
        source: DatacenterId("east".to_owned()),
        target: DatacenterId("west".to_owned()),
        actor: ADMIN.to_owned(),
        reason: "drain".to_owned(),
        barrier: 3,
        epoch: AuthorityEpoch(4),
        commit_index: 5,
    };
    assert_eq!(transfer_committed(&audit).status(), StatusCode::OK);
    assert_eq!(
        run_error(&TransferRunError::Busy("proj".to_owned())).status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        run_error(&TransferRunError::BarrierNotReached).status(),
        StatusCode::GATEWAY_TIMEOUT
    );
    assert_eq!(
        drive_error(&TransferDriveError::Plan(TransferError::Cancelled)).status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        drive_error(&TransferDriveError::Frontier(anyhow::anyhow!("offline"))).status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        cancel_error(&TransferCancelError::Unknown("proj".to_owned())).status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        cancel_error(&TransferCancelError::AlreadyCommitted("proj".to_owned())).status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        cancel_error(&TransferCancelError::Durable(
            "proj".to_owned(),
            anyhow::anyhow!("unreadable")
        ))
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(no_consensus().status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(unauthorized().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(forbidden().status(), StatusCode::FORBIDDEN);
    assert_eq!(unavailable().status(), StatusCode::SERVICE_UNAVAILABLE);
}
