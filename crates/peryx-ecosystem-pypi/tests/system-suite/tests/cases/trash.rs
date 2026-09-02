use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_driver::state::ServingState;
use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_ecosystem_pypi::upload::{TrashInfo, Uploaded};
use peryx_ecosystem_pypi::{CoreMetadata, File, Provenance, Yanked};
use peryx_identity::{GrantScope, Role};
use peryx_storage::blob::Digest;
use tower::ServiceExt as _;

use crate::config::{Config, IndexConfig, IndexKind, SecretSource};
use crate::server::{build_state, router_for};

const UPLOAD_TOKEN: &str = "s3cret";
const PASSWORD: &str = "local password";

fn config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![hosted("hosted")],
        ..Config::default()
    }
}

fn hosted(name: &str) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
        anonymous_read: None,
        tokens: vec![crate::support::writer_token(SecretSource::Literal(
            UPLOAD_TOKEN.to_owned(),
        ))],
        kind: IndexKind::Hosted { volatile: true },
    }
}

async fn provision_users(state: &ServingState) {
    for (name, role, scope) in [
        ("Alice", Role::Administrator, GrantScope::Server),
        (
            "Rita",
            Role::RepositoryReader,
            GrantScope::Repository {
                name: "hosted".to_owned(),
            },
        ),
    ] {
        let user = state.users.create(name).unwrap();
        state.users.set_password(&user.id, PASSWORD).await.unwrap();
        state.authorization.grant(&user.id, role, scope).unwrap();
    }
}

fn seed_pypi_trash(state: &ServingState, filename: &str, deleted_at_unix: i64) {
    let uploaded = Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: filename.to_owned(),
            url: format!("https://files/{filename}"),
            hashes: BTreeMap::from([("sha256".to_owned(), Digest::of(filename.as_bytes()).as_str().to_owned())]),
            requires_python: None,
            size: Some(1_024),
            upload_time: Some("2020-01-01T00:00:00Z".to_owned()),
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
        trashed: Some(TrashInfo {
            deleted_at_unix,
            actor: Some("cleanup-bot".to_owned()),
            reason: Some("mistaken upload".to_owned()),
        }),
    };
    state
        .meta
        .put_upload("hosted", "flask", filename, &serde_json::to_vec(&uploaded).unwrap())
        .unwrap();
}

async fn app() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&config(&dir)).unwrap();
    provision_users(&state.serving).await;
    seed_pypi_trash(&state.serving, "flask-1.0.whl", 1_000);
    seed_pypi_trash(&state.serving, "flask-2.0.whl", 2_000);
    (dir, router_for(state, axum::Router::new()))
}

async fn get(
    router: &axum::Router,
    uri: &str,
    credential: (&str, &str),
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let (user, password) = credential;
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(
                    header::AUTHORIZATION,
                    format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn records(document: &serde_json::Value) -> &Vec<serde_json::Value> {
    document["trash"].as_array().unwrap()
}

#[tokio::test]
async fn test_administrator_lists_pypi_trash_with_actor() {
    let (_dir, router) = app().await;

    let (status, headers, document) = get(&router, "/+trash", ("Alice", PASSWORD)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let seen: Vec<(&str, &str, &str, Option<&str>)> = records(&document)
        .iter()
        .map(|record| {
            (
                record["ecosystem"].as_str().unwrap(),
                record["repository"].as_str().unwrap(),
                record["artifact"].as_str().unwrap(),
                record["actor"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            ("pypi", "hosted", "flask-2.0.whl", Some("cleanup-bot")),
            ("pypi", "hosted", "flask-1.0.whl", Some("cleanup-bot")),
        ]
    );
}

#[tokio::test]
async fn test_repository_reader_sees_records_without_actor() {
    let (_dir, router) = app().await;

    let (status, _, document) = get(&router, "/+trash?repository=hosted", ("Rita", PASSWORD)).await;

    assert_eq!(status, StatusCode::OK);
    let record = &records(&document)[0];
    assert_eq!(record["ecosystem"], "pypi");
    assert_eq!(record["reason"], "mistaken upload");
    assert!(
        record.get("actor").is_none(),
        "the role filter redacts the actor: {record}"
    );
}

#[tokio::test]
async fn test_ecosystem_filter_narrows_to_one_ecosystem() {
    let (_dir, router) = app().await;

    let (_, _, document) = get(&router, "/+trash?ecosystem=pypi", ("Alice", PASSWORD)).await;

    let ecosystems: Vec<&str> = records(&document)
        .iter()
        .map(|record| record["ecosystem"].as_str().unwrap())
        .collect();
    assert_eq!(ecosystems, vec!["pypi", "pypi"]);
}

#[tokio::test]
async fn test_pagination_is_stable_across_pages() {
    let (_dir, router) = app().await;

    let (_, _, first) = get(&router, "/+trash?limit=1", ("Alice", PASSWORD)).await;
    assert_eq!(records(&first).len(), 1);
    let cursor = first["next_cursor"].as_str().expect("a second record remains");

    let (_, _, second) = get(
        &router,
        &format!("/+trash?limit=1&cursor={}", urlencoding(cursor)),
        ("Alice", PASSWORD),
    )
    .await;
    assert_eq!(records(&second).len(), 1);
    assert_ne!(
        records(&first)[0]["artifact"],
        records(&second)[0]["artifact"],
        "the pages do not overlap"
    );
    assert_eq!(second["next_cursor"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_inspect_returns_one_record() {
    let (_dir, router) = app().await;

    let digest = Digest::of(b"flask-1.0.whl");
    let (status, _, document) = get(
        &router,
        &format!(
            "/+trash/record?ecosystem=pypi&repository=hosted&resource=flask&artifact=flask-1.0.whl&digest=sha256:{}",
            digest.as_str()
        ),
        ("Alice", PASSWORD),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["record"]["artifact"], "flask-1.0.whl");
    assert_eq!(document["record"]["actor"], "cleanup-bot");
}

#[tokio::test]
async fn test_web_view_renders_the_trash_page() {
    let (_dir, router) = app().await;

    let response = router
        .clone()
        .oneshot(Request::builder().uri("/admin/trash").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(body.contains("Trash"), "{body}");
    assert!(body.contains("Restorable"), "{body}");
}

#[tokio::test]
async fn test_a_corrupt_record_surfaces_a_store_error() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&config(&dir)).unwrap();
    provision_users(&state.serving).await;
    state
        .serving
        .meta
        .put_upload("hosted", "flask", "flask-1.0.whl", b"not json")
        .unwrap();
    let router = router_for(state, axum::Router::new());

    let (list, _, _) = get(&router, "/+trash", ("Alice", PASSWORD)).await;
    assert_eq!(list, StatusCode::INTERNAL_SERVER_ERROR);

    let (inspect, _, document) = get(
        &router,
        "/+trash/record?ecosystem=pypi&repository=hosted&resource=flask",
        ("Alice", PASSWORD),
    )
    .await;
    assert_eq!(inspect, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(document["error"], "trash query failed");
}

fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
