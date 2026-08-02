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
use peryx_policy::{Policy, PolicyAction, PolicyDecisionState};
use peryx_storage::meta::{MetaStore, NewPolicyDecision};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const READER_SECRET: &str = "reader-secret";
const PASSWORD: &str = "local password";

async fn app(read_only: bool) -> (tempfile::TempDir, MetaStore, axum::Router) {
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
    let mut state = AppState::new(meta.clone(), blobs, 60, vec![index()]);
    state.users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    state.read_only = read_only;
    (dir, meta, crate::router(Arc::new(state)))
}

fn index() -> Index {
    Index {
        name: "private".to_owned(),
        // A route deliberately distinct from the name: PQL scopes by the stable repository name, so
        // `repository == "private"` must resolve regardless of the URL route.
        route: "private-route".to_owned(),
        ecosystem: Ecosystem::Pypi,
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: false,
            tokens: vec![NamedToken {
                name: "reader".to_owned(),
                secret: READER_SECRET.to_owned(),
                grants: vec![Grant {
                    projects: vec![Glob::new("*")],
                    actions: BTreeSet::from([Action::Read]),
                }],
                expires_at: None,
            }],
        },
    }
}

fn decision<'a>(repository: &'a str, project: &'a str, state: PolicyDecisionState, at: i64) -> NewPolicyDecision<'a> {
    NewPolicyDecision {
        repository,
        project,
        version: Some("1.0"),
        filename: Some("package-1.0.whl"),
        source: Some("pypi"),
        action: PolicyAction::Serve,
        state,
        rule: (state == PolicyDecisionState::Deny).then_some("blocked-project"),
        reason: (state == PolicyDecisionState::Deny).then_some("project is blocked"),
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

async fn post(
    app: &axum::Router,
    body: Value,
    credential: Option<(&str, &str)>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
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
    let response = app
        .clone()
        .oneshot(request.body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn projects(document: &Value) -> Vec<String> {
    document["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["project"].as_str().unwrap().to_owned())
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
    assert_eq!(projects(&document), ["alpha", "beta", "gamma"]);
    assert_eq!(document["rows"][0]["source"], json!("pypi"));
    assert_eq!(document["rows"][0]["reason"], json!("project is blocked"));
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
    assert_eq!(projects(&document), ["alpha", "gamma"]);
}

#[tokio::test]
async fn test_query_narrows_read_through_project_index() {
    // A leading `project ==` equality is the cost gate's indexed filter; the source pushes it into the
    // store's project index rather than paging the whole domain, and the result stays exact.
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where project == \"alpha\""}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(projects(&document), ["alpha"]);
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
    assert_eq!(projects(&document), ["alpha", "beta"]);
    let first = &document["rows"][0];
    assert!(first.get("project").is_some());
    assert!(first.get("source").is_none());
    assert!(first.get("reason").is_none());
    assert!(first.get("rule").is_none());
}

#[tokio::test]
async fn test_query_legacy_reader_token_reads_its_repository() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, document) = post(
        &app,
        json!({"query": "from policy.decisions where repository == \"private\""}),
        Some(("__token__", READER_SECRET)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(projects(&document), ["alpha", "beta"]);
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
    assert_eq!(operator_document["rows"][0]["source"], json!("pypi"));

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
async fn test_query_joins_are_not_available_yet() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);
    let (status, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions join usage on project"}),
        Some(("Alice", PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
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
async fn test_query_rejects_bad_requests() {
    let (_dir, meta, app) = app(false).await;
    seed(&meta);

    let (unparsable, _headers, _document) =
        post(&app, json!({"query": "not a query"}), Some(("Alice", PASSWORD))).await;
    assert_eq!(unparsable, StatusCode::BAD_REQUEST);

    let (bad_param, _headers, _document) = post(
        &app,
        json!({"query": "from policy.decisions where downloads == :n", "params": {"n": [1, 2]}}),
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
