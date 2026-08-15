use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::jobs::{
    JobContext, JobFailure, JobLimits, JobReport, JobScheduler, LeaseScope, NodeJob, NodeJobMetadata, Submit,
};
use peryx_driver::state::AppState;
use peryx_driver::users::UserService;
use peryx_identity::{GrantScope, PasswordPolicy, Role, UserId};
use peryx_storage::meta::{JobKind, JobOutcome, JobRunQuery, JobState, MetaStore, NewJobRun};
use tokio::sync::Notify;
use tower::ServiceExt as _;

const ADMIN_PASSWORD: &str = "administrator password";
const OPERATOR_PASSWORD: &str = "operator password";

#[derive(Clone, Copy)]
enum StoreFault {
    None,
    Authentication,
    Authorization,
    JobStore,
}

struct Fixture {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
    app: axum::Router,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_fault(StoreFault::None).await
    }

    async fn with_fault(fault: StoreFault) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peryx.redb");
        let meta = MetaStore::open(&path).unwrap();
        let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let administrator = users.create("Alice").unwrap().id;
        users.set_password(&administrator, ADMIN_PASSWORD).await.unwrap();
        AuthorizationService::new(meta.clone())
            .grant(&administrator, Role::Administrator, GrantScope::Server)
            .unwrap();
        let operator = users.create("Olivia").unwrap().id;
        users.set_password(&operator, OPERATOR_PASSWORD).await.unwrap();
        AuthorizationService::new(meta.clone())
            .grant(&operator, Role::Operator, GrantScope::Server)
            .unwrap();
        drop(users);
        drop(meta);
        apply_fault(&path, fault, &administrator);
        let meta = MetaStore::open_existing(&path).unwrap();
        let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
        let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| 42));
        Arc::get_mut(&mut state.serving).unwrap().users = users;
        let state = Arc::new(state);
        let app = crate::router(state.clone());
        Self { _dir: dir, state, app }
    }

    async fn cancel(&self, id: &str, credential: Option<(&str, &str)>) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(format!("/+jobs/{id}/cancel"));
        if let Some((user, password)) = credential {
            request = request.header(
                header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
            );
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn seed_run(&self, finish: bool) -> String {
        let id = self
            .state
            .serving
            .meta
            .start_job_run(NewJobRun {
                kind: JobKind::new("cache_refresh").unwrap(),
                scope: "",
                repository: None,
                started_at_unix: 1,
            })
            .unwrap();
        if finish {
            self.state
                .serving
                .meta
                .finish_job_run(&id, JobOutcome::succeeded(2, 0, 0))
                .unwrap();
        }
        id
    }
}

fn apply_fault(path: &std::path::Path, fault: StoreFault, administrator: &UserId) {
    match fault {
        StoreFault::None => {}
        StoreFault::Authentication => corrupt_value(path, "server_user_verifier", administrator.as_str()),
        StoreFault::Authorization => corrupt_prefix(path, "role_grant", &format!("{administrator}/")),
        StoreFault::JobStore => {
            let database = redb::Database::open(path).unwrap();
            let txn = database.begin_write().unwrap();
            txn.delete_table(redb::TableDefinition::<&str, &[u8]>::new("job_run"))
                .unwrap();
            txn.open_table(redb::TableDefinition::<&str, u64>::new("job_run"))
                .unwrap();
            txn.commit().unwrap();
        }
    }
}

fn corrupt_value(path: &std::path::Path, table: &'static str, key: &str) {
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, &[u8]>::new(table))
        .unwrap()
        .insert(key, b"{".as_slice())
        .unwrap();
    txn.commit().unwrap();
}

fn corrupt_prefix(path: &std::path::Path, table: &'static str, prefix: &str) {
    let definition = redb::TableDefinition::<&str, &[u8]>::new(table);
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_write().unwrap();
    let keys = {
        let handle = txn.open_table(definition).unwrap();
        redb::ReadableTable::iter(&handle)
            .unwrap()
            .map(|entry| entry.unwrap().0.value().to_owned())
            .filter(|key| key.starts_with(prefix))
            .collect::<Vec<_>>()
    };
    let mut handle = txn.open_table(definition).unwrap();
    for key in keys {
        handle.insert(key.as_str(), b"{".as_slice()).unwrap();
    }
    drop(handle);
    txn.commit().unwrap();
}

/// A durable job that parks on its cancellation signal, so a test can observe an attempt in flight and
/// cancel it deterministically. `started` fires once its run begins, after the scheduler has registered
/// the attempt, so the test can find the running record and know the token is live before it cancels.
struct ParkedJob {
    started: Arc<Notify>,
}

#[async_trait]
impl NodeJob for ParkedJob {
    fn kind(&self) -> &'static str {
        "test_parked"
    }

    fn scope(&self) -> &'static str {
        ""
    }

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: None,
            persist_as: Some(JobKind::new("cache_refresh").unwrap()),
        }
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        self.started.notify_one();
        ctx.cancelled().await;
        Ok(JobReport::default())
    }
}

#[tokio::test]
async fn test_cancel_signals_a_running_attempt_and_it_unwinds() {
    let fixture = Fixture::new().await;
    let scheduler = JobScheduler::new(fixture.state.serving.clone(), JobLimits::node_local());
    let started = Arc::new(Notify::new());
    assert!(matches!(
        scheduler.submit(Arc::new(ParkedJob {
            started: started.clone()
        })),
        Submit::Queued
    ));
    started.notified().await;
    let running = fixture
        .state
        .serving
        .meta
        .query_job_runs(&JobRunQuery {
            cursor: None,
            limit: 25,
        })
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.state == JobState::Running)
        .expect("the parked attempt is running");

    let (status, _) = fixture.cancel(&running.id, Some(("Alice", ADMIN_PASSWORD))).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    scheduler.shutdown().await;
    assert_eq!(
        fixture
            .state
            .serving
            .meta
            .get_job_run(&running.id)
            .unwrap()
            .unwrap()
            .state,
        JobState::Cancelled
    );
}

#[tokio::test]
async fn test_cancel_a_finished_run_is_a_conflict() {
    let fixture = Fixture::new().await;
    let id = fixture.seed_run(true);
    let (status, body) = fixture.cancel(&id, Some(("Alice", ADMIN_PASSWORD))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body, serde_json::json!({"error": "job run already finished"}));
}

#[tokio::test]
async fn test_cancel_a_run_this_node_is_not_running_is_a_conflict() {
    let fixture = Fixture::new().await;
    let id = fixture.seed_run(false);
    let (status, body) = fixture.cancel(&id, Some(("Alice", ADMIN_PASSWORD))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        serde_json::json!({"error": "job run is not running on this node"})
    );
}

#[tokio::test]
async fn test_cancel_an_unknown_run_is_not_found() {
    let fixture = Fixture::new().await;
    let (status, _) = fixture
        .cancel("jr_00000000000000ff", Some(("Alice", ADMIN_PASSWORD)))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_cancel_without_a_credential_is_unauthorized() {
    let fixture = Fixture::new().await;
    let (status, _) = fixture.cancel("jr_00000000000000ff", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_cancel_with_a_wrong_password_is_unauthorized() {
    let fixture = Fixture::new().await;
    let (status, _) = fixture
        .cancel("jr_00000000000000ff", Some(("Alice", "wrong password")))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_cancel_by_a_non_administrator_is_not_found() {
    let fixture = Fixture::new().await;
    let id = fixture.seed_run(false);
    let (status, _) = fixture.cancel(&id, Some(("Olivia", OPERATOR_PASSWORD))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_cancel_when_authentication_storage_is_unavailable() {
    let fixture = Fixture::with_fault(StoreFault::Authentication).await;
    let (status, body) = fixture
        .cancel("jr_00000000000000ff", Some(("Alice", ADMIN_PASSWORD)))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, serde_json::json!({"error": "job control unavailable"}));
}

#[tokio::test]
async fn test_cancel_when_authorization_storage_is_unavailable() {
    let fixture = Fixture::with_fault(StoreFault::Authorization).await;
    let (status, _) = fixture
        .cancel("jr_00000000000000ff", Some(("Alice", ADMIN_PASSWORD)))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_cancel_when_job_storage_is_unavailable() {
    let fixture = Fixture::with_fault(StoreFault::JobStore).await;
    let (status, _) = fixture
        .cancel("jr_00000000000000ff", Some(("Alice", ADMIN_PASSWORD)))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
