use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::Ecosystem;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_driver::users::UserService;
use peryx_identity::{GrantScope, IndexAcl, PasswordPolicy, Role};
use peryx_policy::Policy;
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

const OPERATOR: &str = "Olivia";
const READER: &str = "Rita";
const USER_PASSWORD: &str = "local password";

async fn app() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let authorization = AuthorizationService::new(meta.clone());
    for (name, role) in [(OPERATOR, Role::Operator), (READER, Role::RepositoryReader)] {
        let user = users.create(name).unwrap();
        users.set_password(&user.id, USER_PASSWORD).await.unwrap();
        authorization.grant(&user.id, role, GrantScope::Server).unwrap();
    }
    drop(authorization);
    drop(users);
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, vec![public_index()]);
    super::support::register_example_driver(&mut state);
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, Arc::new(state))
}

fn public_index() -> Index {
    Index {
        name: "public".to_owned(),
        route: "public".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: Vec::new(),
        },
    }
}

async fn get(state: &Arc<AppState>, uri: &str, authorization: Option<&str>) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().uri(uri);
    if let Some(value) = authorization {
        request = request.header(header::AUTHORIZATION, value);
    }
    let response = crate::router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
}

fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
}

#[rstest]
#[case::global("/+search?q=re%3Awidget")]
#[case::scoped("/public/+search?q=re%3Awidget")]
#[tokio::test]
async fn test_pattern_search_needs_operator_authority(#[case] uri: &str) {
    let (_dir, state) = app().await;

    let (status, body) = get(&state, uri, None).await;

    assert_eq!(
        (status, body["error"].as_str()),
        (
            StatusCode::FORBIDDEN,
            Some("pattern search requires operator authority")
        )
    );
}

#[rstest]
#[case::global("/+search?q=re%3Awidget")]
#[case::scoped("/public/+search?q=re%3Awidget")]
#[tokio::test]
async fn test_an_operator_may_run_a_pattern_search(#[case] uri: &str) {
    let (_dir, state) = app().await;

    let (status, body) = get(&state, uri, Some(&basic(OPERATOR, USER_PASSWORD))).await;

    assert_eq!((status, body["total"].as_u64()), (StatusCode::OK, Some(0)));
}

#[rstest]
#[case::no_credential(None)]
#[case::unsupported_scheme(Some("Bearer opaque"))]
#[case::wrong_password(Some("Basic T2xpdmlhOndyb25n"))]
#[tokio::test]
async fn test_a_credential_that_resolves_to_nobody_carries_no_pattern_authority(#[case] authorization: Option<&str>) {
    let (_dir, state) = app().await;

    let (status, _body) = get(&state, "/+search?q=re%3Awidget", authorization).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_a_non_operator_account_carries_no_pattern_authority() {
    let (_dir, state) = app().await;

    let (status, _body) = get(&state, "/+search?q=re%3Awidget", Some(&basic(READER, USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_a_single_character_query_is_a_bad_request() {
    let (_dir, state) = app().await;

    let (status, body) = get(&state, "/+search?q=a", None).await;

    assert_eq!(
        (status, body["error"].as_str()),
        (
            StatusCode::BAD_REQUEST,
            Some("search query must be at least 2 characters")
        )
    );
}

#[tokio::test]
async fn test_substring_search_answers_without_a_credential() {
    let (_dir, state) = app().await;

    let (status, body) = get(&state, "/+search?q=widget", None).await;

    assert_eq!((status, body["total"].as_u64()), (StatusCode::OK, Some(0)));
}
