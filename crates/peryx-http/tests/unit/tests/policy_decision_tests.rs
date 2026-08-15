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
use peryx_identity::{Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, PasswordPolicy, Role, UserState};
use peryx_policy::{Policy, PolicyAction, PolicyDecisionState};
use peryx_storage::meta::{MetaError, MetaStore, NewPolicyDecision, PolicyDecisionQueryError};
use rstest::rstest;
use tower::ServiceExt as _;

const ADMIN_SECRET: &str = "admin-secret";
const READER_SECRET: &str = "reader-secret";
const USER_PASSWORD: &str = "local password";

async fn app() -> (tempfile::TempDir, MetaStore, axum::Router) {
    app_with_options(StoreFault::None, None).await
}

#[derive(Clone, Copy)]
enum StoreFault {
    None,
    Identity,
    Authentication,
    Authorization,
    Query,
}

async fn app_with_fault(fault: StoreFault) -> (tempfile::TempDir, MetaStore, axum::Router) {
    app_with_options(fault, None).await
}

async fn app_with_options(
    fault: StoreFault,
    token_user_state: Option<UserState>,
) -> (tempfile::TempDir, MetaStore, axum::Router) {
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
            "Peter",
            Role::RepositoryPublisher,
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
    if let Some(state) = token_user_state {
        let user = users.create(super::support::EXTERNAL_USER).unwrap();
        users.set_password(&user.id, USER_PASSWORD).await.unwrap();
        if state == UserState::Disabled {
            users.disable(&user.id).unwrap();
        }
    }
    drop(authorization);
    drop(users);
    drop(meta);
    if let Some(table) = match fault {
        StoreFault::None => None,
        StoreFault::Identity => Some("server_user"),
        StoreFault::Authentication => Some("server_user_verifier"),
        StoreFault::Authorization => Some("role_grant"),
        StoreFault::Query => Some("policy_decision"),
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
    super::support::register_example_driver(&mut state);
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    (dir, meta, crate::router(Arc::new(state)))
}

fn private_index() -> Index {
    Index {
        name: "private".to_owned(),
        route: "private".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: vec![
                NamedToken {
                    name: "admin".to_owned(),
                    secret: ADMIN_SECRET.to_owned(),
                    grants: vec![Grant {
                        resources: vec![Glob::new("*")],
                        actions: BTreeSet::from([Action::Write]),
                    }],
                    expires_at: None,
                },
                NamedToken {
                    name: "reader".to_owned(),
                    secret: READER_SECRET.to_owned(),
                    grants: vec![Grant {
                        resources: vec![Glob::new("*")],
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

fn decision(resource: &str, state: PolicyDecisionState, evaluated_at_unix: i64) -> NewPolicyDecision<'_> {
    NewPolicyDecision {
        repository: "private",
        resource,
        group: Some("1.0"),
        artifact: Some("artifact-1.0.bin"),
        source: Some("alpha"),
        action: PolicyAction::Serve,
        state,
        rule: (state == PolicyDecisionState::Deny).then_some("blocked-resource"),
        reason: (state == PolicyDecisionState::Deny).then_some("resource is blocked"),
        evaluated_at_unix,
        next_eligible_at_unix: None,
    }
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
async fn test_policy_decisions_filters_authorized_repository_history() {
    let (_dir, meta, app) = app().await;
    meta.record_policy_decision(decision("alpha", PolicyDecisionState::Allow, 10))
        .unwrap();
    let denied = meta
        .record_policy_decision(decision("beta", PolicyDecisionState::Deny, 20))
        .unwrap();
    let mut other = decision("gamma", PolicyDecisionState::Deny, 21);
    other.repository = "other";
    meta.record_policy_decision(other).unwrap();

    let (status, headers, document) = get(
        &app,
        "/+policy/decisions?repository=private&state=deny&rule=blocked-resource&source=alpha&from=15&to=25",
        Some(("external", ADMIN_SECRET)),
    )
    .await;

    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        (status, document),
        (
            StatusCode::OK,
            serde_json::json!({
                "decisions": [{
                    "id": denied.id,
                    "repository": "private",
                    "resource": "beta",
                    "group": "1.0",
                    "artifact": "artifact-1.0.bin",
                    "source": "alpha",
                    "action": "serve",
                    "state": "deny",
                    "rule": "blocked-resource",
                    "reason": "resource is blocked",
                    "evaluated_at_unix": 20,
                    "input_generation": {"repository": 0, "catalog": 0, "policy": 0},
                    "next_eligible_at_unix": null,
                    "fresh": true
                }],
                "next_cursor": null
            }),
        )
    );
}

#[tokio::test]
async fn test_policy_decisions_scopes_to_one_resource() {
    let (_dir, meta, app) = app().await;
    meta.record_policy_decision(decision("alpha", PolicyDecisionState::Deny, 10))
        .unwrap();
    meta.record_policy_decision(decision("beta", PolicyDecisionState::Deny, 20))
        .unwrap();

    let (status, _headers, document) = get(
        &app,
        "/+policy/decisions?repository=private&resource=alpha",
        Some(("external", ADMIN_SECRET)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        document["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|decision| decision["resource"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["alpha"]
    );
}

#[rstest]
#[case::administrator("Alice", "/+policy/decisions", &[("other", "beta"), ("private", "alpha")])]
#[case::administrator_repository("Alice", "/+policy/decisions?repository=private", &[("private", "alpha")])]
#[case::repository_reader("Rita", "/+policy/decisions?repository=private", &[("private", "alpha")])]
#[case::repository_publisher("Peter", "/+policy/decisions?repository=private", &[("private", "alpha")])]
#[tokio::test]
async fn test_policy_decisions_authorizes_local_roles(
    #[case] user: &str,
    #[case] uri: &str,
    #[case] expected: &[(&str, &str)],
) {
    let (_dir, meta, app) = app().await;
    meta.record_policy_decision(decision("alpha", PolicyDecisionState::Allow, 10))
        .unwrap();
    let mut other = decision("beta", PolicyDecisionState::Allow, 11);
    other.repository = "other";
    meta.record_policy_decision(other).unwrap();

    let (status, headers, document) = get(&app, uri, Some((user, USER_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        document["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|decision| {
                (
                    decision["repository"].as_str().unwrap(),
                    decision["resource"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>(),
        expected
    );
}

#[rstest]
#[case::anonymous("/+policy/decisions?repository=private", None, StatusCode::UNAUTHORIZED)]
#[case::reader_token(
    "/+policy/decisions?repository=private",
    Some(("external", READER_SECRET)),
    StatusCode::FORBIDDEN
)]
#[case::unknown_token(
    "/+policy/decisions?repository=missing",
    Some(("external", ADMIN_SECRET)),
    StatusCode::UNAUTHORIZED
)]
#[case::invalid_token_for_writable_repository(
    "/+policy/decisions?repository=private",
    Some(("external", "invalid")),
    StatusCode::UNAUTHORIZED
)]
#[case::invalid_token_for_read_only_repository(
    "/+policy/decisions?repository=read-only",
    Some(("external", "invalid")),
    StatusCode::UNAUTHORIZED
)]
#[case::repository_user_without_selection(
    "/+policy/decisions",
    Some(("Rita", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::wrong_password_without_selection(
    "/+policy/decisions",
    Some(("Alice", "wrong password")),
    StatusCode::UNAUTHORIZED
)]
#[case::token_without_selection(
    "/+policy/decisions",
    Some(("external", ADMIN_SECRET)),
    StatusCode::UNAUTHORIZED
)]
#[case::local_identity_cannot_fall_back_to_matching_token(
    "/+policy/decisions?repository=private",
    Some(("Alice", ADMIN_SECRET)),
    StatusCode::UNAUTHORIZED
)]
#[case::unknown_identity_cannot_present_a_token(
    "/+policy/decisions?repository=private",
    Some(("Unknown", ADMIN_SECRET)),
    StatusCode::UNAUTHORIZED
)]
#[case::operator_without_repository_scope(
    "/+policy/decisions?repository=private",
    Some(("Olivia", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::operator_without_administrator_scope(
    "/+policy/decisions",
    Some(("Olivia", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::wrong_repository(
    "/+policy/decisions?repository=private",
    Some(("Morgan", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[case::unknown_for_operator(
    "/+policy/decisions?repository=missing",
    Some(("Olivia", USER_PASSWORD)),
    StatusCode::NOT_FOUND
)]
#[tokio::test]
async fn test_policy_decisions_enforces_repository_authorization(
    #[case] uri: &str,
    #[case] credential: Option<(&str, &str)>,
    #[case] expected: StatusCode,
) {
    let (_dir, _meta, app) = app().await;

    let response = get(&app, uri, credential).await;
    assert_eq!(response.0, expected);
    assert_eq!(response.1[header::CACHE_CONTROL], "no-store");
    if expected == StatusCode::UNAUTHORIZED {
        assert_eq!(
            response.1[header::WWW_AUTHENTICATE],
            "Basic realm=\"peryx-policy-decisions\""
        );
    }
}

#[rstest]
#[case::limit("/+policy/decisions?repository=private&limit=0", "limit must be between 1 and 100")]
#[case::cursor("/+policy/decisions?repository=private&cursor=bad", "invalid policy decision cursor")]
#[tokio::test]
async fn test_policy_decisions_rejects_invalid_pagination(#[case] uri: &str, #[case] error: &str) {
    let (_dir, _meta, app) = app().await;
    let response = get(&app, uri, Some(("external", ADMIN_SECRET))).await;

    assert_eq!(
        (response.0, response.2),
        (StatusCode::BAD_REQUEST, serde_json::json!({"error": error}))
    );
    assert_eq!(response.1[header::CACHE_CONTROL], "no-store");
}

#[rstest]
#[case::repository("repository")]
#[case::resource("resource")]
#[case::rule("rule")]
#[case::source("source")]
#[tokio::test]
async fn test_policy_decisions_rejects_oversized_text_filter(#[case] field: &str) {
    let (_dir, _meta, app) = app().await;
    let oversized = "x".repeat(513);
    let uri = if field == "repository" {
        format!("/+policy/decisions?repository={oversized}")
    } else {
        format!("/+policy/decisions?repository=private&{field}={oversized}")
    };

    let response = get(&app, &uri, Some(("external", ADMIN_SECRET))).await;

    assert_eq!(response.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        response.2,
        serde_json::json!({"error": format!("{field} filter exceeds 512 bytes")})
    );
    assert_eq!(response.1[header::CACHE_CONTROL], "no-store");
}

#[rstest]
#[case::state("/+policy/decisions?state=bogus")]
#[case::limit("/+policy/decisions?limit=abc")]
#[tokio::test]
async fn test_policy_decisions_authenticates_before_parsing_queries(#[case] uri: &str) {
    let (_dir, _meta, app) = app().await;

    let anonymous = get(&app, uri, None).await;
    let authenticated = get(&app, uri, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(anonymous.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        anonymous.1[header::WWW_AUTHENTICATE],
        "Basic realm=\"peryx-policy-decisions\""
    );
    assert_eq!(anonymous.1[header::CACHE_CONTROL], "no-store");
    assert_eq!(authenticated.0, StatusCode::BAD_REQUEST);
    assert_eq!(authenticated.1[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        authenticated.2,
        serde_json::json!({"error": "invalid policy decision query"})
    );
}

#[rstest]
#[case::active(UserState::Active, ADMIN_SECRET, StatusCode::OK)]
#[case::disabled(UserState::Disabled, ADMIN_SECRET, StatusCode::OK)]
#[case::wrong_token(UserState::Active, "wrong token", StatusCode::UNAUTHORIZED)]
#[tokio::test]
async fn test_policy_decisions_reserves_the_token_username_for_repository_credentials(
    #[case] state: UserState,
    #[case] secret: &str,
    #[case] expected: StatusCode,
) {
    let (_dir, _meta, app) = app_with_options(StoreFault::None, Some(state)).await;

    let response = get(
        &app,
        "/+policy/decisions?repository=private",
        Some(("external", secret)),
    )
    .await;

    assert_eq!(response.0, expected);
    assert_eq!(response.1[header::CACHE_CONTROL], "no-store");
    if expected == StatusCode::OK {
        assert_eq!(response.2, serde_json::json!({"decisions": [], "next_cursor": null}));
    } else {
        assert_eq!(
            response.1[header::WWW_AUTHENTICATE],
            "Basic realm=\"peryx-policy-decisions\""
        );
    }
}

#[rstest]
#[case::identity(
    StoreFault::Identity,
    "/+policy/decisions",
    StatusCode::SERVICE_UNAVAILABLE,
    "policy decision service unavailable"
)]
#[case::authentication(
    StoreFault::Authentication,
    "/+policy/decisions",
    StatusCode::SERVICE_UNAVAILABLE,
    "policy decision service unavailable"
)]
#[case::authorization(
    StoreFault::Authorization,
    "/+policy/decisions",
    StatusCode::SERVICE_UNAVAILABLE,
    "policy decision service unavailable"
)]
#[case::repository_authorization(
    StoreFault::Authorization,
    "/+policy/decisions?repository=private",
    StatusCode::SERVICE_UNAVAILABLE,
    "policy decision service unavailable"
)]
#[case::query(
    StoreFault::Query,
    "/+policy/decisions",
    StatusCode::INTERNAL_SERVER_ERROR,
    "policy decision query failed"
)]
#[tokio::test]
async fn test_policy_decisions_fails_closed_on_store_errors(
    #[case] fault: StoreFault,
    #[case] uri: &str,
    #[case] status: StatusCode,
    #[case] error: &str,
) {
    let (_dir, _meta, app) = app_with_fault(fault).await;

    let response = get(&app, uri, Some(("Alice", USER_PASSWORD))).await;

    assert_eq!(response.0, status);
    assert_eq!(response.1[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.2, serde_json::json!({"error": error}));
}

#[tokio::test]
async fn test_policy_decision_error_response_hides_store_failures() {
    let response = crate::handlers::policy_decision_error_response(&PolicyDecisionQueryError::Store(
        MetaError::DriverPrecondition("sensitive detail".to_owned()),
    ));
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();

    assert_eq!(
        (status, serde_json::from_slice::<serde_json::Value>(&body).unwrap()),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"error": "policy decision query failed"}),
        )
    );
}
