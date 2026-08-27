use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::AppState;
use peryx_ecosystem_pypi::store::{CachedIndex, PypiStore as _};
use peryx_ecosystem_pypi::upload::Uploaded;
use peryx_ecosystem_pypi::{CoreMetadata, File, Provenance, Yanked};
use peryx_identity::{GrantScope, Role};
use peryx_policy::{PolicyAction, PolicyDecisionState};
use peryx_storage::meta::NewPolicyDecision;
use tower::ServiceExt as _;

use crate::config::{Config, IndexConfig, IndexKind, SecretSource};
use crate::server::{build_state, router_for};

const PASSWORD: &str = "local password";
const HOSTED_FILE: &str = "acme_pkg-1.0-py3-none-any.whl";
const CACHED_FILE: &str = "acme_pkg-2.0-py3-none-any.whl";
const HOSTED_DIGEST: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const UPSTREAM_DIGEST: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const CACHED_DIGEST: &str = "3333333333333333333333333333333333333333333333333333333333333333";

fn cached_pypi() -> IndexConfig {
    IndexConfig {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        anonymous_read: None,
        tokens: Vec::new(),
        kind: IndexKind::Cached {
            routing: crate::support::single_route("http://127.0.0.1:9/simple/"),
            upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
            offline: true,
            prefetch: Box::default(),
        },
    }
}

fn hosted() -> IndexConfig {
    IndexConfig {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        anonymous_read: None,
        tokens: vec![crate::support::writer_token(SecretSource::Literal("s3cret".to_owned()))],
        kind: IndexKind::Hosted { volatile: true },
    }
}

fn virtual_root() -> IndexConfig {
    IndexConfig {
        name: "root-pypi".to_owned(),
        route: "root/pypi".to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        anonymous_read: None,
        tokens: Vec::new(),
        kind: IndexKind::Virtual {
            layers: vec!["hosted".to_owned(), "pypi".to_owned()],
            write_target: Some("hosted".to_owned()),
        },
    }
}

fn config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![cached_pypi(), hosted(), virtual_root()],
        ..Config::default()
    }
}

async fn provision_admin(state: &AppState) {
    let user = state.serving.users.create("Alice").unwrap();
    state.serving.users.set_password(&user.id, PASSWORD).await.unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::Administrator, GrantScope::Server)
        .unwrap();
}

fn hosted_file() -> File {
    File {
        filename: HOSTED_FILE.to_owned(),
        url: format!("https://files/{HOSTED_FILE}"),
        hashes: BTreeMap::from([("sha256".to_owned(), HOSTED_DIGEST.to_owned())]),
        requires_python: None,
        size: Some(1_024),
        upload_time: Some("2020-01-01T00:00:00Z".to_owned()),
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
    }
}

fn seed_hosted(state: &AppState) {
    let uploaded = Uploaded {
        version: "1.0".to_owned(),
        file: hosted_file(),
        trashed: None,
    };
    state
        .serving
        .meta
        .put_upload(
            "hosted",
            "acme-pkg",
            HOSTED_FILE,
            &serde_json::to_vec(&uploaded).unwrap(),
        )
        .unwrap();
    state
        .serving
        .meta
        .put_project("hosted", "acme-pkg", "acme-pkg")
        .unwrap();
}

fn seed_cached(state: &AppState) {
    let body = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"acme-pkg\",\"versions\":[\"1.0\",\"2.0\"],\"files\":[\
         {{\"filename\":\"{HOSTED_FILE}\",\"url\":\"https://upstream.invalid/{HOSTED_FILE}\",\
         \"hashes\":{{\"sha256\":\"{UPSTREAM_DIGEST}\"}}}},\
         {{\"filename\":\"{CACHED_FILE}\",\"url\":\"https://upstream.invalid/{CACHED_FILE}\",\
         \"hashes\":{{\"sha256\":\"{CACHED_DIGEST}\"}}}}]}}"
    );
    let record = CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 1000,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: None,
        body: body.into_bytes(),
    };
    state.serving.meta.put_index("pypi/acme-pkg", &record).unwrap();
}

async fn seeded_state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&config(&dir)).unwrap();
    provision_admin(&state).await;
    seed_hosted(&state);
    seed_cached(&state);
    (dir, state)
}

async fn seeded_state_named(repository: &str) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let mut root = virtual_root();
    root.name = repository.to_owned();
    root.route = repository.to_owned();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![cached_pypi(), hosted(), root],
        ..Config::default()
    };
    let state = build_state(&config).unwrap();
    provision_admin(&state).await;
    seed_hosted(&state);
    seed_cached(&state);
    (dir, state)
}

async fn app() -> (tempfile::TempDir, axum::Router) {
    let (dir, state) = seeded_state().await;
    (dir, router_for(state))
}

fn record_decision(
    state: &AppState,
    filename: Option<&str>,
    action: PolicyAction,
    decision: PolicyDecisionState,
    rule: &str,
    next_eligible_at_unix: Option<i64>,
) {
    state
        .serving
        .meta
        .record_policy_decision(NewPolicyDecision {
            repository: "root-pypi",
            resource: "acme-pkg",
            group: None,
            artifact: filename,
            source: Some("pypi"),
            action,
            state: decision,
            rule: Some(rule),
            reason: Some("policy note"),
            evaluated_at_unix: 0,
            next_eligible_at_unix,
        })
        .unwrap();
}

async fn get(
    router: &axum::Router,
    uri: &str,
    credential: Option<(&str, &str)>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut request = Request::builder().uri(uri);
    if let Some((user, password)) = credential {
        request = request.header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
        );
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    (status, headers, body)
}

async fn candidates(router: &axum::Router, uri: &str) -> serde_json::Value {
    let (status, headers, body) = get(router, uri, Some(("Alice", PASSWORD))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn test_mixed_members_report_the_selected_and_shadowed_candidates() {
    let (_dir, router) = app().await;

    let document = candidates(&router, "/+shadow/candidates?repository=root/pypi&project=acme-pkg").await;

    let rows: Vec<(&str, &str, bool, Option<&str>)> = document["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["member"].as_str().unwrap(),
                row["filename"].as_str().unwrap(),
                row["selected"].as_bool().unwrap(),
                row["reason"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("hosted", HOSTED_FILE, true, None),
            ("pypi", HOSTED_FILE, false, Some("precedence")),
            ("pypi", CACHED_FILE, true, None),
        ]
    );
    let winner = &document["candidates"][0];
    assert_eq!(winner["source"], "hosted");
    assert_eq!(winner["digest"], format!("sha256:{HOSTED_DIGEST}"));
    assert_eq!(document["candidates"][1]["source"], "cached");
    assert_eq!(document["candidates"][1]["digest"], format!("sha256:{UPSTREAM_DIGEST}"));
    assert_eq!(document["next_cursor"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_a_denied_filename_carries_its_decision_on_every_member_row() {
    let (_dir, state) = seeded_state().await;
    record_decision(
        &state,
        Some(HOSTED_FILE),
        PolicyAction::Upload,
        PolicyDecisionState::Allow,
        "upload-ok",
        None,
    );
    record_decision(
        &state,
        Some("acme_pkg-9.9-py3-none-any.whl"),
        PolicyAction::Serve,
        PolicyDecisionState::Deny,
        "other",
        None,
    );
    record_decision(
        &state,
        None,
        PolicyAction::Serve,
        PolicyDecisionState::Deny,
        "project-wide",
        None,
    );
    record_decision(
        &state,
        Some(HOSTED_FILE),
        PolicyAction::Serve,
        PolicyDecisionState::Deny,
        "blocked-project",
        None,
    );
    let router = router_for(state);

    let document = candidates(&router, "/+shadow/candidates?repository=root/pypi&project=acme-pkg").await;
    let rows = document["candidates"].as_array().unwrap();

    for member in ["hosted", "pypi"] {
        let row = rows
            .iter()
            .find(|row| row["filename"] == HOSTED_FILE && row["member"] == member)
            .expect("the member row is present");
        assert_eq!(
            row["decision"]["state"], "deny",
            "the serve decision governs the {member} row"
        );
        assert_eq!(row["decision"]["rule"], "blocked-project");
        assert_eq!(row["decision"]["reason"], "policy note");
        assert_eq!(row["decision"]["fresh"], true);
    }
    let cached = rows.iter().find(|row| row["filename"] == CACHED_FILE).unwrap();
    assert!(
        cached.get("decision").is_none(),
        "an unevaluated filename carries no decision: {cached}"
    );
}

#[tokio::test]
async fn test_a_failed_decision_read_surfaces_as_a_server_error() {
    let repository = "r".repeat(513);
    let (_dir, state) = seeded_state_named(&repository).await;
    let router = router_for(state);

    let (status, _headers, body) = get(
        &router,
        &format!("/+shadow/candidates?repository={repository}&project=acme-pkg"),
        Some(("Alice", PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    let document: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(document["error"], "shadow query failed");
}

#[tokio::test]
async fn test_a_waiting_filename_reports_its_retry_window() {
    let (_dir, state) = seeded_state().await;
    record_decision(
        &state,
        Some(CACHED_FILE),
        PolicyAction::Serve,
        PolicyDecisionState::Wait,
        "cooldown",
        Some(120),
    );
    let router = router_for(state);

    let document = candidates(&router, "/+shadow/candidates?repository=root/pypi&project=acme-pkg").await;
    let cached = document["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["filename"] == CACHED_FILE)
        .unwrap();

    assert_eq!(cached["decision"]["state"], "wait");
    assert_eq!(cached["decision"]["next_eligible_at_unix"], 120);
}

#[tokio::test]
async fn test_shadowed_candidates_stay_absent_from_installer_selection() {
    let (_dir, router) = app().await;

    let (status, _, page) = get(&router, "/root/pypi/simple/acme-pkg/", Some(("Alice", PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(HOSTED_DIGEST),
        "the hosted candidate wins the installer page: {page}"
    );
    assert!(
        page.contains(CACHED_DIGEST),
        "the distinct cached file still serves: {page}"
    );
    assert!(
        !page.contains(UPSTREAM_DIGEST),
        "the shadowed cached candidate must not appear in installer selection: {page}"
    );
}

#[tokio::test]
async fn test_pagination_walks_the_candidates_without_overlap() {
    let (_dir, router) = app().await;

    let first = candidates(
        &router,
        "/+shadow/candidates?repository=root/pypi&project=acme-pkg&limit=2",
    )
    .await;
    assert_eq!(first["candidates"].as_array().unwrap().len(), 2);
    let cursor = first["next_cursor"].as_str().expect("a third candidate remains");

    let second = candidates(
        &router,
        &format!(
            "/+shadow/candidates?repository=root/pypi&project=acme-pkg&limit=2&cursor={}",
            urlencoding(cursor)
        ),
    )
    .await;
    let rows = second["candidates"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["filename"], CACHED_FILE);
    assert_eq!(second["next_cursor"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_a_project_with_no_members_returns_an_empty_page() {
    let (_dir, router) = app().await;

    let document = candidates(&router, "/+shadow/candidates?repository=root/pypi&project=absent").await;

    assert_eq!(document, serde_json::json!({"candidates": [], "next_cursor": null}));
}

#[tokio::test]
async fn test_an_invalid_cursor_is_rejected() {
    let (_dir, router) = app().await;

    let (status, _, body) = get(
        &router,
        "/+shadow/candidates?repository=root/pypi&project=acme-pkg&cursor=",
        Some(("Alice", PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let document: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(document["error"], "invalid shadow cursor");
}

#[tokio::test]
async fn test_an_anonymous_caller_cannot_infer_shadowing() {
    let (_dir, router) = app().await;

    let (status, _, _) = get(
        &router,
        "/+shadow/candidates?repository=root/pypi&project=acme-pkg",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
