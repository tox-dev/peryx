use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::{Ecosystem, ShadowCandidate, ShadowSource};
use peryx_driver::authz::AuthorizationService;
use peryx_driver::shadow::ShadowQueryError;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_driver::users::UserService;
use peryx_identity::{Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, PasswordPolicy, Role};
use peryx_policy::{Policy, PolicyAction, PolicyDecisionState};
use peryx_storage::meta::{MetaStore, NewPolicyDecision};
use rstest::rstest;
use tower::ServiceExt as _;

use peryx_driver::serving::{DriverCapabilities, EcosystemDriver, ShadowDriver};

use crate::handlers::shadow_error_response;

const ADMIN_SECRET: &str = "admin-secret";
const READER_SECRET: &str = "reader-secret";
const USER_PASSWORD: &str = "local password";

#[derive(Clone, Copy)]
enum StoreFault {
    None,
    Authentication,
    Authorization,
}

struct ShadowStub;

impl ShadowDriver for ShadowStub {
    fn shadowed_candidates(
        &self,
        _state: &peryx_driver::state::ServingState,
        _position: usize,
        _project: &str,
    ) -> Result<Vec<ShadowCandidate>, String> {
        Ok(["flask-1.0.bin", "flask-2.0.bin"]
            .into_iter()
            .map(|filename| ShadowCandidate {
                repository: "private".to_owned(),
                project: "flask".to_owned(),
                member: "hosted".to_owned(),
                source: ShadowSource::Hosted,
                filename: filename.to_owned(),
                digest: Some("sha256:abc".to_owned()),
                selected: true,
                reason: None,
            })
            .collect())
    }
}

#[async_trait::async_trait]
impl EcosystemDriver for ShadowStub {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }

    fn capabilities(&self) -> DriverCapabilities<'_> {
        DriverCapabilities {
            shadow: Some(self),
            ..DriverCapabilities::default()
        }
    }

    fn classify_route(&self, _path: &str) -> peryx_driver::rate_limit::RouteClass {
        peryx_driver::rate_limit::RouteClass::Artifact
    }

    fn discover_index(
        &self,
        index: peryx_driver::state::IndexDescription,
        _base: Option<&peryx_driver::discovery::BaseUrl>,
    ) -> serde_json::Value {
        peryx_driver::discovery::minimal_entry(&index)
    }
}

async fn app() -> (tempfile::TempDir, axum::Router) {
    app_with_fault(StoreFault::None).await
}

async fn app_with_fault(fault: StoreFault) -> (tempfile::TempDir, axum::Router) {
    build_app(fault, false).await
}

async fn app_with_driver() -> (tempfile::TempDir, axum::Router) {
    build_app(StoreFault::None, true).await
}

async fn build_app(fault: StoreFault, register_driver: bool) -> (tempfile::TempDir, axum::Router) {
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
    ] {
        let user = users.create(name).unwrap();
        users.set_password(&user.id, USER_PASSWORD).await.unwrap();
        authorization.grant(&user.id, role, scope).unwrap();
    }
    drop(authorization);
    drop(users);
    drop(meta);
    if let Some(table) = match fault {
        StoreFault::None => None,
        StoreFault::Authentication => Some("server_user_verifier"),
        StoreFault::Authorization => Some("role_grant"),
    } {
        let database = redb::Database::open(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .delete_table(redb::TableDefinition::<&str, &[u8]>::new(table))
            .unwrap();
        transaction
            .open_table(redb::TableDefinition::<&str, u64>::new(table))
            .unwrap();
        transaction.commit().unwrap();
    }
    let meta = MetaStore::open_existing(path).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, vec![private_index(), read_only_index()]);
    state.users = UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    if register_driver {
        state.register_ecosystem(Arc::new(ShadowStub), Arc::new(peryx_search::EmptyIndexer));
        state
            .meta
            .record_policy_decision(NewPolicyDecision {
                repository: "private",
                project: "flask",
                version: None,
                filename: Some("flask-1.0.bin"),
                source: None,
                action: PolicyAction::Serve,
                state: PolicyDecisionState::Deny,
                rule: Some("blocked"),
                reason: Some("policy"),
                evaluated_at_unix: 42,
                next_eligible_at_unix: None,
            })
            .unwrap();
    }
    (dir, crate::router(Arc::new(state)))
}

fn private_index() -> Index {
    Index {
        name: "private".to_owned(),
        route: "private".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: true },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: vec![
                NamedToken {
                    name: "admin".to_owned(),
                    secret: ADMIN_SECRET.to_owned(),
                    grants: vec![Grant {
                        projects: vec![Glob::new("*")],
                        actions: BTreeSet::from([Action::Write]),
                    }],
                    expires_at: None,
                },
                NamedToken {
                    name: "reader".to_owned(),
                    secret: READER_SECRET.to_owned(),
                    grants: vec![Grant {
                        projects: vec![Glob::new("*")],
                        actions: BTreeSet::from([Action::Read]),
                    }],
                    expires_at: None,
                },
            ],
        },
    }
}

fn read_only_index() -> Index {
    let mut index = private_index();
    index.name = "read-only".to_owned();
    index.route = "read-only".to_owned();
    index.acl.tokens.clear();
    index
}

async fn get(
    app: &axum::Router,
    uri: &str,
    credential: Option<(&str, &str)>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut request = Request::builder().uri(uri);
    if let Some((user, password)) = credential {
        request = request.header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
        );
    }
    let response = app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn test_shadow_returns_an_empty_page_when_no_driver_resolves_the_repository() {
    let (_dir, app) = app().await;

    let (status, headers, document) = get(
        &app,
        "/+shadow/candidates?repository=private&project=flask&limit=25",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(document, serde_json::json!({"candidates": [], "next_cursor": null}));
}

#[tokio::test]
async fn test_shadow_renders_driver_candidates_with_policy_decisions() {
    let (_dir, app) = app_with_driver().await;
    let (status, _, document) = get(
        &app,
        "/+shadow/candidates?repository=private&project=flask",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["candidates"][0]["filename"], "flask-1.0.bin");
    assert_eq!(document["candidates"][0]["decision"]["rule"], "blocked");
    assert!(document["candidates"][1]["decision"].is_null());
}

#[tokio::test]
async fn test_shadow_admits_a_repository_reader_for_their_repository() {
    let (_dir, app) = app().await;

    let (status, _, document) = get(
        &app,
        "/+shadow/candidates?repository=private&project=flask",
        Some(("Rita", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(document, serde_json::json!({"candidates": [], "next_cursor": null}));
}

#[rstest]
#[case::anonymous(
    "/+shadow/candidates?repository=private&project=flask",
    None,
    StatusCode::UNAUTHORIZED
)]
#[case::write_token(
    "/+shadow/candidates?repository=private&project=flask",
    Some(("__token__", ADMIN_SECRET)),
    StatusCode::OK
)]
#[case::reader_token_lacks_write(
    "/+shadow/candidates?repository=private&project=flask",
    Some(("__token__", READER_SECRET)),
    StatusCode::FORBIDDEN
)]
#[case::unknown_repository_token(
    "/+shadow/candidates?repository=missing&project=flask",
    Some(("__token__", ADMIN_SECRET)),
    StatusCode::UNAUTHORIZED
)]
#[case::invalid_token(
    "/+shadow/candidates?repository=private&project=flask",
    Some(("__token__", "wrong-secret")),
    StatusCode::UNAUTHORIZED
)]
#[case::administrator_selects_missing_repository(
    "/+shadow/candidates?repository=missing&project=flask",
    Some(("Alice", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::reader_outside_their_repository(
    "/+shadow/candidates?repository=read-only&project=flask",
    Some(("Rita", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[tokio::test]
async fn test_shadow_authorization(
    #[case] uri: &str,
    #[case] credential: Option<(&str, &str)>,
    #[case] expected: StatusCode,
) {
    let (_dir, app) = app().await;

    let (status, _, _) = get(&app, uri, credential).await;

    assert_eq!(status, expected);
}

#[rstest]
#[case::missing_project("/+shadow/candidates?repository=private")]
#[case::missing_repository("/+shadow/candidates?project=flask")]
#[case::non_numeric_limit("/+shadow/candidates?repository=private&project=flask&limit=abc")]
#[tokio::test]
async fn test_shadow_rejects_unparseable_queries(#[case] uri: &str) {
    let (_dir, app) = app().await;

    let (status, _, document) = get(&app, uri, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(document["error"], "invalid shadow query");
}

#[rstest]
#[case::bad_limit("limit=500", "limit must be between 1 and 100")]
#[case::bad_cursor("cursor=", "invalid shadow cursor")]
#[tokio::test]
async fn test_shadow_reports_query_layer_errors(#[case] filter: &str, #[case] message: &str) {
    let (_dir, app) = app().await;

    let (status, _, document) = get(
        &app,
        &format!("/+shadow/candidates?repository=private&project=flask&{filter}"),
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(document["error"], message);
}

#[tokio::test]
async fn test_shadow_rejects_an_oversized_project() {
    let (_dir, app) = app().await;
    let project = "p".repeat(513);

    let (status, _, document) = get(
        &app,
        &format!("/+shadow/candidates?repository=private&project={project}"),
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(document["error"], "project filter exceeds 512 bytes");
}

#[rstest]
#[case::authentication(StoreFault::Authentication, StatusCode::SERVICE_UNAVAILABLE)]
#[case::authorization(StoreFault::Authorization, StatusCode::SERVICE_UNAVAILABLE)]
#[tokio::test]
async fn test_shadow_surfaces_store_faults(#[case] fault: StoreFault, #[case] expected: StatusCode) {
    let (_dir, app) = app_with_fault(fault).await;

    let (status, _, _) = get(
        &app,
        "/+shadow/candidates?repository=private&project=flask",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, expected);
}

#[rstest]
#[case::limit(ShadowQueryError::InvalidLimit, StatusCode::BAD_REQUEST)]
#[case::cursor(ShadowQueryError::InvalidCursor, StatusCode::BAD_REQUEST)]
#[case::project(ShadowQueryError::ProjectTooLong, StatusCode::BAD_REQUEST)]
#[case::store(ShadowQueryError::Store("boom".to_owned()), StatusCode::INTERNAL_SERVER_ERROR)]
fn test_shadow_error_response_maps_each_variant(#[case] error: ShadowQueryError, #[case] expected: StatusCode) {
    let response = shadow_error_response(&error);

    assert_eq!(response.status(), expected);
}
