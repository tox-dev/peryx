use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::Ecosystem;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_driver::users::UserService;
use peryx_identity::{Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, PasswordPolicy, Role};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::meta::{AccountingClass, MetaStore, NewQuotaReservation, QuotaLimits};
use rstest::rstest;
use tower::ServiceExt as _;

const READER_SECRET: &str = "reader-secret";
const ADMIN_SECRET: &str = "admin-secret";
const USER_PASSWORD: &str = "local password";

#[derive(Clone, Copy)]
enum StoreFault {
    None,
    Authentication,
    Authorization,
    Quota,
}

async fn app() -> (tempfile::TempDir, Arc<AppState>) {
    app_with_fault(StoreFault::None).await
}

async fn app_with_fault(fault: StoreFault) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let authorization = AuthorizationService::new(meta.clone());
    for (name, role, scope) in [
        ("Alice", Role::Administrator, GrantScope::Server),
        ("Olivia", Role::Operator, GrantScope::Server),
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
        StoreFault::Quota => Some("quota_usage"),
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
    if matches!(fault, StoreFault::None) {
        seed_quota(&meta);
    }
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, indexes());
    state.users = UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, Arc::new(state))
}

/// Settle 3000 committed and 500 reserved accounted bytes across two projects of the `private` store,
/// keyed by its index name rather than its route, so a status read must map one to the other.
fn seed_quota(meta: &MetaStore) {
    let committed = meta
        .reserve_quota(
            NewQuotaReservation {
                repository: "private",
                project: Some("flask"),
                version: Some("3.0"),
                digest: "sha256:aaaa",
                bytes: 3000,
                class: AccountingClass::Hosted,
                created_at_unix: 0,
            },
            QuotaLimits::default(),
        )
        .unwrap();
    meta.commit_quota_reservation(committed.id).unwrap();
    meta.reserve_quota(
        NewQuotaReservation {
            repository: "private",
            project: Some("django"),
            version: Some("5.0"),
            digest: "sha256:bbbb",
            bytes: 500,
            class: AccountingClass::Hosted,
            created_at_unix: 0,
        },
        QuotaLimits::default(),
    )
    .unwrap();
}

fn indexes() -> Vec<Index> {
    vec![
        Index {
            name: "private".to_owned(),
            route: "root/private".to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind: IndexKind::Hosted { volatile: false },
            policy: limited_policy(),
            acl: IndexAcl {
                anonymous_read: false,
                tokens: vec![
                    token("reader", READER_SECRET, Action::Read),
                    token("admin", ADMIN_SECRET, Action::Write),
                ],
            },
        },
        unlimited("other", "other"),
        unlimited("public", "public"),
    ]
}

fn unlimited(name: &str, route: &str) -> Index {
    Index {
        name: name.to_owned(),
        route: route.to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: false,
            tokens: Vec::new(),
        },
    }
}

fn limited_policy() -> Policy {
    Policy::compile(
        &PolicyConfig {
            max_file_size_bytes: Some(1024),
            max_accounted_bytes: Some(10_000),
            max_projects: Some(5),
            max_versions_per_project: Some(20),
            ..PolicyConfig::default()
        },
        str::to_owned,
    )
}

fn token(name: &str, secret: &str, action: Action) -> NamedToken {
    NamedToken {
        name: name.to_owned(),
        secret: secret.to_owned(),
        grants: vec![Grant {
            projects: vec![Glob::new("*")],
            actions: BTreeSet::from([action]),
        }],
        expires_at: None,
    }
}

async fn get(
    state: &Arc<AppState>,
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
    let response = crate::router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
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
async fn test_summary_reports_counters_limits_and_paginates() {
    let (_dir, state) = app().await;

    let (status, headers, page1) = get(&state, "/+quota?limit=2", Some(("Alice", USER_PASSWORD))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-cache");
    let repositories = page1["repositories"].as_array().unwrap();
    assert_eq!(repositories.len(), 2);
    let private = &repositories[0];
    assert_eq!(private["repository"], "root/private");
    assert_eq!(private["ecosystem"], "pypi");
    assert_eq!(private["accounted_bytes"]["committed"], 3000);
    assert_eq!(private["accounted_bytes"]["reserved"], 500);
    assert_eq!(private["accounted_bytes"]["limit"], 10_000);
    assert_eq!(private["accounted_bytes"]["remaining"], 6500);
    assert_eq!(private["projects"]["committed"], 1);
    assert_eq!(private["projects"]["reserved"], 1);
    assert_eq!(private["projects"]["remaining"], 3);
    assert_eq!(private["file_bytes"]["limit"], serde_json::Value::Null);
    assert_eq!(private["file_bytes"]["remaining"], serde_json::Value::Null);
    assert_eq!(private["limits"]["max_accounted_bytes"], 10_000);
    assert_eq!(private["limits"]["max_file_bytes"], 1024);
    assert_eq!(private["limits"]["audit"], false);
    assert_eq!(repositories[1]["repository"], "other");

    let cursor = page1["next_cursor"].as_str().unwrap().to_owned();
    let (_, _, page2) = get(
        &state,
        &format!("/+quota?limit=2&cursor={cursor}"),
        Some(("Alice", USER_PASSWORD)),
    )
    .await;
    let page2_repositories = page2["repositories"].as_array().unwrap();
    assert_eq!(page2_repositories.len(), 1);
    assert_eq!(page2_repositories[0]["repository"], "public");
    assert_eq!(page2["next_cursor"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_detail_reports_one_repository_for_a_reader() {
    let (_dir, state) = app().await;

    let (status, headers, body) = get(
        &state,
        "/+quota/repository?repository=root/private",
        Some(("Rita", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-cache");
    assert_eq!(body["repository"], "root/private");
    assert_eq!(body["accounted_bytes"]["committed"], 3000);
    assert_eq!(body["accounted_bytes"]["remaining"], 6500);
    assert_eq!(body["limits"]["max_versions_per_project"], 20);
}

#[rstest]
#[case::anonymous(None, StatusCode::UNAUTHORIZED)]
#[case::administrator(Some(("Alice", USER_PASSWORD)), StatusCode::OK)]
#[case::operator(Some(("Olivia", USER_PASSWORD)), StatusCode::NOT_FOUND)]
#[case::repository_reader(Some(("Rita", USER_PASSWORD)), StatusCode::NOT_FOUND)]
#[case::legacy_token(Some(("__token__", READER_SECRET)), StatusCode::FORBIDDEN)]
#[case::unknown_user(Some(("Unknown", USER_PASSWORD)), StatusCode::UNAUTHORIZED)]
#[case::wrong_password(Some(("Alice", "wrong")), StatusCode::UNAUTHORIZED)]
#[tokio::test]
async fn test_summary_needs_operator_authority(#[case] credential: Option<(&str, &str)>, #[case] expected: StatusCode) {
    let (_dir, state) = app().await;

    let (status, headers, _) = get(&state, "/+quota", credential).await;

    assert_eq!(status, expected);
    if expected == StatusCode::UNAUTHORIZED {
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Basic realm=\"peryx-quota\"");
    }
}

#[rstest]
#[case::anonymous("/+quota/repository?repository=root/private", None, StatusCode::UNAUTHORIZED)]
#[case::administrator_any(
    "/+quota/repository?repository=other",
    Some(("Alice", USER_PASSWORD)),
    StatusCode::OK
)]
#[case::operator("/+quota/repository?repository=root/private", Some(("Olivia", USER_PASSWORD)), StatusCode::NOT_FOUND)]
#[case::reader_wrong_repository(
    "/+quota/repository?repository=root/private",
    Some(("Morgan", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::admin_unknown_repository(
    "/+quota/repository?repository=missing",
    Some(("Alice", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::reader_token(
    "/+quota/repository?repository=root/private",
    Some(("__token__", READER_SECRET)),
    StatusCode::OK
)]
#[case::write_only_token(
    "/+quota/repository?repository=root/private",
    Some(("__token__", ADMIN_SECRET)),
    StatusCode::FORBIDDEN
)]
#[case::invalid_token(
    "/+quota/repository?repository=root/private",
    Some(("__token__", "invalid")),
    StatusCode::UNAUTHORIZED
)]
#[case::token_unknown_repository(
    "/+quota/repository?repository=missing",
    Some(("__token__", READER_SECRET)),
    StatusCode::UNAUTHORIZED
)]
#[tokio::test]
async fn test_detail_authorization_matrix(
    #[case] uri: &str,
    #[case] credential: Option<(&str, &str)>,
    #[case] expected: StatusCode,
) {
    let (_dir, state) = app().await;

    let (status, _, _) = get(&state, uri, credential).await;

    assert_eq!(status, expected);
}

#[rstest]
#[case::missing_repository("/+quota/repository", "repository is required")]
#[case::oversized_repository(&format!("/+quota/repository?repository={}", "x".repeat(513)), "repository filter exceeds 512 bytes")]
#[case::invalid_query("/+quota/repository?repository=root/private&limit=abc", "invalid quota query")]
#[tokio::test]
async fn test_detail_rejects_invalid_requests(#[case] uri: &str, #[case] error: &str) {
    let (_dir, state) = app().await;

    let (status, _, body) = get(&state, uri, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, serde_json::json!({ "error": error }));
}

#[rstest]
#[case::limit_zero("/+quota?limit=0", StatusCode::BAD_REQUEST, "limit must be between 1 and 100")]
#[case::limit_large("/+quota?limit=101", StatusCode::BAD_REQUEST, "limit must be between 1 and 100")]
#[case::cursor_base64("/+quota?cursor=@@", StatusCode::BAD_REQUEST, "invalid quota cursor")]
#[case::cursor_number("/+quota?cursor=eA", StatusCode::BAD_REQUEST, "invalid quota cursor")]
#[tokio::test]
async fn test_summary_rejects_invalid_query(#[case] uri: &str, #[case] status: StatusCode, #[case] error: &str) {
    let (_dir, state) = app().await;

    let (got, _, body) = get(&state, uri, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(got, status);
    assert_eq!(body, serde_json::json!({ "error": error }));
}

#[tokio::test]
async fn test_summary_authenticates_before_parsing() {
    let (_dir, state) = app().await;

    let (status, _, _) = get(&state, "/+quota?limit=abc", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = get(&state, "/+quota?limit=abc", Some(("Alice", USER_PASSWORD))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[rstest]
#[case::authentication(StoreFault::Authentication)]
#[case::authorization(StoreFault::Authorization)]
#[case::quota(StoreFault::Quota)]
#[tokio::test]
async fn test_summary_fails_closed_on_store_errors(#[case] fault: StoreFault) {
    let (_dir, state) = app_with_fault(fault).await;

    let (status, _, body) = get(&state, "/+quota", Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, serde_json::json!({ "error": "quota service unavailable" }));
}

#[rstest]
#[case::authentication(StoreFault::Authentication, StatusCode::SERVICE_UNAVAILABLE)]
#[case::authorization(StoreFault::Authorization, StatusCode::SERVICE_UNAVAILABLE)]
#[case::quota(StoreFault::Quota, StatusCode::SERVICE_UNAVAILABLE)]
#[tokio::test]
async fn test_detail_fails_closed_on_store_errors(#[case] fault: StoreFault, #[case] expected: StatusCode) {
    let (_dir, state) = app_with_fault(fault).await;

    let (status, _, _) = get(
        &state,
        "/+quota/repository?repository=root/private",
        Some(("Rita", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, expected);
}
