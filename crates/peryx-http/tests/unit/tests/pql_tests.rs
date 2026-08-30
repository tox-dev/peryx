use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use peryx_core::Ecosystem;
use peryx_driver::ScanCancellation;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::http_services::{HttpDomainServices, PqlQueryService};
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_driver::users::UserService;
use peryx_events::metrics::{Metrics, Observation};
use peryx_identity::{Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, PasswordPolicy, Role};
use peryx_policy::{Policy, PolicyAction, PolicyDecisionState};
use peryx_pql::{Ast, Page, PqlError, QueryScope};
use peryx_storage::meta::{MetaStore, NewPolicyDecision};
use redb::TableDefinition;
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const READER_SECRET: &str = "reader-secret";
const NOREAD_SECRET: &str = "noread-secret";
const PUBLIC_SECRET: &str = "public-secret";
const PASSWORD: &str = "local password";

async fn app(read_only: bool) -> (tempfile::TempDir, MetaStore, axum::Router) {
    let (dir, meta, _metrics, router) = build(read_only).await;
    (dir, meta, router)
}

async fn build(read_only: bool) -> (tempfile::TempDir, MetaStore, Metrics, axum::Router) {
    let (dir, meta, metrics, state) = build_state(read_only).await;
    (dir, meta, metrics, crate::router(state))
}

async fn build_state(read_only: bool) -> (tempfile::TempDir, MetaStore, Metrics, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let authorization = AuthorizationService::new(meta.clone());
    for (name, role, scope) in [
        ("Alice", Role::Administrator, GrantScope::Server),
        (
            "Rita",
            Role::RepositoryReader,
            GrantScope::Repository {
                name: "private".to_owned(),
            },
        ),
        (
            "Morgan",
            Role::RepositoryReader,
            GrantScope::Repository {
                name: "other".to_owned(),
            },
        ),
    ] {
        let user = users.create(name).unwrap();
        users.set_password(&user.id, PASSWORD).await.unwrap();
        authorization.grant(&user.id, role, scope).unwrap();
    }
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, vec![index(), locked_index(), public_index()]);
    super::support::register_example_driver(&mut state);
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    state.set_read_only(read_only).unwrap();
    let metrics = state.serving.metrics.clone();
    (dir, meta, metrics, Arc::new(state))
}

fn seed_usage(metrics: &Metrics) {
    for (route, resource, bytes, times) in [
        ("private", "alpha", 100u64, 2),
        ("private", "beta", 50, 1),
        ("other", "gamma", 30, 1),
    ] {
        for _ in 0..times {
            metrics.record(Observation::Read {
                repository: route.to_owned(),
                resource: resource.to_owned(),
                artifact: format!("{resource}.bin"),
                group: None,
                source: None,
                bytes,
            });
        }
    }
    metrics.flush().unwrap();
}

async fn app_authentication_storage_fault() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let alice = users.create("Alice").unwrap();
    users.set_password(&alice.id, PASSWORD).await.unwrap();
    drop(users);
    drop(meta);

    let database = redb::Database::open(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(TableDefinition::<&str, &[u8]>::new("server_user_verifier"))
        .unwrap()
        .insert(alice.id.as_str(), b"{".as_slice())
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    let meta = MetaStore::open_existing(path).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, vec![index()]);
    super::support::register_example_driver(&mut state);
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, crate::router(Arc::new(state)))
}

/// An app whose authorization service reads a store whose `role_grant` table has the wrong shape, so
/// every grant lookup faults and the decision fails closed as [`DenyReason::StorageUnavailable`] while
/// user authentication still succeeds against the healthy store.
async fn app_authz_storage_fault() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let alice = users.create("Alice").unwrap();
    users.set_password(&alice.id, PASSWORD).await.unwrap();

    let broken_path = dir.path().join("broken.redb");
    let database = redb::Database::create(&broken_path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(TableDefinition::<&str, u64>::new("role_grant")).unwrap();
    txn.commit().unwrap();
    drop(database);
    let broken = AuthorizationService::new(MetaStore::open_existing(broken_path).unwrap());

    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, vec![index()]);
    super::support::register_example_driver(&mut state);
    let serving = Arc::get_mut(&mut state.serving).unwrap();
    serving.users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    serving.authorization = broken;
    (dir, crate::router(Arc::new(state)))
}

async fn app_policy_query_storage_fault() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let alice = users.create("Alice").unwrap();
    users.set_password(&alice.id, PASSWORD).await.unwrap();
    AuthorizationService::new(meta.clone())
        .grant(&alice.id, Role::Administrator, GrantScope::Server)
        .unwrap();
    meta.record_policy_decision(decision("private", "alpha", PolicyDecisionState::Deny, 30))
        .unwrap();
    drop(users);
    drop(meta);

    let database = redb::Database::open(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(TableDefinition::<&str, &[u8]>::new("policy_input_generation"))
        .unwrap()
        .insert("private", b"{".as_slice())
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    let meta = MetaStore::open_existing(path).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, vec![index()]);
    super::support::register_example_driver(&mut state);
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, crate::router(Arc::new(state)))
}

fn index() -> Index {
    Index {
        name: "private".to_owned(),
        // A route deliberately distinct from the name: PQL scopes by the stable repository name, so
        // `repository == "private"` must resolve regardless of the URL route.
        route: "private-route".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: false,
            tokens: vec![NamedToken {
                name: "reader".to_owned(),
                secret: READER_SECRET.to_owned(),
                grants: vec![Grant {
                    resources: vec![Glob::new("*")],
                    actions: BTreeSet::from([Action::Read]),
                }],
                expires_at: None,
            }],
        },
    }
}

/// A repository whose only token grants writes, never reads, so a ecosystem read is authenticated but
/// forbidden rather than unauthenticated.
fn locked_index() -> Index {
    Index {
        name: "locked".to_owned(),
        route: "locked-route".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: false,
            tokens: vec![NamedToken {
                name: "noread".to_owned(),
                secret: NOREAD_SECRET.to_owned(),
                grants: vec![Grant {
                    resources: vec![Glob::new("*")],
                    actions: BTreeSet::from([Action::Write]),
                }],
                expires_at: None,
            }],
        },
    }
}

/// A repository that serves artifacts to callers who present nothing, so operational reads there can
/// only be granted by a credential that resolves on its own.
fn public_index() -> Index {
    Index {
        name: "public".to_owned(),
        route: "public-route".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: vec![NamedToken {
                name: "public-reader".to_owned(),
                secret: PUBLIC_SECRET.to_owned(),
                grants: vec![Grant {
                    resources: vec![Glob::new("*")],
                    actions: BTreeSet::from([Action::Read]),
                }],
                expires_at: None,
            }],
        },
    }
}

fn decision<'a>(repository: &'a str, resource: &'a str, state: PolicyDecisionState, at: i64) -> NewPolicyDecision<'a> {
    NewPolicyDecision {
        repository,
        resource,
        group: Some("1.0"),
        artifact: Some("artifact-1.0.bin"),
        source: Some("alpha"),
        action: PolicyAction::Serve,
        state,
        rule: (state == PolicyDecisionState::Deny).then_some("blocked-resource"),
        reason: (state == PolicyDecisionState::Deny).then_some("resource is blocked"),
        evaluated_at_unix: at,
        next_eligible_at_unix: None,
    }
}

fn seed(meta: &MetaStore) {
    meta.record_policy_decision(decision("private", "alpha", PolicyDecisionState::Deny, 30))
        .unwrap();
    meta.record_policy_decision(decision("private", "beta", PolicyDecisionState::Allow, 20))
        .unwrap();
    meta.record_policy_decision(decision("other", "gamma", PolicyDecisionState::Deny, 10))
        .unwrap();
}

fn seed_many(meta: &MetaStore, repository: &str, count: i64) {
    for at in 0..count {
        let resource = format!("proj-{at}");
        meta.record_policy_decision(decision(repository, &resource, PolicyDecisionState::Allow, at))
            .unwrap();
    }
}

fn query_request(body: &Value, credential: Option<(&str, &str)>) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/+query")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some((user, password)) = credential {
        request = request.header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
        );
    }
    request.body(Body::from(serde_json::to_vec(body).unwrap())).unwrap()
}

async fn post(
    app: &axum::Router,
    body: Value,
    credential: Option<(&str, &str)>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let response = app.clone().oneshot(query_request(&body, credential)).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn test_query_cancellation_reaches_the_next_scan_page() {
    let (_dir, _meta, _metrics, state) = build_state(false).await;
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
    let services = HttpDomainServices::for_state(&state).with_pql(Arc::new(BoundaryPql {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        observed: observed_tx,
    }));
    let app = crate::router_with_services(state, services);
    let request = tokio::spawn(app.oneshot(query_request(
        &json!({"query": "from policy.decisions"}),
        Some(("Alice", PASSWORD)),
    )));
    entered_rx.recv().await.unwrap();

    request.abort();
    let request_result = request.await;
    release_tx.send(()).unwrap();
    let observed = observed_rx.recv().await;

    assert!(request_result.unwrap_err().is_cancelled());
    assert_eq!(observed, Some(true));
}

#[tokio::test]
async fn test_query_worker_panic_returns_server_error() {
    let (_dir, _meta, _metrics, state) = build_state(false).await;
    let services = HttpDomainServices::for_state(&state).with_pql(Arc::new(PanicPql));
    let app = crate::router_with_services(state, services);

    let (status, _, document) = post(
        &app,
        json!({"query": "from policy.decisions"}),
        Some(("Alice", PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        document["error"]
            .as_str()
            .unwrap()
            .starts_with("query scan worker failed:")
    );
}

struct PanicPql;

impl PqlQueryService for PanicPql {
    fn execute(
        &self,
        _ast: &Ast,
        _scope: &QueryScope,
        _cursor: Option<&str>,
        _cancellation: &ScanCancellation,
    ) -> Result<Page, PqlError> {
        panic!("query scan panic")
    }
}

/// Stands in for a paged scan that has reached a page boundary: it reports what the stop signal reads
/// once the test releases it, so the release orders the read after the request drops.
struct BoundaryPql {
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    observed: tokio::sync::mpsc::UnboundedSender<bool>,
}

impl PqlQueryService for BoundaryPql {
    fn execute(
        &self,
        _ast: &Ast,
        _scope: &QueryScope,
        _cursor: Option<&str>,
        cancellation: &ScanCancellation,
    ) -> Result<Page, PqlError> {
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        self.observed.send(cancellation.is_cancelled()).unwrap();
        Err(PqlError::Backend("scan stopped at a page boundary".to_owned()))
    }
}

fn resources(document: &Value) -> Vec<String> {
    document["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["resource"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn test_query_operator_reads_across_repositories() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, headers, document) = post(
        &app,
        json!({"query": "from policy.decisions order by evaluated_at desc"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(resources(&document), ["alpha", "beta", "gamma"]);
    assert_eq!(document["rows"][0]["source"], json!("alpha"));
    assert_eq!(document["rows"][0]["reason"], json!("resource is blocked"));
}

#[tokio::test]
async fn test_query_binds_parameters_out_of_band() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({
            "query": "from policy.decisions where state == :state order by evaluated_at desc",
            "params": {"state": "deny"}
        }),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resources(&document), ["alpha", "gamma"]);
}

#[rstest]
#[case::utc("1970-01-01T00:00:20Z")]
#[case::offset("1970-01-01T01:00:20+01:00")]
#[case::fractional("1970-01-01T00:00:20.999Z")]
#[tokio::test]
async fn test_query_binds_timestamp_parameters(#[case] cutoff: &str) {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({
            "query": "from policy.decisions where evaluated_at >= :cutoff order by evaluated_at desc",
            "params": {"cutoff": cutoff}
        }),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(
        (status, resources(&document)),
        (StatusCode::OK, vec!["alpha".to_owned(), "beta".to_owned()])
    );
}

#[tokio::test]
async fn test_query_keeps_integer_parameters_on_integer_columns() {
    let (_dir, _meta, metrics, app) = build(false).await;
    seed_usage(&metrics);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from usage.reads where reads == :reads", "params": {"reads": 2}}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(
        (status, resources(&document)),
        (StatusCode::OK, vec!["alpha".to_owned()])
    );
}

#[rstest]
#[case::malformed("not-a-time")]
#[case::out_of_range("10000-01-01T00:00:00Z")]
#[tokio::test]
async fn test_query_rejects_invalid_timestamp_parameters(#[case] cutoff: &str) {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({
            "query": "from policy.decisions where evaluated_at >= :cutoff",
            "params": {"cutoff": cutoff}
        }),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(
        (status, document["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("the query is not valid"))
    );
}

#[tokio::test]
async fn test_query_rejects_conflicting_parameter_contexts() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({
            "query": "from policy.decisions where evaluated_at >= :value and state == :value",
            "params": {"value": "1970-01-01T00:00:00Z"}
        }),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(
        (status, document["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("the query is not valid"))
    );
}

#[tokio::test]
async fn test_query_rejects_a_non_string_repository_parameter() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({
            "query": "from policy.decisions where repository == :repository",
            "params": {"repository": true}
        }),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(
        (status, document["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("the query is not valid"))
    );
}

#[tokio::test]
async fn test_query_rejects_a_missing_parameter() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where evaluated_at >= :cutoff"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(
        (status, document["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("a query parameter was not supplied"))
    );
}

#[tokio::test]
async fn test_query_rejects_a_null_parameter() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({
            "query": "from policy.decisions where evaluated_at >= :cutoff",
            "params": {"cutoff": null}
        }),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(
        (status, document["error"].as_str()),
        (
            StatusCode::BAD_REQUEST,
            Some("a query parameter has an unsupported type")
        )
    );
}

#[tokio::test]
async fn test_query_narrows_read_through_resource_index() {
    // A leading `resource ==` equality is the cost gate's indexed filter; the source pushes it into the
    // store's resource index rather than paging the whole domain, and the result stays exact.
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where resource == \"alpha\""}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resources(&document), ["alpha"]);
}

#[tokio::test]
async fn test_query_multi_value_resource_filter_pages_without_the_index() {
    // A multi-value `resource in (...)` is a pushdown column but not a single-equality the store's resource
    // index can serve, so the source pages the domain and the executor filters in memory; the result
    // still stays exact.
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where resource in (\"alpha\", \"beta\")"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut got = resources(&document);
    got.sort();
    assert_eq!(got, ["alpha", "beta"]);
}

#[tokio::test]
async fn test_query_aggregates_counts_by_state() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions aggregate count() as n by state"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let counts: Vec<(String, i64)> = document["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["state"].as_str().unwrap().to_owned(), row["n"].as_i64().unwrap()))
        .collect();
    assert!(counts.contains(&("deny".to_owned(), 2)));
    assert!(counts.contains(&("allow".to_owned(), 1)));
}

#[tokio::test]
async fn test_query_repository_reader_gets_operator_fields_filtered() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"private\""}),
        Some(("Rita", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-cache");
    assert_eq!(resources(&document), ["alpha", "beta"]);
    let first = &document["rows"][0];
    assert!(first.get("resource").is_some());
    assert!(first.get("source").is_none());
    assert!(first.get("reason").is_none());
    assert!(first.get("rule").is_none());
}

#[rstest]
#[case::usage_bytes_filter("from usage.reads where repository == \"private\" and bytes >= 100 select resource")]
#[case::usage_bytes_selection("from usage.reads where repository == \"private\" select resource, bytes")]
#[case::usage_bytes_order("from usage.reads where repository == \"private\" order by bytes desc")]
#[case::usage_bytes_group("from usage.reads where repository == \"private\" aggregate count() as n by bytes")]
#[case::usage_bytes_aggregate(
    "from usage.reads where repository == \"private\" aggregate sum(bytes) as total by resource"
)]
#[case::policy_source_filter(
    "from policy.decisions where repository == \"private\" and source == \"alpha\" select resource"
)]
#[case::policy_rule_filter(
    "from policy.decisions where repository == \"private\" and rule == \"blocked-resource\" select resource"
)]
#[case::policy_reason_filter(
    "from policy.decisions where repository == \"private\" and reason == \"resource is blocked\" select resource"
)]
#[case::policy_source_selection("from policy.decisions where repository == \"private\" select resource, source")]
#[case::policy_source_group("from policy.decisions where repository == \"private\" aggregate count() as n by source")]
#[tokio::test]
async fn test_query_repository_reader_cannot_name_an_operator_column(#[case] query: &str) {
    // Filtering, ordering, grouping, and aggregating on a protected column each disclose its value
    // through row presence, so the evaluator refuses the query rather than hiding the column after
    // it has shaped the page.
    let (_dir, meta, metrics, app) = build(false).await;
    seed(&meta);
    seed_usage(&metrics);
    let (status, _headers, document) = post(&app, json!({ "query": query }), Some(("Rita", PASSWORD))).await;
    assert_eq!(
        (status, &document["error"]),
        (StatusCode::BAD_REQUEST, &json!("the query is not valid"))
    );
}

#[tokio::test]
async fn test_query_operator_still_reads_protected_columns() {
    let (_dir, meta, metrics, app) = build(false).await;
    seed(&meta);
    seed_usage(&metrics);
    let (usage_status, _headers, usage) = post(
        &app,
        json!({"query": "from usage.reads where bytes >= 100 select resource, bytes order by bytes desc"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(
        (usage_status, &usage["rows"][0]["bytes"]),
        (StatusCode::OK, &json!(200))
    );
    let (policy_status, _headers, policy) = post(
        &app,
        json!({"query": "from policy.decisions where source == \"alpha\" aggregate count() as seen by rule"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!((policy_status, &policy["rows"][0]["seen"]), (StatusCode::OK, &json!(2)));
}

#[tokio::test]
async fn test_query_repository_reader_reads_the_visible_usage_columns() {
    let (_dir, _meta, metrics, app) = build(false).await;
    seed_usage(&metrics);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from usage.reads where repository == \"private\" order by reads desc"}),
        Some(("Rita", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resources(&document), ["alpha", "beta"]);
    assert_eq!(document["rows"][0]["reads"], json!(2));
    assert!(document["rows"][0].get("bytes").is_none());
}

#[tokio::test]
async fn test_query_legacy_reader_token_reads_its_repository() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"private\""}),
        Some(("external", READER_SECRET)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resources(&document), ["alpha", "beta"]);
    assert!(document["rows"][0].get("source").is_none());
}

#[tokio::test]
async fn test_query_replica_honors_classification_and_no_store() {
    let (_dir, meta, app) = app(true).await;
    seed(&meta);

    let (operator_status, operator_headers, operator_document) = post(
        &app,
        json!({"query": "from policy.decisions"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(operator_status, StatusCode::OK);
    assert_eq!(operator_headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(operator_document["rows"][0]["source"], json!("alpha"));

    let (reader_status, reader_headers, reader_document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"private\""}),
        Some(("Rita", PASSWORD)),
    )
    .await;
    assert_eq!(reader_status, StatusCode::OK);
    assert_eq!(reader_headers[header::CACHE_CONTROL], "private, no-cache");
    assert!(reader_document["rows"][0].get("source").is_none());
}

#[tokio::test]
async fn test_query_cursor_is_bound_to_scope() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (_status, _headers, page) = post(
        &app,
        json!({"query": "from policy.decisions limit 1"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    let cursor = page["next_cursor"]
        .as_str()
        .expect("operator query paginates")
        .to_owned();

    // Replaying the operator's cursor under a repository-scoped grant is refused, not re-scoped.
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"private\"", "cursor": cursor}),
        Some(("Rita", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        document["error"],
        json!("the caller's scope changed; restart the query")
    );
}

#[tokio::test]
async fn test_query_usage_domain_reads_totals() {
    let (_dir, _meta, metrics, app) = build(false).await;
    seed_usage(&metrics);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from usage.reads order by reads desc"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resources(&document), ["alpha", "gamma", "beta"]);
    assert_eq!(document["rows"][0]["reads"], json!(2));
}

#[tokio::test]
async fn test_query_join_correlates_decisions_with_usage() {
    let (_dir, meta, metrics, app) = build(false).await;
    seed(&meta);
    seed_usage(&metrics);
    let (status, headers, document) = post(
        &app,
        json!({
            "query": "from policy.decisions join usage.reads on repository, resource order by evaluated_at desc"
        }),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(resources(&document), ["alpha", "beta", "gamma"]);
    let alpha = &document["rows"][0];
    assert_eq!(alpha["resource"], json!("alpha"));
    assert_eq!(alpha["reads"], json!(2));
    assert_eq!(alpha["bytes"], json!(200));
    assert_eq!(alpha["state"], json!("deny"));
}

#[tokio::test]
async fn test_query_join_applies_most_restrictive_field_class() {
    let (_dir, meta, metrics, app) = build(false).await;
    seed(&meta);
    seed_usage(&metrics);
    let (status, headers, document) = post(
        &app,
        json!({
            "query": "from policy.decisions join usage.reads on repository, resource where repository == \"private\""
        }),
        Some(("Rita", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-cache");
    assert_eq!(resources(&document), ["alpha", "beta"]);
    let first = &document["rows"][0];
    // Repository readers receive repository-scoped fields.
    assert!(first.get("reads").is_some());
    assert!(first.get("bytes").is_none());
    assert!(first.get("source").is_none());
}

#[tokio::test]
async fn test_query_join_cursor_is_scope_bound() {
    let (_dir, meta, metrics, app) = build(false).await;
    seed(&meta);
    seed_usage(&metrics);
    let (_status, _headers, page) = post(
        &app,
        json!({"query": "from policy.decisions join usage.reads on repository, resource limit 1"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    let cursor = page["next_cursor"].as_str().expect("join paginates").to_owned();
    let (status, _headers, document) = post(
        &app,
        json!({
            "query": "from policy.decisions join usage.reads on repository, resource where repository == \"private\"",
            "cursor": cursor
        }),
        Some(("Rita", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        document["error"],
        json!("the caller's scope changed; restart the query")
    );
}

#[tokio::test]
async fn test_query_join_rejects_unknown_domain_and_key() {
    let (_dir, meta, metrics, app) = build(false).await;
    seed(&meta);
    seed_usage(&metrics);
    let (unknown_domain, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions join ghosts on repository, resource"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(unknown_domain, StatusCode::NOT_FOUND);
    let (unknown_key, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions join usage.reads on repository, evaluated_at"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(unknown_key, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_query_rejects_unknown_column() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions where nope == 1"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_query_unauthorized_and_forbidden_paths() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);

    let (anonymous, _headers, _document) = post(&app, json!({"query": "from policy.decisions"}), None).await;
    assert_eq!(anonymous, StatusCode::UNAUTHORIZED);

    // A repository reader without an operator grant cannot run an operator-wide query.
    let (no_grant, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions"}),
        Some(("Rita", PASSWORD)),
    )
    .await;
    assert_eq!(no_grant, StatusCode::NOT_FOUND);

    // Morgan may read `other`, not `private`.
    let (wrong_repo, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"private\""}),
        Some(("Morgan", PASSWORD)),
    )
    .await;
    assert_eq!(wrong_repo, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_query_local_user_cannot_select_an_unknown_repository() {
    let (_dir, _meta, app) = app(false).await;
    let (status, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"missing\""}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_query_ecosystem_credential_cannot_query_all_repositories() {
    let (_dir, _meta, app) = app(false).await;
    let (status, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions"}),
        Some(("external", READER_SECRET)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_query_ecosystem_credential_cannot_select_an_unknown_repository() {
    let (_dir, _meta, app) = app(false).await;
    let (status, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"missing\""}),
        Some(("external", READER_SECRET)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_query_rejects_bad_requests() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);

    let (unparsable, _headers, _document) =
        post(&app, json!({"query": "not a query"}), Some(("Alice", PASSWORD))).await;
    assert_eq!(unparsable, StatusCode::BAD_REQUEST);

    let (bad_param, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions where reads == :n", "params": {"n": [1, 2]}}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(bad_param, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_query_requires_json_body() {
    let (_dir, _meta, app) = app(false).await;
    let request = Request::builder()
        .method("POST")
        .uri("/+query")
        .header(header::CONTENT_TYPE, "text/plain")
        .header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("Alice:{PASSWORD}"))),
        )
        .body(Body::from("from policy.decisions"))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn test_query_rejects_invalid_body() {
    let (_dir, _meta, app) = app(false).await;
    let (status, _headers, _document) = post(&app, json!({"unknown": "field"}), Some(("Alice", PASSWORD))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_query_scopes_to_repository_equality_on_the_right_of_an_and() {
    // The repository equality that scopes the grant may sit on either side of an `and`; here it is the
    // right operand, so the resolver must recurse past the left comparison to find it.
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where state == \"deny\" and repository == \"private\""}),
        Some(("Rita", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resources(&document), ["alpha"]);
}

#[rstest]
#[case::equality_at_limit(
    format!("from policy.decisions where resource == \"{}\"", "x".repeat(512)),
    StatusCode::OK,
    None
)]
#[case::equality_over_limit(
    format!("from policy.decisions where resource == \"{}\"", "x".repeat(513)),
    StatusCode::BAD_REQUEST,
    Some("the query is not valid")
)]
#[case::membership_at_limit(
    format!("from policy.decisions where resource in (\"{}\")", "é".repeat(256)),
    StatusCode::OK,
    None
)]
#[case::membership_over_limit(
    format!("from policy.decisions where resource in (\"{}x\")", "é".repeat(256)),
    StatusCode::BAD_REQUEST,
    Some("the query is not valid")
)]
#[tokio::test]
async fn test_query_bounds_policy_resource_filter_bytes(
    #[case] query: String,
    #[case] expected_status: StatusCode,
    #[case] expected_error: Option<&str>,
) {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(&app, json!({"query": query}), Some(("Alice", PASSWORD))).await;
    assert_eq!(
        (status, document.get("error").and_then(Value::as_str)),
        (expected_status, expected_error)
    );
}

#[tokio::test]
async fn test_query_unknown_domain_is_not_found() {
    // A domain the source does not serve is answered as 404 without disclosing whether it exists.
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, _document) = post(&app, json!({"query": "from ghosts"}), Some(("Alice", PASSWORD))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_query_accepts_bool_and_int_parameters() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, _document) = post(
        &app,
        json!({
            "query": "from policy.decisions where fresh == :flag",
            "params": {"flag": true, "count": 5}
        }),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_query_body_over_the_limit_is_too_large() {
    let (_dir, _meta, app) = app(false).await;
    let (status, _headers, _document) = post(&app, json!({"query": "x".repeat(9000)}), Some(("Alice", PASSWORD))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_query_ecosystem_credential_with_wrong_secret_is_unauthorized() {
    // A ecosystem credential whose secret matches no grant identifies as anonymous, and the repository forbids
    // anonymous reads, so the caller is asked to authenticate.
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"private\""}),
        Some(("external", "wrong-secret")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[rstest]
#[case::unknown_secret("wrong-secret")]
#[case::empty_secret("")]
#[tokio::test]
async fn test_query_ecosystem_credential_with_wrong_secret_on_a_public_repository_is_unauthorized(
    #[case] secret: &str,
) {
    let (_dir, meta, app) = app(false).await;
    meta.record_policy_decision(decision("public", "alpha", PolicyDecisionState::Deny, 30))
        .unwrap();
    let (status, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"public\""}),
        Some(("external", secret)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_query_ecosystem_credential_reads_a_public_repository() {
    let (_dir, meta, app) = app(false).await;
    meta.record_policy_decision(decision("public", "alpha", PolicyDecisionState::Deny, 30))
        .unwrap();
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"public\""}),
        Some(("external", PUBLIC_SECRET)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resources(&document), vec!["alpha".to_owned()]);
}

#[tokio::test]
async fn test_query_ecosystem_credential_without_read_is_forbidden() {
    // The `locked` token authenticates but grants only writes, so a read is refused as forbidden rather
    // than unauthenticated.
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"locked\""}),
        Some(("external", NOREAD_SECRET)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_query_authentication_storage_fault_is_service_unavailable() {
    let (_dir, app) = app_authentication_storage_fault().await;
    let (status, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_query_authz_storage_fault_is_service_unavailable() {
    let (_dir, app) = app_authz_storage_fault().await;
    let (status, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_query_policy_storage_fault_is_service_unavailable() {
    let (_dir, app) = app_policy_query_storage_fault().await;
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where resource == \"alpha\""}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(
        (status, document),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "the query backend is unavailable"})
        )
    );
}

#[tokio::test]
async fn test_query_source_pages_through_the_store() {
    // With more decisions than one store page, the source must loop on the store cursor to gather them
    // all; a full page plus a next cursor proves it read past the first store page.
    let (_dir, meta, app) = app(false).await;
    seed_many(&meta, "private", 101);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"private\" limit 100"}),
        Some(("Rita", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["rows"].as_array().unwrap().len(), 100);
    assert!(document["next_cursor"].is_string());
}
