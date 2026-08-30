use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::rate_limit::{RateLimitConfig, RouteLimit};
use peryx_driver::{AppState, HttpRoutes as _};
use peryx_identity::{Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, Role};
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyAction, PolicyDecisionState};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{MetaStore, NewPolicyDecision};
use redb::TableDefinition;
use rstest::rstest;
use tower::ServiceExt as _;

use crate::store::PypiStore as _;
use crate::upload::Uploaded;
use crate::{CoreMetadata, File, Provenance, Yanked};

use super::*;

const PASSWORD: &str = "local password";
const PROJECT: &str = "acme-pkg";
const FIRST_FILE: &str = "acme_pkg-1.0-py3-none-any.whl";
const SECOND_FILE: &str = "acme_pkg-2.0-py3-none-any.whl";

#[tokio::test]
async fn shadow_candidates_ignore_repositories_owned_by_another_ecosystem() {
    let (_directory, state) = state(vec![Index {
        name: "foreign".to_owned(),
        route: "foreign".to_owned(),
        ecosystem: peryx_core::Ecosystem::new("foreign"),
        kind: IndexKind::Virtual {
            layers: Vec::new(),
            write_target: None,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }]);
    let authorization = local_reader(&state, "foreign").await;

    let (status, _, body) = request(
        &state,
        "/+shadow/candidates?repository=foreign&project=acme-pkg",
        Some(HeaderValue::from_str(&authorization).unwrap()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"candidates": [], "next_cursor": null})
    );
}

#[tokio::test]
async fn shadow_routes_serve_the_admin_page() {
    let (_directory, state) = state(Vec::new());

    let (status, headers, body) = request(&state, "/admin/shadow", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(body, include_str!("../../../src/shadow/shadow.html"));
}

#[tokio::test]
async fn shadow_routes_use_the_admin_limit() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::with_rate_limits(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        Vec::new(),
        RateLimitConfig {
            admin: RouteLimit::new(1, 60),
            ..RateLimitConfig::enabled_defaults()
        },
        std::iter::empty(),
    );
    crate::tests::install(&mut state);
    let app = peryx_http::router(Arc::new(state));

    let first = app
        .clone()
        .oneshot(Request::get("/admin/shadow").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let second = app
        .clone()
        .oneshot(Request::get("/admin/shadow").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let package_read = app
        .oneshot(Request::get("/unknown").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(
        (first.status(), second.status(), package_read.status()),
        (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS, StatusCode::NOT_FOUND)
    );
}

#[tokio::test]
async fn shadow_candidates_return_decisions_and_paginate_without_overlap() {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", IndexAcl::default());
    record_decisions(&state);
    let authorization = local_reader(&state, "root-pypi").await;

    let (status, headers, body) = request(
        &state,
        "/+shadow/candidates?repository=root/pypi&project=acme-pkg&limit=2",
        Some(HeaderValue::from_str(&authorization).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let first: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        first["candidates"],
        serde_json::json!([
            {
                "decision": {
                    "evaluated_at_unix": 20,
                    "fresh": true,
                    "next_eligible_at_unix": 120,
                    "reason": "policy note",
                    "rule": "cooldown",
                    "state": "wait"
                },
                "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                "filename": FIRST_FILE,
                "member": "first",
                "selected": true,
                "source": "hosted"
            },
            {
                "decision": {
                    "evaluated_at_unix": 20,
                    "fresh": true,
                    "next_eligible_at_unix": 120,
                    "reason": "policy note",
                    "rule": "cooldown",
                    "state": "wait"
                },
                "digest": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                "filename": FIRST_FILE,
                "member": "second",
                "reason": "precedence",
                "selected": false,
                "source": "hosted"
            }
        ])
    );
    let cursor = first["next_cursor"].as_str().unwrap();

    let (status, _, body) = request(
        &state,
        &format!(
            "/+shadow/candidates?repository=root/pypi&project=acme-pkg&limit=2&cursor={}",
            url::form_urlencoded::byte_serialize(cursor.as_bytes()).collect::<String>()
        ),
        Some(HeaderValue::from_str(&authorization).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "candidates": [{
                "digest": "sha256:3333333333333333333333333333333333333333333333333333333333333333",
                "filename": SECOND_FILE,
                "member": "second",
                "selected": true,
                "source": "hosted"
            }],
            "next_cursor": null
        })
    );
}

#[tokio::test]
async fn shadow_candidates_find_stale_decisions_behind_unrelated_records() {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", IndexAcl::default());
    state
        .serving
        .meta
        .record_policy_decision(decision(
            Some(SECOND_FILE),
            PolicyAction::Serve,
            PolicyDecisionState::Deny,
            "blocked",
            10,
            None,
        ))
        .unwrap();
    for index in 0..101 {
        let artifact = format!("unrelated-{index}.whl");
        state
            .serving
            .meta
            .record_policy_decision(decision(
                Some(&artifact),
                PolicyAction::Serve,
                PolicyDecisionState::Allow,
                "unrelated",
                20 + index,
                None,
            ))
            .unwrap();
    }
    state
        .serving
        .meta
        .record_policy_decision(decision(
            Some(SECOND_FILE),
            PolicyAction::Upload,
            PolicyDecisionState::Allow,
            "upload",
            200,
            None,
        ))
        .unwrap();
    state.serving.meta.next_serial().unwrap();
    let authorization = local_reader(&state, "root-pypi").await;

    let (status, _, body) = request(
        &state,
        "/+shadow/candidates?repository=root/pypi&project=acme-pkg",
        Some(HeaderValue::from_str(&authorization).unwrap()),
    )
    .await;
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    let candidate = body["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["filename"] == SECOND_FILE)
        .unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        candidate["decision"],
        serde_json::json!({
            "evaluated_at_unix": 10,
            "fresh": false,
            "reason": "policy note",
            "rule": "blocked",
            "state": "deny",
        })
    );
}

#[tokio::test]
async fn shadow_candidates_skip_decision_reads_for_empty_pages() {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", IndexAcl::default());
    let authorization = local_reader(&state, "root-pypi").await;

    let (status, _, body) = request(
        &state,
        "/+shadow/candidates?repository=root/pypi&project=absent",
        Some(HeaderValue::from_str(&authorization).unwrap()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"candidates": [], "next_cursor": null})
    );
}

#[rstest]
#[case::invalid_limit(
    "/+shadow/candidates?repository=root/pypi&project=acme-pkg&limit=0".to_owned(),
    "limit must be between 1 and 100"
)]
#[case::empty_cursor(
    "/+shadow/candidates?repository=root/pypi&project=acme-pkg&cursor=".to_owned(),
    "invalid shadow cursor"
)]
#[case::project_too_long(
    format!("/+shadow/candidates?repository=root/pypi&project={}", "x".repeat(513)),
    "project filter exceeds 512 bytes"
)]
#[tokio::test]
async fn shadow_candidates_report_validation_errors(#[case] uri: String, #[case] expected: &str) {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", IndexAcl::default());
    let authorization = local_reader(&state, "root-pypi").await;

    let (status, headers, body) = request(&state, &uri, Some(HeaderValue::from_str(&authorization).unwrap())).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"error": expected})
    );
}

#[tokio::test]
async fn shadow_contract_reports_malformed_parameters() {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", IndexAcl::default());
    let authorization = local_reader(&state, "root-pypi").await;

    for (uri, expected) in [
        (
            "/+shadow/candidates?project=acme-pkg",
            "missing shadow query parameter `repository`",
        ),
        (
            "/+shadow/candidates?repository=root/pypi",
            "missing shadow query parameter `project`",
        ),
        (
            "/+shadow/candidates?repository=root/pypi&resource=acme-pkg",
            "unknown shadow query parameter `resource`; use `project`",
        ),
        (
            "/+shadow/candidates?repository=root/pypi&project=acme-pkg&arbitrary=secret",
            "unknown shadow query parameter",
        ),
        (
            "/+shadow/candidates?repository=root/pypi&project=acme-pkg&limit=abc",
            "shadow query parameter `limit` must be an unsigned integer",
        ),
        (
            "/+shadow/candidates?repository=root/pypi&repository=root/pypi&project=acme-pkg",
            "duplicate shadow query parameter",
        ),
    ] {
        let (status, _, body) = request(&state, uri, Some(HeaderValue::from_str(&authorization).unwrap())).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": expected}),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn shadow_candidates_hide_decision_store_errors() {
    let repository = "r".repeat(513);
    let (_directory, state) = seeded_state(&repository, &repository, IndexAcl::default());
    let authorization = local_reader(&state, &repository).await;

    let (status, _, body) = request(
        &state,
        &format!("/+shadow/candidates?repository={repository}&project={PROJECT}"),
        Some(HeaderValue::from_str(&authorization).unwrap()),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"error": "shadow query failed"})
    );
}

#[rstest]
#[case::missing(None)]
#[case::wrong_scheme(Some(HeaderValue::from_static("Bearer token")))]
#[tokio::test]
async fn shadow_candidates_require_basic_authentication(#[case] authorization: Option<HeaderValue>) {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", IndexAcl::default());

    let (status, headers, body) = request(
        &state,
        "/+shadow/candidates?repository=root/pypi&project=acme-pkg",
        authorization,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers[header::WWW_AUTHENTICATE], "Basic realm=\"peryx-shadow\"");
    assert!(body.is_empty());
}

#[tokio::test]
async fn shadow_candidates_hide_unknown_repositories_from_local_users() {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", IndexAcl::default());
    let authorization = local_reader(&state, "root-pypi").await;

    let (status, _, body) = request(
        &state,
        "/+shadow/candidates?repository=missing&project=acme-pkg",
        Some(HeaderValue::from_str(&authorization).unwrap()),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.is_empty());
}

#[tokio::test]
async fn shadow_candidates_hide_repositories_without_a_reader_grant() {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", IndexAcl::default());
    let authorization = local_user(&state).await;

    let (status, _, body) = request(
        &state,
        "/+shadow/candidates?repository=root/pypi&project=acme-pkg",
        Some(HeaderValue::from_str(&authorization).unwrap()),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.is_empty());
}

#[tokio::test]
async fn shadow_candidates_report_identity_store_failures() {
    let (_directory, state) = state_with_corrupt_table("server_user_name");

    let (status, _, body) = request(
        &state,
        "/+shadow/candidates?repository=root/pypi&project=acme-pkg",
        Some(HeaderValue::from_static("Basic QWxpY2U6cGFzc3dvcmQ=")),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"error": "shadow inspection service unavailable"})
    );
}

#[tokio::test]
async fn shadow_candidates_report_password_overload() {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let mut state = AppState::new(
        meta.clone(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        Vec::new(),
    );
    Arc::get_mut(&mut state.serving).unwrap().users = peryx_driver::users::UserService::with_password_settings(
        meta,
        peryx_identity::PasswordPolicy::new(8, 1, 1).unwrap(),
        0,
    );
    crate::tests::install(&mut state);

    let (status, headers, body) = request(
        &Arc::new(state),
        "/+shadow/candidates?repository=root-pypi&project=acme-pkg",
        Some(HeaderValue::from_static("Basic QWxpY2U6cGFzc3dvcmQ=")),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"error": "shadow inspection service unavailable"})
    );
}

#[tokio::test]
async fn shadow_candidates_report_authorization_store_failures() {
    let (_directory, state) = state_with_corrupt_table("role_grant");
    let authorization = local_user(&state).await;

    let (status, _, body) = request(
        &state,
        "/+shadow/candidates?repository=root/pypi&project=acme-pkg",
        Some(HeaderValue::from_str(&authorization).unwrap()),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"error": "shadow inspection service unavailable"})
    );
}

#[tokio::test]
async fn shadow_candidates_accept_a_write_index_credential() {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", token_acl(Action::Write));

    let (status, _, body) = request(
        &state,
        "/+shadow/candidates?repository=root/pypi&project=absent",
        Some(index_credential("secret")),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"candidates": [], "next_cursor": null})
    );
}

#[tokio::test]
async fn shadow_candidates_forbid_an_index_credential_without_write_access() {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", token_acl(Action::Read));

    let (status, _, body) = request(
        &state,
        "/+shadow/candidates?repository=root/pypi&project=acme-pkg",
        Some(index_credential("secret")),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.is_empty());
}

#[rstest]
#[case::unknown_repository("missing", "secret")]
#[case::wrong_secret("root/pypi", "wrong")]
#[tokio::test]
async fn shadow_candidates_reject_unusable_index_credentials(#[case] repository: &str, #[case] secret: &str) {
    let (_directory, state) = seeded_state("root-pypi", "root/pypi", token_acl(Action::Write));

    let (status, headers, body) = request(
        &state,
        &format!("/+shadow/candidates?repository={repository}&project={PROJECT}"),
        Some(index_credential(secret)),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers[header::WWW_AUTHENTICATE], "Basic realm=\"peryx-shadow\"");
    assert!(body.is_empty());
}

fn state(indexes: Vec<Index>) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let state = app_state(
        &directory,
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        indexes,
    );
    (directory, state)
}

fn seeded_state(repository: &str, route: &str, acl: IndexAcl) -> (tempfile::TempDir, Arc<AppState>) {
    let indexes = vec![
        hosted_index("first"),
        hosted_index("second"),
        Index {
            name: repository.to_owned(),
            route: route.to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![0, 1],
                write_target: Some(0),
            },
            policy: Policy::default(),
            acl,
        },
    ];
    let (directory, state) = state(indexes);
    seed_file(&state, "first", FIRST_FILE, "1.0", '1');
    seed_file(&state, "second", FIRST_FILE, "1.0", '2');
    seed_file(&state, "second", SECOND_FILE, "2.0", '3');
    (directory, state)
}

fn hosted_index(name: &str) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Hosted { volatile: true },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

fn seed_file(state: &AppState, repository: &str, filename: &str, version: &str, digest_digit: char) {
    let uploaded = Uploaded {
        version: version.to_owned(),
        file: File {
            filename: filename.to_owned(),
            url: format!("https://files.invalid/{filename}"),
            hashes: BTreeMap::from([("sha256".to_owned(), digest_digit.to_string().repeat(64))]),
            requires_python: None,
            size: Some(1),
            upload_time: Some("2020-01-01T00:00:00Z".to_owned()),
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
        trashed: None,
    };
    state
        .serving
        .meta
        .put_upload(repository, PROJECT, filename, &serde_json::to_vec(&uploaded).unwrap())
        .unwrap();
    state.serving.meta.put_project(repository, PROJECT, PROJECT).unwrap();
}

fn record_decisions(state: &AppState) {
    for decision in [
        decision(
            Some(FIRST_FILE),
            PolicyAction::Upload,
            PolicyDecisionState::Allow,
            "upload",
            1,
            None,
        ),
        decision(None, PolicyAction::Serve, PolicyDecisionState::Deny, "project", 2, None),
        decision(
            Some("other.whl"),
            PolicyAction::Serve,
            PolicyDecisionState::Deny,
            "other",
            3,
            None,
        ),
        decision(
            Some(FIRST_FILE),
            PolicyAction::Serve,
            PolicyDecisionState::Allow,
            "older",
            10,
            None,
        ),
        decision(
            Some(FIRST_FILE),
            PolicyAction::Serve,
            PolicyDecisionState::Wait,
            "cooldown",
            20,
            Some(120),
        ),
    ] {
        state.serving.meta.record_policy_decision(decision).unwrap();
    }
}

fn decision<'a>(
    artifact: Option<&'a str>,
    action: PolicyAction,
    state: PolicyDecisionState,
    rule: &'a str,
    evaluated_at_unix: i64,
    next_eligible_at_unix: Option<i64>,
) -> NewPolicyDecision<'a> {
    NewPolicyDecision {
        repository: "root-pypi",
        resource: PROJECT,
        group: None,
        artifact,
        source: Some("first"),
        action,
        state,
        rule: Some(rule),
        reason: Some("policy note"),
        evaluated_at_unix,
        next_eligible_at_unix,
    }
}

async fn local_reader(state: &AppState, repository: &str) -> String {
    let user = state.serving.users.create("Alice").unwrap();
    state.serving.users.set_password(&user.id, PASSWORD).await.unwrap();
    state
        .serving
        .authorization
        .grant(
            &user.id,
            Role::RepositoryReader,
            GrantScope::Repository {
                name: repository.to_owned(),
            },
        )
        .unwrap();
    basic("Alice", PASSWORD)
}

async fn local_user(state: &AppState) -> String {
    let user = state.serving.users.create("Alice").unwrap();
    state.serving.users.set_password(&user.id, PASSWORD).await.unwrap();
    basic("Alice", PASSWORD)
}

fn token_acl(action: Action) -> IndexAcl {
    IndexAcl {
        anonymous_read: false,
        tokens: vec![NamedToken {
            name: "shadow".to_owned(),
            secret: "secret".to_owned(),
            grants: vec![Grant {
                resources: vec![Glob::new("*")],
                actions: BTreeSet::from([action]),
            }],
            expires_at: None,
        }],
    }
}

fn index_credential(secret: &str) -> HeaderValue {
    HeaderValue::from_str(&basic("__token__", secret)).unwrap()
}

fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
}

fn state_with_corrupt_table(table: &'static str) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .open_table(TableDefinition::<&str, u64>::new(table))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let indexes = vec![Index {
        name: "root-pypi".to_owned(),
        route: "root/pypi".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Virtual {
            layers: Vec::new(),
            write_target: None,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }];
    let state = app_state(&directory, MetaStore::open_existing(path).unwrap(), indexes);
    (directory, state)
}

fn app_state(directory: &tempfile::TempDir, meta: MetaStore, indexes: Vec<Index>) -> Arc<AppState> {
    let mut state = AppState::new(
        meta,
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        indexes,
    );
    crate::tests::install(&mut state);
    Arc::new(state)
}

async fn request(
    state: &Arc<AppState>,
    uri: &str,
    authorization: Option<HeaderValue>,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().uri(uri);
    if let Some(authorization) = authorization {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    let response = ShadowRoutes
        .routes()
        .into_router()
        .with_state(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    (status, headers, body)
}
