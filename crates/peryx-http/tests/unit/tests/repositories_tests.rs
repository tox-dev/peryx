use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::http_services::{HttpDomainServices, NewRepository, StoreServices};
use peryx_driver::state::AppState;
use peryx_driver::users::UserService;
use peryx_identity::{GrantScope, PasswordPolicy, Role};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN_PASSWORD: &str = "administrator password";
const OPERATOR_PASSWORD: &str = "operator password";
const ADMIN: (&str, &str) = ("Alice", ADMIN_PASSWORD);
const OPERATOR: (&str, &str) = ("Olivia", OPERATOR_PASSWORD);

#[derive(Clone, Copy, PartialEq, Eq)]
enum StoreFault {
    None,
    Authentication,
    Repositories,
}

struct Fixture {
    _dir: tempfile::TempDir,
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
        match fault {
            StoreFault::None => {}
            StoreFault::Authentication => corrupt_table(&path, "server_user_verifier"),
            StoreFault::Repositories => corrupt_table(&path, "repository"),
        }
        let meta = MetaStore::open_existing(&path).unwrap();
        let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
        let mut state = AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| 42));
        Arc::get_mut(&mut state.serving).unwrap().users = users;
        Self {
            _dir: dir,
            app: crate::router(Arc::new(state)),
        }
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        credential: Option<(&str, &str)>,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        self.raw(
            method,
            uri,
            credential,
            body.map(|v| serde_json::to_vec(&v).unwrap()),
            Some("application/json"),
        )
        .await
    }

    async fn raw(
        &self,
        method: Method,
        uri: &str,
        credential: Option<(&str, &str)>,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some((user, password)) = credential {
            request = request.header(
                header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
            );
        }
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let body = body.map_or_else(Body::empty, Body::from);
        let response = self.app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn create(&self, route: &str) -> Value {
        let (status, _, body) = self
            .send(
                Method::POST,
                "/+repositories",
                Some(ADMIN),
                Some(json!({
                    "route": route, "display_name": "A repo", "ecosystem": "alpha", "definition": {}
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body
    }
}

/// Delete a redb table and reopen it under a mismatched value type, so the store's typed access fails.
fn corrupt_table(path: &std::path::Path, table: &str) {
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.delete_table(redb::TableDefinition::<&str, &[u8]>::new(table))
        .unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new(table)).unwrap();
    txn.commit().unwrap();
}

fn etag(headers: &HeaderMap) -> String {
    headers[header::ETAG].to_str().unwrap().to_owned()
}

#[tokio::test]
async fn test_repository_routes_use_the_injected_service() {
    let auth_dir = tempfile::tempdir().unwrap();
    let auth_meta = MetaStore::open(auth_dir.path().join("auth.redb")).unwrap();
    let users = UserService::with_password_settings(auth_meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let administrator = users.create("Alice").unwrap().id;
    users.set_password(&administrator, ADMIN_PASSWORD).await.unwrap();
    AuthorizationService::new(auth_meta.clone())
        .grant(&administrator, Role::Administrator, GrantScope::Server)
        .unwrap();
    let mut state = AppState::with_clock(
        auth_meta,
        peryx_storage::blob::BlobStore::new(auth_dir.path().join("blobs")),
        60,
        Vec::new(),
        Arc::new(|| 42),
    );
    Arc::get_mut(&mut state.serving).unwrap().users = users;

    let repository_dir = tempfile::tempdir().unwrap();
    let repository_meta = MetaStore::open(repository_dir.path().join("repositories.redb")).unwrap();
    repository_meta
        .create_repository(
            NewRepository {
                route: "injected/source".to_owned(),
                display_name: "Injected source".to_owned(),
                ecosystem: "neutral".to_owned(),
                definition: Value::Null,
                created_by: administrator,
            },
            41,
        )
        .unwrap();
    let state = Arc::new(state);
    let services =
        HttpDomainServices::for_state(&state).with_repositories(Arc::new(StoreServices::new(repository_meta)));
    let response = crate::router_with_services(state, services)
        .oneshot(
            Request::builder()
                .uri("/+repositories")
                .header(
                    header::AUTHORIZATION,
                    format!("Basic {}", STANDARD.encode(format!("{}:{}", ADMIN.0, ADMIN.1))),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert_eq!(body["repositories"][0]["route"], "injected/source");
}

#[tokio::test]
async fn test_full_lifecycle_create_inspect_list_update_disable_enable() {
    let fixture = Fixture::new().await;

    let created = fixture.create("root/alpha").await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["route"], "root/alpha");
    assert_eq!(created["version"], 1);
    assert_eq!(created["state"], "enabled");

    let (status, headers, body) = fixture
        .send(Method::GET, &format!("/+repositories/{id}"), Some(ADMIN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etag(&headers), "\"1\"");
    assert_eq!(body["id"], id);

    let (status, _, list) = fixture.send(Method::GET, "/+repositories", Some(ADMIN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["repositories"].as_array().unwrap().len(), 1);

    let (status, headers, updated) = fixture
        .raw(
            Method::PUT,
            &format!("/+repositories/{id}"),
            Some(ADMIN),
            Some(serde_json::to_vec(&json!({"display_name": "Renamed", "definition": {"k": 1}})).unwrap()),
            Some("application/json"),
        )
        .await;
    // no If-Match yet
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED, "{updated}");
    let _ = headers;

    let (status, headers, updated) = fixture
        .if_match_put(&id, "\"1\"", json!({"display_name": "Renamed", "definition": {}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["display_name"], "Renamed");
    assert_eq!(updated["version"], 2);
    assert_eq!(etag(&headers), "\"2\"");

    let (status, _, disabled) = fixture
        .if_match_post(&format!("/+repositories/{id}/disable"), "\"2\"")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disabled["state"], "disabled");
    assert_eq!(disabled["version"], 3);

    let (status, _, enabled) = fixture
        .if_match_post(&format!("/+repositories/{id}/enable"), "\"3\"")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(enabled["state"], "enabled");
}

impl Fixture {
    async fn if_match_put(&self, id: &str, if_match: &str, body: Value) -> (StatusCode, HeaderMap, Value) {
        let request = Request::builder()
            .method(Method::PUT)
            .uri(format!("/+repositories/{id}"))
            .header(
                header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{}:{}", ADMIN.0, ADMIN.1))),
            )
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::IF_MATCH, if_match)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        self.run(request).await
    }

    async fn if_match_post(&self, uri: &str, if_match: &str) -> (StatusCode, HeaderMap, Value) {
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(
                header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{}:{}", ADMIN.0, ADMIN.1))),
            )
            .header(header::IF_MATCH, if_match)
            .body(Body::empty())
            .unwrap();
        self.run(request).await
    }

    async fn run(&self, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn test_create_rejects_a_duplicate_route() {
    let fixture = Fixture::new().await;
    fixture.create("root/alpha").await;
    let (status, _, body) = fixture
        .send(
            Method::POST,
            "/+repositories",
            Some(ADMIN),
            Some(json!({
                "route": "root/alpha", "display_name": "Other", "ecosystem": "beta", "definition": {}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[rstest]
#[case::empty_route(json!({"route": "", "display_name": "n", "ecosystem": "alpha", "definition": {}}), "route must not be empty")]
#[case::long_route(json!({"route": "x".repeat(600), "display_name": "n", "ecosystem": "alpha", "definition": {}}), "route is too long")]
#[case::empty_name(json!({"route": "r", "display_name": "", "ecosystem": "alpha", "definition": {}}), "display name must not be empty")]
#[case::long_name(json!({"route": "r", "display_name": "x".repeat(300), "ecosystem": "alpha", "definition": {}}), "display name is too long")]
#[case::empty_ecosystem(json!({"route": "r", "display_name": "n", "ecosystem": "", "definition": {}}), "ecosystem must not be empty")]
#[case::long_ecosystem(json!({"route": "r", "display_name": "n", "ecosystem": "x".repeat(100), "definition": {}}), "ecosystem is too long")]
#[tokio::test]
async fn test_create_rejects_each_invalid_field(#[case] body: Value, #[case] message: &str) {
    let fixture = Fixture::new().await;
    let (status, _, response) = fixture
        .send(Method::POST, "/+repositories", Some(ADMIN), Some(body))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response["error"], message);
}

#[tokio::test]
async fn test_create_requires_administrator_credentials() {
    let fixture = Fixture::new().await;
    let anon = fixture
        .send(Method::POST, "/+repositories", None, Some(json!({"route": "r"})))
        .await;
    assert_eq!(anon.0, StatusCode::UNAUTHORIZED);
    let wrong = fixture
        .send(
            Method::POST,
            "/+repositories",
            Some(("Alice", "nope")),
            Some(json!({"route": "r"})),
        )
        .await;
    assert_eq!(wrong.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_a_non_administrator_cannot_distinguish_missing_from_forbidden() {
    let fixture = Fixture::new().await;
    let created = fixture.create("root/alpha").await;
    let id = created["id"].as_str().unwrap();
    // The operator lacks administration authority: an existing repo and a missing one both read 404.
    let existing = fixture
        .send(Method::GET, &format!("/+repositories/{id}"), Some(OPERATOR), None)
        .await;
    let missing = fixture
        .send(Method::GET, "/+repositories/repo_absent", Some(OPERATOR), None)
        .await;
    assert_eq!(existing.0, StatusCode::NOT_FOUND);
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_create_rejects_a_non_json_or_malformed_body() {
    let fixture = Fixture::new().await;
    let wrong_type = fixture
        .raw(
            Method::POST,
            "/+repositories",
            Some(ADMIN),
            Some(b"{}".to_vec()),
            Some("text/plain"),
        )
        .await;
    assert_eq!(wrong_type.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let malformed = fixture
        .raw(
            Method::POST,
            "/+repositories",
            Some(ADMIN),
            Some(b"{not json".to_vec()),
            Some("application/json"),
        )
        .await;
    assert_eq!(malformed.0, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_create_rejects_an_oversized_body() {
    let fixture = Fixture::new().await;
    let big =
        json!({"route": "r", "display_name": "n", "ecosystem": "alpha", "definition": {"pad": "x".repeat(70_000)}});
    let (status, _, _) = fixture
        .raw(
            Method::POST,
            "/+repositories",
            Some(ADMIN),
            Some(serde_json::to_vec(&big).unwrap()),
            Some("application/json"),
        )
        .await;
    // The route body limit rejects it before the handler; either 413 is acceptable.
    assert!(status == StatusCode::PAYLOAD_TOO_LARGE, "{status}");
}

#[tokio::test]
async fn test_inspect_and_list_report_missing_and_bad_limit() {
    let fixture = Fixture::new().await;
    let missing = fixture
        .send(Method::GET, "/+repositories/repo_absent", Some(ADMIN), None)
        .await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    let bad_limit = fixture
        .send(Method::GET, "/+repositories?limit=0", Some(ADMIN), None)
        .await;
    assert_eq!(bad_limit.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_list_filters_by_state_and_paginates() {
    let fixture = Fixture::new().await;
    let a = fixture.create("a").await;
    fixture.create("b").await;
    fixture
        .if_match_post(
            &format!("/+repositories/{}/disable", a["id"].as_str().unwrap()),
            "\"1\"",
        )
        .await;

    let (_, _, disabled) = fixture
        .send(Method::GET, "/+repositories?state=disabled", Some(ADMIN), None)
        .await;
    assert_eq!(disabled["repositories"].as_array().unwrap().len(), 1);

    let (_, _, first) = fixture
        .send(Method::GET, "/+repositories?limit=1", Some(ADMIN), None)
        .await;
    assert_eq!(first["repositories"].as_array().unwrap().len(), 1);
    let cursor = first["next_cursor"].as_str().unwrap();
    let (_, _, second) = fixture
        .send(
            Method::GET,
            &format!("/+repositories?limit=1&cursor={cursor}"),
            Some(ADMIN),
            None,
        )
        .await;
    assert_eq!(second["repositories"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_update_enforces_the_if_match_precondition() {
    let fixture = Fixture::new().await;
    let id = fixture.create("root/alpha").await["id"].as_str().unwrap().to_owned();

    let bad = fixture
        .if_match_put(&id, "not-a-version", json!({"display_name": "x", "definition": {}}))
        .await;
    assert_eq!(bad.0, StatusCode::BAD_REQUEST);

    let conflict = fixture
        .if_match_put(&id, "\"9\"", json!({"display_name": "x", "definition": {}}))
        .await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert_eq!(conflict.2["current_version"], 1);

    let missing = fixture
        .if_match_put("repo_absent", "\"1\"", json!({"display_name": "x", "definition": {}}))
        .await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);

    let field = fixture
        .if_match_put(&id, "\"1\"", json!({"display_name": "", "definition": {}}))
        .await;
    assert_eq!(field.0, StatusCode::UNPROCESSABLE_ENTITY);

    let non_json = self_non_json_put(&fixture, &id).await;
    assert_eq!(non_json, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

async fn self_non_json_put(fixture: &Fixture, id: &str) -> StatusCode {
    let request = Request::builder()
        .method(Method::PUT)
        .uri(format!("/+repositories/{id}"))
        .header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("{}:{}", ADMIN.0, ADMIN.1))),
        )
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::IF_MATCH, "\"1\"")
        .body(Body::from("x"))
        .unwrap();
    fixture.run(request).await.0
}

#[tokio::test]
async fn test_disable_requires_if_match_and_conflicts_on_a_stale_version() {
    let fixture = Fixture::new().await;
    let id = fixture.create("root/alpha").await["id"].as_str().unwrap().to_owned();

    let (no_precondition, _, _) = fixture
        .run(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/+repositories/{id}/disable"))
                .header(
                    header::AUTHORIZATION,
                    format!("Basic {}", STANDARD.encode(format!("{}:{}", ADMIN.0, ADMIN.1))),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(no_precondition, StatusCode::PRECONDITION_REQUIRED);

    let conflict = fixture
        .if_match_post(&format!("/+repositories/{id}/disable"), "\"9\"")
        .await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);

    let missing = fixture
        .if_match_post("/+repositories/repo_absent/disable", "\"1\"")
        .await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_disable_is_idempotent() {
    let fixture = Fixture::new().await;
    let id = fixture.create("root/alpha").await["id"].as_str().unwrap().to_owned();
    let first = fixture
        .if_match_post(&format!("/+repositories/{id}/disable"), "\"1\"")
        .await;
    assert_eq!(first.2["version"], 2);
    let again = fixture
        .if_match_post(&format!("/+repositories/{id}/disable"), "\"2\"")
        .await;
    assert_eq!(again.0, StatusCode::OK);
    assert_eq!(again.2["version"], 2);
}

#[tokio::test]
async fn test_mutations_require_administrator_authority() {
    let fixture = Fixture::new().await;
    let id = fixture.create("root/alpha").await["id"].as_str().unwrap().to_owned();

    // The operator lacks administration authority, so an update it authors reads as a 404.
    let update = fixture
        .run(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/+repositories/{id}"))
                .header(
                    header::AUTHORIZATION,
                    format!("Basic {}", STANDARD.encode(format!("{}:{}", OPERATOR.0, OPERATOR.1))),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, "\"1\"")
                .body(Body::from(
                    serde_json::to_vec(&json!({"display_name": "x", "definition": {}})).unwrap(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(update.0, StatusCode::NOT_FOUND);

    // A disable with no credentials is challenged.
    let disable = fixture
        .run(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/+repositories/{id}/disable"))
                .header(header::IF_MATCH, "\"1\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(disable.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_an_authentication_store_failure_is_unavailable() {
    let fixture = Fixture::with_fault(StoreFault::Authentication).await;
    let (status, _, _) = fixture.send(Method::GET, "/+repositories", Some(ADMIN), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_a_repository_store_failure_is_unavailable() {
    let fixture = Fixture::with_fault(StoreFault::Repositories).await;
    let create = fixture
        .send(
            Method::POST,
            "/+repositories",
            Some(ADMIN),
            Some(json!({
                "route": "r", "display_name": "n", "ecosystem": "alpha", "definition": {}
            })),
        )
        .await;
    assert_eq!(create.0, StatusCode::SERVICE_UNAVAILABLE);
    let list = fixture.send(Method::GET, "/+repositories", Some(ADMIN), None).await;
    assert_eq!(list.0, StatusCode::SERVICE_UNAVAILABLE);
    let inspect = fixture
        .send(Method::GET, "/+repositories/repo_x", Some(ADMIN), None)
        .await;
    assert_eq!(inspect.0, StatusCode::SERVICE_UNAVAILABLE);
    let update = fixture
        .if_match_put("repo_x", "\"1\"", json!({"display_name": "n", "definition": {}}))
        .await;
    assert_eq!(update.0, StatusCode::SERVICE_UNAVAILABLE);
    let disable = fixture.if_match_post("/+repositories/repo_x/disable", "\"1\"").await;
    assert_eq!(disable.0, StatusCode::SERVICE_UNAVAILABLE);
}
