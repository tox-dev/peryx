use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::{Ecosystem, TrashRecord};
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_driver::trash::TrashQueryError;
use peryx_driver::users::UserService;
use peryx_identity::{Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, PasswordPolicy, Role};
use peryx_policy::Policy;
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

use peryx_driver::serving::{DriverCapabilities, EcosystemDriver, TrashDriver};

use crate::handlers::trash_error_response;

const ADMIN_SECRET: &str = "admin-secret";
const READER_SECRET: &str = "reader-secret";
const USER_PASSWORD: &str = "local password";

#[derive(Clone, Copy)]
enum StoreFault {
    None,
    Authentication,
    Authorization,
}

struct TrashStub {
    error: bool,
}

impl TrashDriver for TrashStub {
    fn trash_records(&self, _meta: &MetaStore, index_names: &[String]) -> Result<Vec<TrashRecord>, String> {
        if self.error {
            return Err("trash unavailable".to_owned());
        }
        Ok(index_names
            .iter()
            .map(|repository| TrashRecord {
                ecosystem: Ecosystem::new("example"),
                repository: repository.clone(),
                name: "flask".to_owned(),
                reference: Some("flask-1.0.bin".to_owned()),
                digest: Some("sha256:abc".to_owned()),
                reason: Some("replaced".to_owned()),
                actor: Some("Alice".to_owned()),
                deleted_at_unix: 1_000,
                retained: true,
            })
            .collect())
    }
}

#[async_trait::async_trait]
impl EcosystemDriver for TrashStub {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }

    fn capabilities(&self) -> DriverCapabilities<'_> {
        DriverCapabilities {
            trash: Some(self),
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
    build_app(fault, None).await
}

async fn app_with_driver() -> (tempfile::TempDir, axum::Router) {
    build_app(StoreFault::None, Some(false)).await
}

async fn app_with_error_driver() -> (tempfile::TempDir, axum::Router) {
    build_app(StoreFault::None, Some(true)).await
}

async fn build_app(fault: StoreFault, driver_error: Option<bool>) -> (tempfile::TempDir, axum::Router) {
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
    if let Some(error) = driver_error {
        state.register_ecosystem(Arc::new(TrashStub { error }), Arc::new(peryx_search::EmptyIndexer));
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
async fn test_trash_list_returns_an_empty_page_for_an_authorized_administrator() {
    let (_dir, app) = app().await;

    let (status, headers, document) = get(
        &app,
        "/+trash?ecosystem=alpha&state=restorable&deadline_before=1000&limit=25",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(document, serde_json::json!({"trash": [], "next_cursor": null}));
}

#[tokio::test]
async fn test_trash_list_and_record_render_driver_records() {
    let (_dir, app) = app_with_driver().await;
    let (status, _, page) = get(&app, "/+trash?repository=private", Some(("Alice", USER_PASSWORD))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["trash"][0]["name"], "flask");
    assert_eq!(page["trash"][0]["actor"], "Alice");

    let (status, _, record) = get(
        &app,
        "/+trash/record?ecosystem=example&repository=private&name=flask&reference=flask-1.0.bin&digest=sha256%3Aabc",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(record["record"]["reference"], "flask-1.0.bin");
}

#[tokio::test]
async fn test_trash_record_reports_driver_failures() {
    let (_dir, app) = app_with_error_driver().await;
    let (status, _, _) = get(
        &app,
        "/+trash/record?ecosystem=example&repository=private&name=flask",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_trash_list_admits_a_repository_reader_for_their_repository() {
    let (_dir, app) = app().await;

    let (status, document) = {
        let (status, _, document) = get(&app, "/+trash?repository=private", Some(("Rita", USER_PASSWORD))).await;
        (status, document)
    };

    assert_eq!(status, StatusCode::OK);
    assert_eq!(document, serde_json::json!({"trash": [], "next_cursor": null}));
}

#[rstest]
#[case::anonymous("/+trash?repository=private", None, StatusCode::UNAUTHORIZED)]
#[case::write_token("/+trash?repository=private", Some(("__token__", ADMIN_SECRET)), StatusCode::OK)]
#[case::token_without_repository("/+trash", Some(("__token__", ADMIN_SECRET)), StatusCode::UNAUTHORIZED)]
#[case::reader_token_lacks_write(
    "/+trash?repository=private",
    Some(("__token__", READER_SECRET)),
    StatusCode::FORBIDDEN
)]
#[case::unknown_repository_token("/+trash?repository=missing", Some(("__token__", ADMIN_SECRET)), StatusCode::UNAUTHORIZED)]
#[case::invalid_token("/+trash?repository=private", Some(("__token__", "wrong-secret")), StatusCode::UNAUTHORIZED)]
#[case::administrator_selects_missing_repository(
    "/+trash?repository=missing",
    Some(("Alice", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::repository_reader_without_admin_scope("/+trash", Some(("Rita", USER_PASSWORD)), StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_trash_list_authorization(
    #[case] uri: &str,
    #[case] credential: Option<(&str, &str)>,
    #[case] expected: StatusCode,
) {
    let (_dir, app) = app().await;

    let (status, _, _) = get(&app, uri, credential).await;

    assert_eq!(status, expected);
}

#[rstest]
#[case::unknown_state("/+trash?state=gone")]
#[case::non_numeric_limit("/+trash?limit=abc")]
#[tokio::test]
async fn test_trash_list_rejects_unparseable_filters(#[case] uri: &str) {
    let (_dir, app) = app().await;

    let (status, _, document) = get(&app, uri, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(document["error"], "invalid trash query");
}

#[rstest]
#[case::bad_limit("/+trash?limit=500", "limit must be between 1 and 100")]
#[case::bad_cursor("/+trash?cursor=", "invalid trash cursor")]
#[tokio::test]
async fn test_trash_list_reports_query_layer_errors(#[case] uri: &str, #[case] message: &str) {
    let (_dir, app) = app().await;

    let (status, _, document) = get(&app, uri, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(document["error"], message);
}

#[rstest]
#[case::authentication(StoreFault::Authentication, StatusCode::SERVICE_UNAVAILABLE)]
#[case::authorization(StoreFault::Authorization, StatusCode::SERVICE_UNAVAILABLE)]
#[tokio::test]
async fn test_trash_list_surfaces_store_faults(#[case] fault: StoreFault, #[case] expected: StatusCode) {
    let (_dir, app) = app_with_fault(fault).await;

    let (status, _, _) = get(&app, "/+trash", Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, expected);
}

#[tokio::test]
async fn test_trash_record_rejects_an_invalid_ecosystem() {
    let (_dir, app) = app().await;

    let (status, _, document) = get(
        &app,
        "/+trash/record?ecosystem=%20&repository=private&name=demo",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(document["error"], "invalid trash query");
}

#[tokio::test]
async fn test_trash_inspect_returns_not_found_for_an_absent_record() {
    let (_dir, app) = app().await;

    let (status, headers, _) = get(
        &app,
        "/+trash/record?ecosystem=alpha&repository=private&name=flask",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn test_trash_inspect_rejects_anonymous() {
    let (_dir, app) = app().await;
    let (status, _, _) = get(
        &app,
        "/+trash/record?ecosystem=alpha&repository=private&name=flask",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_trash_accepts_unregistered_ecosystems() {
    let (_dir, app) = app().await;
    let (list_status, _, document) = get(&app, "/+trash?ecosystem=unregistered", Some(("Alice", USER_PASSWORD))).await;
    let (inspect_status, _, _) = get(
        &app,
        "/+trash/record?ecosystem=unregistered&repository=private&name=flask",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(list_status, StatusCode::OK);
    assert_eq!(document, serde_json::json!({"trash": [], "next_cursor": null}));
    assert_eq!(inspect_status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_trash_inspect_rejects_a_query_missing_the_required_name() {
    let (_dir, app) = app().await;

    let (status, _, document) = get(
        &app,
        "/+trash/record?ecosystem=alpha&repository=private",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(document["error"], "invalid trash query");
}

#[tokio::test]
async fn test_trash_inspect_forbids_a_repository_token_without_write() {
    let (_dir, app) = app().await;

    let (status, _, _) = get(
        &app,
        "/+trash/record?ecosystem=alpha&repository=private&name=flask",
        Some(("__token__", READER_SECRET)),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[rstest]
#[case::limit(TrashQueryError::InvalidLimit, StatusCode::BAD_REQUEST)]
#[case::cursor(TrashQueryError::InvalidCursor, StatusCode::BAD_REQUEST)]
#[case::repository(TrashQueryError::RepositoryTooLong, StatusCode::BAD_REQUEST)]
#[case::store(TrashQueryError::Store("boom".to_owned()), StatusCode::INTERNAL_SERVER_ERROR)]
fn test_trash_error_response_maps_each_variant(#[case] error: TrashQueryError, #[case] expected: StatusCode) {
    let response = trash_error_response(&error);

    assert_eq!(response.status(), expected);
}
