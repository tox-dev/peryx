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
use peryx_storage::meta::{ArtifactSource, MetaStore};
use tower::ServiceExt as _;

const USER_PASSWORD: &str = "local password";

async fn app() -> (tempfile::TempDir, Arc<AppState>) {
    build_app(false).await
}

/// Build an app with one artifact of each source and each byte-availability state recorded. When
/// `corrupt` is set, the placement table is dropped and reopened with a mismatched value type, so every
/// placement read fails and the endpoint answers with a server error rather than a partial view.
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
    // A local upload, a proxied miss still reachable upstream, and a generated artifact whose bytes are
    // gone: one of each source, and one of each availability state.
    meta.record_artifact_placement("sha256:aaa", ArtifactSource::Hosted, true)
        .unwrap();
    meta.record_artifact_placement("sha256:bbb", ArtifactSource::Proxy, false)
        .unwrap();
    meta.record_artifact_placement("sha256:ccc", ArtifactSource::Generated, false)
        .unwrap();
    drop(meta);
    if corrupt {
        let database = redb::Database::open(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .delete_table(redb::TableDefinition::<&str, &[u8]>::new("artifact_placement"))
            .unwrap();
        transaction
            .open_table(redb::TableDefinition::<&str, u64>::new("artifact_placement"))
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
    assert!(no_store, "a placement response is never cached");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, value)
}

#[tokio::test]
async fn test_placements_administrator_sees_health_and_rows() {
    let (_dir, state) = app().await;

    let (status, body) = get(&state, "/+availability/placements", Some("Alice")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["captured_at"].is_number());
    assert_eq!(body["health"]["local"], 1);
    assert_eq!(body["health"]["remote_only"], 1);
    assert_eq!(body["health"]["unavailable"], 1);
    assert_eq!(body["health"]["total"], 3);
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{body}");
    // Rows come in digest order, one of each source and each availability.
    assert_eq!(rows[0]["digest"], "sha256:aaa");
    assert_eq!(rows[0]["source"], "hosted");
    assert_eq!(rows[0]["availability"], "local");
    assert_eq!(rows[1]["source"], "proxy");
    assert_eq!(rows[1]["availability"], "remote_only");
    assert_eq!(rows[2]["source"], "generated");
    assert_eq!(rows[2]["availability"], "unavailable");
    assert!(
        body.get("next_cursor").is_none(),
        "a full listing has no more pages: {body}"
    );
}

#[tokio::test]
async fn test_placements_operator_sees_only_aggregate_health() {
    let (_dir, state) = app().await;

    let (status, body) = get(&state, "/+availability/placements", Some("Olivia")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["health"]["total"], 3);
    assert!(
        body.get("rows").is_none(),
        "an operator reads no per-digest rows: {body}"
    );
    assert!(body.get("next_cursor").is_none(), "{body}");
}

#[tokio::test]
async fn test_placements_repository_token_is_forbidden() {
    let (_dir, state) = app().await;

    let (status, _) = get(&state, "/+availability/placements", Some("Rita")).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_placements_anonymous_is_forbidden() {
    let (_dir, state) = app().await;

    let (status, _) = get(&state, "/+availability/placements", None).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_placements_administrator_pages_by_cursor() {
    let (_dir, state) = app().await;

    let (status, first) = get(&state, "/+availability/placements?limit=2", Some("Alice")).await;
    assert_eq!(status, StatusCode::OK);
    let rows = first["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        first["next_cursor"], "sha256:bbb",
        "the cursor resumes after the last row: {first}"
    );

    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, second) = get(
        &state,
        &format!("/+availability/placements?limit=2&cursor={cursor}"),
        Some("Alice"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = second["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "one row remains past the cursor: {second}");
    assert_eq!(rows[0]["digest"], "sha256:ccc");
    assert!(
        second.get("next_cursor").is_none(),
        "the last page has no cursor: {second}"
    );
}

#[tokio::test]
async fn test_placements_rejects_a_limit_out_of_range() {
    let (_dir, state) = app().await;

    let (status, _) = get(&state, "/+availability/placements?limit=0", Some("Alice")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_placements_rejects_an_unparseable_query() {
    let (_dir, state) = app().await;

    let (status, _) = get(&state, "/+availability/placements?limit=abc", Some("Alice")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_placements_administrator_store_fault_is_a_server_error() {
    let (_dir, state) = build_app(true).await;

    let (status, body) = get(&state, "/+availability/placements", Some("Alice")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, serde_json::json!({"error": "placement query failed"}));
}

#[tokio::test]
async fn test_placements_operator_store_fault_is_a_server_error() {
    let (_dir, state) = build_app(true).await;

    let (status, _) = get(&state, "/+availability/placements", Some("Olivia")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}
