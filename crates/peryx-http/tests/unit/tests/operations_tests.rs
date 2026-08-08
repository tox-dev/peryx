use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::AppState;
use peryx_driver::users::UserService;
use peryx_identity::{GrantScope, PasswordPolicy, Role};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{MetaStore, OperationResult};
use tower::ServiceExt as _;

const USER_PASSWORD: &str = "local password";

async fn app() -> (tempfile::TempDir, Arc<AppState>) {
    build_app(false).await
}

/// Build an app with one operation of each client-facing status recorded. When `corrupt` is set, the
/// operation-outcome table is dropped and reopened with a mismatched value type, so every read fails and
/// the endpoint answers with a server error rather than a partial view.
async fn build_app(corrupt: bool) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let authorization = AuthorizationService::new(meta.clone());
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    for (name, role) in [
        ("Alice", Role::Administrator),
        ("Olivia", Role::Operator),
        ("Rita", Role::RepositoryReader),
    ] {
        let user = users.create(name).unwrap();
        users.set_password(&user.id, USER_PASSWORD).await.unwrap();
        let scope = if matches!(role, Role::RepositoryReader) {
            GrantScope::Repository {
                name: "private".to_owned(),
            }
        } else {
            GrantScope::Server
        };
        authorization.grant(&user.id, role, scope).unwrap();
    }
    drop(authorization);
    drop(users);
    // One write of each client-facing status: pending (no deadline), expired (a deadline long past any
    // real clock), published, and failed.
    meta.claim_operation("op-1", None, 1).unwrap();
    meta.claim_operation("op-2", Some(10), 1).unwrap();
    meta.claim_operation("op-3", None, 1).unwrap();
    meta.finalize_operation("op-3", OperationResult::Published, b"serial-7", 2)
        .unwrap();
    meta.claim_operation("op-4", None, 1).unwrap();
    meta.finalize_operation("op-4", OperationResult::Failed, b"quota", 2)
        .unwrap();
    drop(meta);
    if corrupt {
        let database = redb::Database::open(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .delete_table(redb::TableDefinition::<&str, &[u8]>::new("operation_outcome"))
            .unwrap();
        transaction
            .open_table(redb::TableDefinition::<&str, u64>::new("operation_outcome"))
            .unwrap();
        transaction.commit().unwrap();
    }
    let meta = MetaStore::open_existing(&path).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, Vec::new());
    state.users = UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, Arc::new(state))
}

async fn get(state: &Arc<AppState>, uri: &str, credential: Option<&str>) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().uri(uri);
    if let Some(user) = credential {
        request = request.header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("{user}:{USER_PASSWORD}"))),
        );
    }
    let response = crate::router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let no_store = response
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        == "no-store";
    assert!(no_store, "an operations response is never cached");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, value)
}

#[tokio::test]
async fn test_operations_administrator_sees_health_and_rows() {
    let (_dir, state) = app().await;

    let (status, body) = get(&state, "/+availability/operations", Some("Alice")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["captured_at"].is_number());
    assert_eq!(body["health"]["pending"], 1);
    assert_eq!(body["health"]["published"], 1);
    assert_eq!(body["health"]["failed"], 1);
    assert_eq!(
        body["health"]["expired"], 1,
        "a pending write past its deadline reads expired: {body}"
    );
    assert_eq!(body["health"]["total"], 4);
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "{body}");
    // Rows come in operation-id order, one of each client-facing status.
    assert_eq!(rows[0]["operation"], "op-1");
    assert_eq!(rows[0]["status"], "pending");
    assert_eq!(rows[1]["operation"], "op-2");
    assert_eq!(rows[1]["status"], "expired");
    assert_eq!(rows[2]["status"], "published");
    assert_eq!(rows[3]["status"], "failed");
    assert!(
        body.get("next_cursor").is_none(),
        "a full listing has no more pages: {body}"
    );
}

#[tokio::test]
async fn test_operations_operator_sees_only_aggregate_health() {
    let (_dir, state) = app().await;

    let (status, body) = get(&state, "/+availability/operations", Some("Olivia")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["health"]["total"], 4);
    assert!(
        body.get("rows").is_none(),
        "an operator reads no per-operation rows: {body}"
    );
    assert!(body.get("next_cursor").is_none(), "{body}");
}

#[tokio::test]
async fn test_operations_repository_token_is_forbidden() {
    let (_dir, state) = app().await;

    let (status, _) = get(&state, "/+availability/operations", Some("Rita")).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_operations_anonymous_is_forbidden() {
    let (_dir, state) = app().await;

    let (status, _) = get(&state, "/+availability/operations", None).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_operations_administrator_pages_by_cursor() {
    let (_dir, state) = app().await;

    let (status, first) = get(&state, "/+availability/operations?limit=2", Some("Alice")).await;
    assert_eq!(status, StatusCode::OK);
    let rows = first["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        first["next_cursor"], "op-2",
        "the cursor resumes after the last row: {first}"
    );

    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, second) = get(
        &state,
        &format!("/+availability/operations?limit=2&cursor={cursor}"),
        Some("Alice"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = second["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "two rows remain past the cursor: {second}");
    assert_eq!(rows[0]["operation"], "op-3");
    assert!(
        second.get("next_cursor").is_none(),
        "the last page has no cursor: {second}"
    );
}

#[tokio::test]
async fn test_operations_rejects_a_limit_out_of_range() {
    let (_dir, state) = app().await;

    let (status, _) = get(&state, "/+availability/operations?limit=0", Some("Alice")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_operations_rejects_an_unparseable_query() {
    let (_dir, state) = app().await;

    let (status, _) = get(&state, "/+availability/operations?limit=abc", Some("Alice")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_operations_administrator_store_fault_is_a_server_error() {
    let (_dir, state) = build_app(true).await;

    let (status, body) = get(&state, "/+availability/operations", Some("Alice")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, serde_json::json!({"error": "operation query failed"}));
}

#[tokio::test]
async fn test_operations_operator_store_fault_is_a_server_error() {
    let (_dir, state) = build_app(true).await;

    let (status, _) = get(&state, "/+availability/operations", Some("Olivia")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}
