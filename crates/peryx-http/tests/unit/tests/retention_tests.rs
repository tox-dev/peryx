use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use http_body_util::BodyExt as _;
use peryx_core::Ecosystem;
use peryx_driver::BlockingScanExecutor;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::serving::{NameDriver, RetentionDriver};
use peryx_driver::state::{AppState, Clock, Index, IndexKind, ServingState};
use peryx_driver::users::UserService;
use peryx_identity::{GrantScope, IndexAcl, PasswordPolicy, Role};
use peryx_policy::{
    Policy, RetentionClass, RetentionDecision, RetentionFrontier, RetentionOutcome, RetentionSummary,
    RetentionVisibility,
};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

const ADMIN_PASSWORD: &str = "administrator password";
const OPERATOR_PASSWORD: &str = "operator password";
const UNSUPPORTED_SELECTOR_ERROR: &str = "example retention does not support selector \"cached\"";

type PlanCalls = Arc<Mutex<Vec<(String, Option<i64>)>>>;

/// A driver whose plan is fixed test data: `unsupported` returns no plan at all, `fail` raises a store
/// error mid-scan, and otherwise it emits `decisions` in order.
struct StubDriver {
    decisions: Vec<RetentionDecision>,
    unsupported: bool,
    fail: Option<String>,
}

impl RetentionDriver for StubDriver {
    fn validate_retention(&self, _policy: &peryx_policy::RetentionPolicy) -> Result<(), String> {
        Ok(())
    }

    fn plan_retention(
        &self,
        scan: &peryx_driver::serving::RetentionScan<'_>,
        start: &mut dyn FnMut(RetentionSummary) -> Result<(), String>,
        emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate_retention(scan.policy)?;
        let generation = scan
            .meta
            .policy_input_generation(scan.index)
            .map_err(|error| error.to_string())?;
        start(RetentionSummary {
            policy_version: scan.policy.version(),
            frontier: RetentionFrontier {
                repository: generation.repository,
                catalog: generation.catalog,
                policy: generation.policy,
            },
        })?;
        for decision in &self.decisions {
            emit(decision.clone())?;
        }
        if let Some(reason) = &self.fail {
            return Err(reason.clone());
        }
        Ok(())
    }
}

struct TimestampDriver {
    calls: PlanCalls,
    validation_error: Option<&'static str>,
}

impl RetentionDriver for TimestampDriver {
    fn validate_retention(&self, _policy: &peryx_policy::RetentionPolicy) -> Result<(), String> {
        self.validation_error.map_or(Ok(()), |error| Err(error.to_owned()))
    }

    fn plan_retention(
        &self,
        scan: &peryx_driver::serving::RetentionScan<'_>,
        start: &mut dyn FnMut(RetentionSummary) -> Result<(), String>,
        emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate_retention(scan.policy)?;
        self.calls.lock().unwrap().push((scan.index.to_owned(), scan.now));
        StubDriver {
            decisions: ["a", "b", "c"].into_iter().map(decision).collect(),
            unsupported: false,
            fail: None,
        }
        .plan_retention(scan, start, emit)
    }
}

impl NameDriver for StubDriver {
    fn normalize_name(&self, name: &str) -> String {
        name.to_ascii_lowercase().replace(['.', '_'], "-")
    }
}

#[test]
fn test_stub_driver_implements_required_contracts() {
    let description = peryx_driver::state::IndexDescription {
        name: "test".to_owned(),
        route: "test".to_owned(),
        ecosystem: "example".to_owned(),
        kind: "hosted",
        layers: Vec::new(),
        precedence: Vec::new(),
        uploads: false,
        volatile_deletes: false,
        upload_to: None,
        upstream: None,
        hosted: None,
    };

    assert_eq!(description.ecosystem, "example");
}

fn decision(artifact: &str) -> RetentionDecision {
    RetentionDecision {
        resource: "demo".to_owned(),
        group: Some("1.0".to_owned()),
        artifact: artifact.to_owned(),
        digest: format!("sha-{artifact}"),
        class: RetentionClass::Hosted,
        visibility: RetentionVisibility::Active,
        source: None,
        bytes: 10,
        outcome: RetentionOutcome::Remove,
        rule: Some("resource-prefix"),
        retained_groups: Vec::new(),
    }
}

fn hosted_index(name: &str, ecosystem: Ecosystem) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem,
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: Vec::new(),
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StoreFault {
    None,
    Authentication,
    Generation,
}

struct Fixture {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
    serving: Arc<ServingState>,
    app: axum::Router,
}

impl Fixture {
    async fn new(driver: StubDriver) -> Self {
        Self::with_capabilities(driver, StoreFault::None, true).await
    }

    async fn without_name(driver: StubDriver) -> Self {
        Self::with_capabilities(driver, StoreFault::None, false).await
    }

    async fn with_fault(driver: StubDriver, fault: StoreFault) -> Self {
        Self::with_capabilities(driver, fault, true).await
    }

    async fn with_driver(driver: Arc<dyn RetentionDriver>, clock: Clock) -> Self {
        Self::build(Some(driver), None, StoreFault::None, Some(clock)).await
    }

    async fn with_capabilities(driver: StubDriver, fault: StoreFault, names: bool) -> Self {
        let driver = (!driver.unsupported).then(|| Arc::new(driver));
        let name_driver = if names {
            driver.clone().map(|driver| driver as Arc<dyn NameDriver>)
        } else {
            None
        };
        Self::build(
            driver.map(|driver| driver as Arc<dyn RetentionDriver>),
            name_driver,
            fault,
            None,
        )
        .await
    }

    async fn build(
        driver: Option<Arc<dyn RetentionDriver>>,
        name_driver: Option<Arc<dyn NameDriver>>,
        fault: StoreFault,
        clock: Option<Clock>,
    ) -> Self {
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
        corrupt(&path, fault, administrator.as_str());
        let meta = MetaStore::open_existing(&path).unwrap();
        let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
        let mut state = AppState::new(
            meta.clone(),
            blobs,
            60,
            vec![
                hosted_index("hosted", Ecosystem::new("example")),
                hosted_index("hosted-two", Ecosystem::new("example")),
                hosted_index("beta-repo", Ecosystem::new("other")),
            ],
        );
        let serving = Arc::get_mut(&mut state.serving).unwrap();
        serving.users = UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        if let Some(clock) = clock {
            serving.clock = clock;
        }
        state.register_capabilities(|registrar| {
            if let Some(driver) = driver {
                registrar.register_retention(Ecosystem::new("example"), driver);
            }
            if let Some(driver) = name_driver {
                registrar.register_name(Ecosystem::new("example"), driver);
            }
        });
        let serving = state.serving.clone();
        let state = Arc::new(state);
        Self {
            _dir: dir,
            app: crate::router(state.clone()),
            state,
            serving,
        }
    }

    async fn post(
        &self,
        uri: &str,
        credential: Option<(&str, &str)>,
        body: Body,
        json: bool,
    ) -> axum::response::Response {
        let mut request = Request::builder().method(Method::POST).uri(uri);
        if let Some((user, password)) = credential {
            request = request.header(
                header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
            );
        }
        if json {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        self.app.clone().oneshot(request.body(body).unwrap()).await.unwrap()
    }

    async fn plan(&self, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let response = self
            .post(
                "/+retention/plan",
                Some(("Alice", ADMIN_PASSWORD)),
                Body::from(serde_json::to_vec(&body).unwrap()),
                true,
            )
            .await;
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_status_responds_at_blocking_scan_capacity() {
    let fixture = Fixture::new(StubDriver {
        decisions: Vec::new(),
        unsupported: false,
        fail: None,
    })
    .await;
    let scans = SaturatedScans::start(fixture.state.blocking_scans.clone()).await;

    let response = fixture
        .app
        .clone()
        .oneshot(Request::builder().uri("/+status").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let status = response.status();
    scans.release().await;

    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_retention_export_keeps_its_existing_blocking_path() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a")],
        unsupported: false,
        fail: None,
    })
    .await;
    let scans = SaturatedScans::start(fixture.state.blocking_scans.clone()).await;

    let response = fixture
        .post(
            "/+retention/export",
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from(serde_json::to_vec(&plan_body("hosted")).unwrap()),
            true,
        )
        .await;

    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    scans.release().await;

    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8(body.to_vec()).unwrap().contains("\"artifact\":\"a\""));
}

struct SaturatedScans {
    release: Arc<(Mutex<bool>, Condvar)>,
    scans: Vec<tokio::task::JoinHandle<Result<(), tokio::task::JoinError>>>,
}

impl SaturatedScans {
    async fn start(executor: BlockingScanExecutor) -> Self {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut scans = Vec::new();
        for _ in 0..2 {
            let executor = executor.clone();
            let release = release.clone();
            let started_tx = started_tx.clone();
            scans.push(tokio::spawn(async move {
                executor
                    .run(move |_| {
                        started_tx.send(()).unwrap();
                        let (released, changed) = &*release;
                        let mut released = released.lock().unwrap();
                        while !*released {
                            released = changed.wait(released).unwrap();
                        }
                        drop(released);
                    })
                    .await
            }));
        }
        for _ in 0..2 {
            started_rx.recv().await.unwrap();
        }
        Self { release, scans }
    }

    async fn release(mut self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
        for scan in self.scans.drain(..) {
            scan.await.unwrap().unwrap();
        }
    }
}

impl Drop for SaturatedScans {
    fn drop(&mut self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}

async fn timestamp_fixture() -> (Fixture, Arc<AtomicI64>, PlanCalls) {
    let now = Arc::new(AtomicI64::new(100));
    let clock_now = now.clone();
    let clock: Clock = Arc::new(move || clock_now.load(Ordering::Relaxed));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fixture = Fixture::with_driver(
        Arc::new(TimestampDriver {
            calls: calls.clone(),
            validation_error: None,
        }),
        clock,
    )
    .await;
    (fixture, now, calls)
}

fn plan_body(repository: &str) -> serde_json::Value {
    serde_json::json!({"repository": repository, "expire": [{"selector": "resource-prefix", "prefix": ""}]})
}

fn corrupt(path: &std::path::Path, fault: StoreFault, administrator: &str) {
    let (table, key) = match fault {
        StoreFault::None => return,
        StoreFault::Authentication => ("server_user_verifier", administrator),
        StoreFault::Generation => ("policy_input_generation", "hosted"),
    };
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_write().unwrap();
    {
        let mut handle = txn
            .open_table(redb::TableDefinition::<&str, &[u8]>::new(table))
            .unwrap();
        handle.insert(key, b"{ not json".as_slice()).unwrap();
    }
    txn.commit().unwrap();
}

#[tokio::test]
async fn test_plan_returns_ordered_candidates_and_identity_for_an_administrator() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a"), decision("b")],
        unsupported: false,
        fail: None,
    })
    .await;

    let (status, body) = fixture.plan(plan_body("hosted")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(body["candidates"][0]["artifact"], "a");
    assert!(body["summary"]["policy_version"].is_number());
    assert!(body["next_cursor"].is_null());
}

#[rstest]
#[case::plan("/+retention/plan")]
#[case::export("/+retention/export")]
#[tokio::test]
async fn test_retention_rejects_an_unsupported_selector_before_output(#[case] uri: &str) {
    let fixture = Fixture::with_driver(
        Arc::new(TimestampDriver {
            calls: Arc::new(Mutex::new(Vec::new())),
            validation_error: Some(UNSUPPORTED_SELECTOR_ERROR),
        }),
        Arc::new(|| 0),
    )
    .await;

    let response = fixture
        .post(
            uri,
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "repository": "hosted",
                    "expire": [{"selector": "cached"}],
                }))
                .unwrap(),
            ),
            true,
        )
        .await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body.as_ref(),
        br#"{"error":"example retention does not support selector \"cached\""}"#
    );
}

#[tokio::test]
async fn test_plan_normalizes_prefixes_with_the_repository_name_driver() {
    let fixture = Fixture::new(StubDriver {
        decisions: Vec::new(),
        unsupported: false,
        fail: None,
    })
    .await;
    let mut versions = Vec::new();
    for prefix in ["Acme_Tools", "acme.tools", "acme-tools"] {
        let (status, body) = fixture
            .plan(serde_json::json!({
                "repository": "hosted",
                "expire": [{"selector": "resource-prefix", "prefix": prefix}],
            }))
            .await;
        assert_eq!(status, StatusCode::OK);
        versions.push(body["summary"]["policy_version"].clone());
    }

    assert_eq!(versions, vec![versions[0].clone(); 3]);
}

#[tokio::test]
async fn test_plan_keeps_prefixes_unchanged_without_a_name_driver() {
    let fixture = Fixture::without_name(StubDriver {
        decisions: Vec::new(),
        unsupported: false,
        fail: None,
    })
    .await;
    let (_, upper) = fixture
        .plan(serde_json::json!({
            "repository": "hosted",
            "expire": [{"selector": "resource-prefix", "prefix": "Demo"}],
        }))
        .await;
    let (_, lower) = fixture
        .plan(serde_json::json!({
            "repository": "hosted",
            "expire": [{"selector": "resource-prefix", "prefix": "demo"}],
        }))
        .await;

    assert_ne!(upper["summary"]["policy_version"], lower["summary"]["policy_version"]);
}

#[tokio::test]
async fn test_plan_pages_then_resumes_from_the_cursor() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a"), decision("b"), decision("c")],
        unsupported: false,
        fail: None,
    })
    .await;

    let mut body = plan_body("hosted");
    body["limit"] = serde_json::json!(1);
    let (status, first) = fixture.plan(body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["candidates"][0]["artifact"], "a");
    let cursor = first["next_cursor"].as_str().expect("a full page carries a cursor");

    body["cursor"] = serde_json::json!(cursor);
    let (status, second) = fixture.plan(body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["candidates"][0]["artifact"], "b");
}

#[tokio::test]
async fn test_plan_reuses_the_cursor_evaluation_time() {
    let (fixture, now, calls) = timestamp_fixture().await;
    let mut body = plan_body("hosted");
    body["limit"] = serde_json::json!(1);
    let (status, first) = fixture.plan(body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["candidates"][0]["artifact"], "a");

    now.store(200, Ordering::Relaxed);
    body["cursor"] = first["next_cursor"].clone();
    let (status, second) = fixture.plan(body).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["candidates"][0]["artifact"], "b");
    assert_eq!(
        *calls.lock().unwrap(),
        vec![("hosted".to_owned(), Some(100)), ("hosted".to_owned(), Some(100))]
    );
}

#[tokio::test]
async fn test_plan_rejects_a_cursor_from_another_repository_before_scanning() {
    let (fixture, _, calls) = timestamp_fixture().await;
    let mut body = plan_body("hosted");
    body["limit"] = serde_json::json!(1);
    let (status, first) = fixture.plan(body).await;
    assert_eq!(status, StatusCode::OK);

    let mut replay = plan_body("hosted-two");
    replay["cursor"] = first["next_cursor"].clone();
    let (status, response) = fixture.plan(replay).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        response,
        serde_json::json!({"error": "the plan cursor is stale: the repository changed"})
    );
    assert_eq!(*calls.lock().unwrap(), vec![("hosted".to_owned(), Some(100))]);
}

#[tokio::test]
async fn test_plan_rejects_a_tampered_cursor() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a"), decision("b")],
        unsupported: false,
        fail: None,
    })
    .await;
    let mut body = plan_body("hosted");
    body["limit"] = serde_json::json!(1);
    let (status, first) = fixture.plan(body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let mut cursor = URL_SAFE_NO_PAD.decode(first["next_cursor"].as_str().unwrap()).unwrap();
    *cursor.last_mut().unwrap() = 0xff;
    body["cursor"] = serde_json::json!(URL_SAFE_NO_PAD.encode(cursor));

    let (status, response) = fixture.plan(body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response, serde_json::json!({"error": "invalid retention plan cursor"}));
}

#[tokio::test]
async fn test_plan_rejects_an_unbound_cursor() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a")],
        unsupported: false,
        fail: None,
    })
    .await;
    let mut body = plan_body("hosted");
    body["cursor"] = serde_json::json!(
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "after": 0,
                "summary": RetentionSummary {
                    policy_version: 999,
                    frontier: RetentionFrontier::default(),
                },
            }))
            .unwrap(),
        )
    );

    let (status, response) = fixture.plan(body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response, serde_json::json!({"error": "invalid retention plan cursor"}));
}

#[tokio::test]
async fn test_plan_rejects_a_stale_cursor() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a"), decision("b")],
        unsupported: false,
        fail: None,
    })
    .await;
    let mut body = plan_body("hosted");
    body["limit"] = serde_json::json!(1);
    let (status, first) = fixture.plan(body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    fixture.serving.meta.advance_policy_generation("hosted").unwrap();
    body["cursor"] = first["next_cursor"].clone();

    let (status, _) = fixture.plan(body).await;

    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_plan_reports_a_repository_without_retention_as_not_found() {
    let fixture = Fixture::new(StubDriver {
        decisions: Vec::new(),
        unsupported: true,
        fail: None,
    })
    .await;

    let (status, _) = fixture.plan(plan_body("hosted")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_plan_reports_a_store_failure_as_server_error() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a")],
        unsupported: false,
        fail: Some("meta read failed".to_owned()),
    })
    .await;

    let (status, _) = fixture.plan(plan_body("hosted")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_plan_worker_panic_returns_server_error() {
    let fixture = Fixture::with_driver(Arc::new(PanicDriver), Arc::new(|| 42)).await;

    let (status, document) = fixture.plan(plan_body("hosted")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        document["error"]
            .as_str()
            .unwrap()
            .starts_with("retention scan worker failed:")
    );
}

struct PanicDriver;

impl RetentionDriver for PanicDriver {
    fn validate_retention(&self, _policy: &peryx_policy::RetentionPolicy) -> Result<(), String> {
        Ok(())
    }

    fn plan_retention(
        &self,
        _scan: &peryx_driver::serving::RetentionScan<'_>,
        _start: &mut dyn FnMut(RetentionSummary) -> Result<(), String>,
        _emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String> {
        panic!("retention scan panic")
    }
}

#[tokio::test]
async fn test_plan_rejects_an_unknown_repository() {
    let fixture = Fixture::new(StubDriver {
        decisions: Vec::new(),
        unsupported: false,
        fail: None,
    })
    .await;

    let (status, _) = fixture.plan(plan_body("absent")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_plan_rejects_a_repository_whose_ecosystem_has_no_driver() {
    let fixture = Fixture::new(StubDriver {
        decisions: Vec::new(),
        unsupported: false,
        fail: None,
    })
    .await;

    let (status, _) = fixture.plan(plan_body("beta-repo")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_plan_rejects_an_invalid_cursor() {
    let fixture = Fixture::new(StubDriver {
        decisions: Vec::new(),
        unsupported: false,
        fail: None,
    })
    .await;
    let mut body = plan_body("hosted");
    body["cursor"] = serde_json::json!("not a cursor");

    let (status, _) = fixture.plan(body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_plan_rejects_an_out_of_range_limit() {
    let fixture = Fixture::new(StubDriver {
        decisions: Vec::new(),
        unsupported: false,
        fail: None,
    })
    .await;
    let mut body = plan_body("hosted");
    body["limit"] = serde_json::json!(0);

    let (status, _) = fixture.plan(body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_plan_rejects_anonymous_and_non_administrator_callers() {
    let fixture = Fixture::new(StubDriver {
        decisions: Vec::new(),
        unsupported: false,
        fail: None,
    })
    .await;
    let body = Body::from(serde_json::to_vec(&plan_body("hosted")).unwrap());

    let anonymous = fixture.post("/+retention/plan", None, body, true).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let wrong = fixture
        .post(
            "/+retention/plan",
            Some(("Alice", "wrong password")),
            Body::from(serde_json::to_vec(&plan_body("hosted")).unwrap()),
            true,
        )
        .await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let operator = fixture
        .post(
            "/+retention/plan",
            Some(("Olivia", OPERATOR_PASSWORD)),
            Body::from(serde_json::to_vec(&plan_body("hosted")).unwrap()),
            true,
        )
        .await;
    assert_eq!(operator.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_plan_requires_a_json_body_within_the_size_limit() {
    let fixture = Fixture::new(StubDriver {
        decisions: Vec::new(),
        unsupported: false,
        fail: None,
    })
    .await;

    let untyped = fixture
        .post(
            "/+retention/plan",
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from("{}"),
            false,
        )
        .await;
    assert_eq!(untyped.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let malformed = fixture
        .post(
            "/+retention/plan",
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from("not json"),
            true,
        )
        .await;
    assert_eq!(malformed.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let huge = fixture
        .post(
            "/+retention/plan",
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from(vec![b'x'; 70 * 1024]),
            true,
        )
        .await;
    assert_eq!(huge.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_plan_reports_authentication_storage_failure_as_unavailable() {
    let fixture = Fixture::with_fault(
        StubDriver {
            decisions: Vec::new(),
            unsupported: false,
            fail: None,
        },
        StoreFault::Authentication,
    )
    .await;

    let response = fixture
        .post(
            "/+retention/plan",
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from(serde_json::to_vec(&plan_body("hosted")).unwrap()),
            true,
        )
        .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_plan_refuses_when_the_repository_is_at_its_concurrency_bound() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a")],
        unsupported: false,
        fail: None,
    })
    .await;
    let held: Vec<_> = (0..2)
        .map(|_| fixture.serving.retention_gates.try_enter("hosted").unwrap())
        .collect();

    let (status, _) = fixture.plan(plan_body("hosted")).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    drop(held);
}

#[tokio::test]
async fn test_export_streams_json_lines_with_the_identity_first() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a"), decision("b")],
        unsupported: false,
        fail: None,
    })
    .await;

    let response = fixture
        .post(
            "/+retention/export",
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from(serde_json::to_vec(&plan_body("hosted")).unwrap()),
            true,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/x-ndjson"
    );
    assert!(response.headers().get(header::ETAG).is_some());
    assert_eq!(response.headers().get(header::ACCEPT_RANGES).unwrap(), "none");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("summary"));
    assert!(lines[1].contains("\"artifact\":\"a\""));
}

#[tokio::test]
async fn test_export_rejects_a_stale_cursor_before_streaming() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a"), decision("b")],
        unsupported: false,
        fail: None,
    })
    .await;
    let mut body = plan_body("hosted");
    body["limit"] = serde_json::json!(1);
    let (status, first) = fixture.plan(body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    fixture.serving.meta.advance_policy_generation("hosted").unwrap();
    body["cursor"] = first["next_cursor"].clone();

    let response = fixture
        .post(
            "/+retention/export",
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from(serde_json::to_vec(&body).unwrap()),
            true,
        )
        .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_export_rejects_a_cursor_from_another_repository_before_streaming() {
    let (fixture, _, calls) = timestamp_fixture().await;
    let mut body = plan_body("hosted");
    body["limit"] = serde_json::json!(1);
    let (status, first) = fixture.plan(body).await;
    assert_eq!(status, StatusCode::OK);

    let mut replay = plan_body("hosted-two");
    replay["cursor"] = first["next_cursor"].clone();
    let response = fixture
        .post(
            "/+retention/export",
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from(serde_json::to_vec(&replay).unwrap()),
            true,
        )
        .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(*calls.lock().unwrap(), vec![("hosted".to_owned(), Some(100))]);
}

#[tokio::test]
async fn test_export_rejects_an_anonymous_caller() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a")],
        unsupported: false,
        fail: None,
    })
    .await;

    let response = fixture
        .post(
            "/+retention/export",
            None,
            Body::from(serde_json::to_vec(&plan_body("hosted")).unwrap()),
            true,
        )
        .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_export_refuses_when_the_repository_is_at_its_concurrency_bound() {
    let fixture = Fixture::new(StubDriver {
        decisions: vec![decision("a")],
        unsupported: false,
        fail: None,
    })
    .await;
    let held: Vec<_> = (0..2)
        .map(|_| fixture.serving.retention_gates.try_enter("hosted").unwrap())
        .collect();

    let response = fixture
        .post(
            "/+retention/export",
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from(serde_json::to_vec(&plan_body("hosted")).unwrap()),
            true,
        )
        .await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(held);
}

#[tokio::test]
async fn test_export_reports_a_store_failure_reading_the_identity() {
    let fixture = Fixture::with_fault(
        StubDriver {
            decisions: vec![decision("a")],
            unsupported: false,
            fail: None,
        },
        StoreFault::Generation,
    )
    .await;

    let response = fixture
        .post(
            "/+retention/export",
            Some(("Alice", ADMIN_PASSWORD)),
            Body::from(serde_json::to_vec(&plan_body("hosted")).unwrap()),
            true,
        )
        .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
