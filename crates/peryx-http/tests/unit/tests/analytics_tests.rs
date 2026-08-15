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
use peryx_events::metrics::Observation;
use peryx_identity::{Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, PasswordPolicy, Role};
use peryx_policy::Policy;
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

const ADMIN_SECRET: &str = "admin-secret";
const READER_SECRET: &str = "reader-secret";
const USER_PASSWORD: &str = "local password";

#[derive(Clone, Copy)]
enum StoreFault {
    None,
    Authentication,
    Authorization,
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
    let mut state = AppState::new(meta.clone(), blobs, 60, indexes());
    super::support::register_example_driver(&mut state);
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, Arc::new(state))
}

fn indexes() -> Vec<Index> {
    vec![
        index(
            "private",
            vec![
                token("reader", READER_SECRET, Action::Read),
                token("admin", ADMIN_SECRET, Action::Write),
            ],
        ),
        index("other", Vec::new()),
        index("read-only", Vec::new()),
    ]
}

fn index(route: &str, tokens: Vec<NamedToken>) -> Index {
    Index {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: false,
            tokens,
        },
    }
}

fn token(name: &str, secret: &str, action: Action) -> NamedToken {
    NamedToken {
        name: name.to_owned(),
        secret: secret.to_owned(),
        grants: vec![Grant {
            resources: vec![Glob::new("*")],
            actions: BTreeSet::from([action]),
        }],
        expires_at: None,
    }
}

fn seed(state: &AppState) {
    for (route, resource, group, source, bytes, times) in [
        ("private", "resource-a", "group-a", Some("source-a"), 10u64, 2),
        ("private", "resource-b", "group-b", None, 5, 1),
        ("other", "resource-c", "group-c", Some("source-a"), 99, 1),
    ] {
        for _ in 0..times {
            state.serving.metrics.record(Observation::Read {
                repository: route.to_owned(),
                resource: resource.to_owned(),
                artifact: format!("{resource}-{group}.bin"),
                group: Some(group.to_owned()),
                source: source.map(str::to_owned),
                bytes,
            });
        }
    }
    state.serving.metrics.flush().unwrap();
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

fn field(row: &serde_json::Value, key: &str) -> String {
    match &row[key] {
        serde_json::Value::Null => "-".to_owned(),
        value => value.as_str().map_or_else(|| value.to_string(), str::to_owned),
    }
}

fn rows(body: &serde_json::Value, key: &str, columns: &[&str]) -> Vec<String> {
    body[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| field(row, column))
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect()
}

#[tokio::test]
async fn test_top_resources_ranks_across_repositories_and_paginates() {
    let (_dir, state) = app().await;
    seed(&state);

    let (status, headers, page1) = get(
        &state,
        "/+analytics/top-resources?limit=2",
        Some(("Alice", USER_PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        rows(&page1, "resources", &["repository", "resource", "reads", "bytes"]),
        ["private/resource-a/2/20", "other/resource-c/1/99"]
    );
    assert_eq!(page1["interval"]["retained_from_day"], serde_json::Value::Null);
    assert_eq!(page1["interval"]["window_clamped_to_retention"], false);

    let cursor = page1["next_cursor"].as_str().unwrap().to_owned();
    let (_, _, page2) = get(
        &state,
        &format!("/+analytics/top-resources?limit=2&cursor={cursor}"),
        Some(("Alice", USER_PASSWORD)),
    )
    .await;
    assert_eq!(
        rows(&page2, "resources", &["repository", "resource", "reads", "bytes"]),
        ["private/resource-b/1/5"]
    );
    assert_eq!(page2["next_cursor"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_repository_scope_limits_rows_to_the_authorized_repository() {
    let (_dir, state) = app().await;
    seed(&state);

    let (status, _, body) = get(
        &state,
        "/+analytics/top-resources?repository=private",
        Some(("Rita", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&body, "resources", &["repository", "resource", "reads", "bytes"]),
        ["private/resource-a/2/20", "private/resource-b/1/5"]
    );
}

#[tokio::test]
async fn test_groups_view_splits_by_group() {
    let (_dir, state) = app().await;
    seed(&state);

    let (status, _, body) = get(
        &state,
        "/+analytics/groups?repository=private",
        Some(("Rita", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&body, "groups", &["resource", "group", "reads", "bytes"]),
        ["resource-a/group-a/2/20", "resource-b/group-b/1/5"]
    );
}

#[tokio::test]
async fn test_timeline_view_buckets_the_repository_by_day() {
    let (_dir, state) = app().await;
    seed(&state);

    let (status, _, body) = get(
        &state,
        "/+analytics/timeline?repository=private",
        Some(("Rita", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let buckets = body["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets[0]["reads"], 3);
    assert_eq!(buckets[0]["bytes"], 25);
    assert_eq!(
        buckets[0]["start_unix"].as_i64().unwrap(),
        buckets[0]["day"].as_i64().unwrap() * 86_400
    );
}

#[tokio::test]
async fn test_unused_view_is_empty_when_every_resource_is_active() {
    let (_dir, state) = app().await;
    seed(&state);

    let (status, _, body) = get(
        &state,
        "/+analytics/unused?repository=private",
        Some(("Rita", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unused"], serde_json::json!([]));
}

#[tokio::test]
async fn test_unused_view_lists_resources_idle_over_the_window() {
    let (_dir, state) = app().await;
    seed(&state);

    // A window before every seeded read makes each private resource unused within it, ordered by
    // lifetime reads.
    let (status, _, body) = get(
        &state,
        "/+analytics/unused?repository=private&from=0&to=86400",
        Some(("Rita", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&body, "unused", &["repository", "resource", "lifetime_reads"]),
        ["private/resource-a/2", "private/resource-b/1"]
    );
}

#[rstest]
#[case::operator_all(Some(("Alice", USER_PASSWORD)), "/+analytics/sources", StatusCode::OK)]
#[case::operator_repository(Some(("Alice", USER_PASSWORD)), "/+analytics/sources?repository=private", StatusCode::OK)]
#[case::operator_unknown_repository(Some(("Alice", USER_PASSWORD)), "/+analytics/sources?repository=missing", StatusCode::NOT_FOUND)]
#[case::repository_reader(Some(("Rita", USER_PASSWORD)), "/+analytics/sources?repository=private", StatusCode::NOT_FOUND)]
#[case::scoped_credential(Some(("external", READER_SECRET)), "/+analytics/sources?repository=private", StatusCode::FORBIDDEN)]
#[tokio::test]
async fn test_sources_view_is_operator_only(
    #[case] credential: Option<(&str, &str)>,
    #[case] uri: &str,
    #[case] expected: StatusCode,
) {
    let (_dir, state) = app().await;
    seed(&state);

    let (status, _, body) = get(&state, uri, credential).await;

    assert_eq!(status, expected);
    if expected == StatusCode::OK && !uri.contains("repository") {
        assert_eq!(
            rows(&body, "sources", &["resource", "source", "reads", "bytes"]),
            [
                "resource-a/source-a/2/20",
                "resource-c/source-a/1/99",
                "resource-b/-/1/5"
            ]
        );
    }
}

#[rstest]
#[case::anonymous("/+analytics/top-resources", None, StatusCode::UNAUTHORIZED)]
#[case::reader_token(
    "/+analytics/top-resources?repository=private",
    Some(("external", READER_SECRET)),
    StatusCode::OK
)]
#[case::write_only_token(
    "/+analytics/top-resources?repository=private",
    Some(("external", ADMIN_SECRET)),
    StatusCode::FORBIDDEN
)]
#[case::invalid_token(
    "/+analytics/top-resources?repository=private",
    Some(("external", "invalid")),
    StatusCode::UNAUTHORIZED
)]
#[case::token_unknown_repository(
    "/+analytics/top-resources?repository=missing",
    Some(("external", READER_SECRET)),
    StatusCode::UNAUTHORIZED
)]
#[case::token_without_selection(
    "/+analytics/top-resources",
    Some(("external", READER_SECRET)),
    StatusCode::UNAUTHORIZED
)]
#[case::operator_all("/+analytics/top-resources", Some(("Olivia", USER_PASSWORD)), StatusCode::OK)]
#[case::operator_repository(
    "/+analytics/top-resources?repository=private",
    Some(("Olivia", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::reader_without_selection("/+analytics/top-resources", Some(("Rita", USER_PASSWORD)), StatusCode::NOT_FOUND)]
#[case::reader_wrong_repository(
    "/+analytics/top-resources?repository=private",
    Some(("Morgan", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::admin_unknown_repository(
    "/+analytics/top-resources?repository=missing",
    Some(("Alice", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::unknown_user("/+analytics/top-resources", Some(("Unknown", USER_PASSWORD)), StatusCode::UNAUTHORIZED)]
#[case::wrong_password("/+analytics/top-resources", Some(("Alice", "wrong")), StatusCode::UNAUTHORIZED)]
#[tokio::test]
async fn test_authorization_matrix(
    #[case] uri: &str,
    #[case] credential: Option<(&str, &str)>,
    #[case] expected: StatusCode,
) {
    let (_dir, state) = app().await;

    let (status, headers, _) = get(&state, uri, credential).await;

    assert_eq!(status, expected);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    if expected == StatusCode::UNAUTHORIZED {
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Basic realm=\"peryx-analytics\"");
    }
}

#[rstest]
#[case::limit_zero("/+analytics/top-resources?limit=0", "limit must be between 1 and 100")]
#[case::limit_large("/+analytics/top-resources?limit=101", "limit must be between 1 and 100")]
#[case::cursor_base64("/+analytics/top-resources?cursor=@@", "invalid analytics cursor")]
#[case::cursor_number("/+analytics/top-resources?cursor=eA", "invalid analytics cursor")]
#[case::range("/+analytics/top-resources?from=100&to=50", "time range start is after its end")]
#[tokio::test]
async fn test_rejects_invalid_query_parameters(#[case] uri: &str, #[case] error: &str) {
    let (_dir, state) = app().await;

    let (status, headers, body) = get(&state, uri, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, serde_json::json!({ "error": error }));
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn test_rejects_oversized_repository_filter() {
    let (_dir, state) = app().await;
    let uri = format!("/+analytics/top-resources?repository={}", "x".repeat(513));

    let (status, _, body) = get(&state, &uri, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        serde_json::json!({"error": "repository filter exceeds 512 bytes"})
    );
}

#[rstest]
#[case::anonymous(None, StatusCode::UNAUTHORIZED)]
#[case::authenticated(Some(("Alice", USER_PASSWORD)), StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn test_authenticates_before_parsing(#[case] credential: Option<(&str, &str)>, #[case] expected: StatusCode) {
    let (_dir, state) = app().await;

    let (status, _, _) = get(&state, "/+analytics/top-resources?limit=abc", credential).await;

    assert_eq!(status, expected);
}

#[rstest]
#[case::authentication(StoreFault::Authentication)]
#[case::authorization(StoreFault::Authorization)]
#[tokio::test]
async fn test_fails_closed_on_store_errors(#[case] fault: StoreFault) {
    let (_dir, state) = app_with_fault(fault).await;

    let (status, headers, body) = get(&state, "/+analytics/top-resources", Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(body, serde_json::json!({"error": "analytics service unavailable"}));
}
