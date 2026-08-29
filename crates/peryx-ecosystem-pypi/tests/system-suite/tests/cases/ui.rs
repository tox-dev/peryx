use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write as _;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use peryx_core::path::local_artifact_url;
use peryx_ecosystem_pypi::store::{CachedIndex, CachedPageWrite, PypiStore as _};
use peryx_ecosystem_pypi::upload::Uploaded;
use peryx_ecosystem_pypi::{CoreMetadata, File, Provenance, Yanked, to_json};
use peryx_ha::{ArtifactPlacement, ArtifactPlacementStore, ArtifactSource};
use peryx_identity::Action;
use peryx_storage::blob::Digest;
use rstest::{fixture, rstest};
use sha2::{Digest as _, Sha256};
use tower::ServiceExt as _;

use super::{get, get_authorized, get_with_origin, seed_administrator};
use crate::config::{Config, IndexConfig, IndexKind, SecretSource, TokenConfig};
use crate::server::{build_router, build_state, router_for};

#[fixture]
fn ui_router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir, false)).unwrap();
    (dir, router)
}

#[fixture]
fn filter_router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir, false)).unwrap();
    put_filter_files(&state);
    (dir, router_for(state))
}

fn ui_config(dir: &tempfile::TempDir, cached_offline: bool) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![
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
                    routing: crate::tests::single_route("http://127.0.0.1:9/simple/"),
                    upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
                    offline: cached_offline,
                    prefetch: Box::default(),
                },
            },
            IndexConfig {
                name: "hosted".to_owned(),
                route: "hosted".to_owned(),
                policy: peryx_policy::PolicyConfig::default(),
                ecosystem_policy: toml::Table::new(),
                ecosystem_settings: toml::Table::new(),
                webhooks: Vec::new(),
                ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
                anonymous_read: None,
                tokens: vec![crate::tests::writer_token(SecretSource::Literal("s3cret".to_owned()))],
                kind: IndexKind::Hosted { volatile: true },
            },
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
            },
        ],
        ..Config::default()
    }
}

fn reader_authorization() -> String {
    format!("Basic {}", STANDARD.encode("_:read-secret"))
}

fn other_reader_authorization() -> String {
    format!("Basic {}", STANDARD.encode("_:other-read-secret"))
}

#[fixture]
fn private_pypi_ui_router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&private_pypi_ui_config(&dir)).unwrap();
    (dir, router)
}

fn private_pypi_ui_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![IndexConfig {
            name: "vault".to_owned(),
            route: "vault".to_owned(),
            policy: peryx_policy::PolicyConfig::default(),
            ecosystem_policy: toml::Table::new(),
            ecosystem_settings: toml::Table::new(),
            webhooks: Vec::new(),
            ecosystem: peryx_core::Ecosystem::new("pypi"),
            anonymous_read: Some(false),
            tokens: vec![
                crate::tests::writer_token(SecretSource::Literal("s3cret".to_owned())),
                TokenConfig {
                    name: "reader".to_owned(),
                    secret: SecretSource::Literal("read-secret".to_owned()),
                    resources: vec!["*".to_owned()],
                    actions: BTreeSet::from([Action::Read]),
                    expires_at: None,
                },
                TokenConfig {
                    name: "other-reader".to_owned(),
                    secret: SecretSource::Literal("other-read-secret".to_owned()),
                    resources: vec!["other".to_owned()],
                    actions: BTreeSet::from([Action::Read]),
                    expires_at: None,
                },
            ],
            kind: IndexKind::Hosted { volatile: true },
        }],
        ..Config::default()
    }
}

async fn upload_private_fixture(router: &axum::Router) -> String {
    let wheel = include_bytes!("../../../fixtures/veloxdemo-1.0.0-py3-none-any.whl");
    let boundary = "peryxuitest";
    let sha256 = Digest::of(wheel);
    let mut body = Vec::new();
    for (name, value) in [
        (":action", "file_upload"),
        ("name", "veloxdemo"),
        ("version", "1.0.0"),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", sha256.as_str()),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        b"--peryxuitest\r\nContent-Disposition: form-data; name=\"content\"; \
          filename=\"veloxdemo-1.0.0-py3-none-any.whl\"\r\n\r\n",
    );
    body.extend_from_slice(wheel);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .uri("/vault/")
        .method("POST")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("__token__:s3cret")),
        )
        .body(Body::from(body))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    sha256.as_str().to_owned()
}

async fn ui_router_admin() -> (tempfile::TempDir, axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir, false)).unwrap();
    let authorization = seed_administrator(&state).await;
    (dir, router_for(state), authorization)
}

async fn ui_router_admin_stateful() -> (
    tempfile::TempDir,
    std::sync::Arc<peryx_driver::AppState>,
    axum::Router,
    String,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir, false)).unwrap();
    let authorization = seed_administrator(&state).await;
    (dir, state.clone(), router_for(state), authorization)
}

fn rendered_main(body: &str) -> &str {
    body.split_once("<main>")
        .and_then(|(_, main)| main.split_once("</main>"))
        .map(|(main, _)| main)
        .expect("page renders one main element")
}

fn rendered_files(body: &str) -> &str {
    body.rsplit_once(r#"<table class="browse-table""#)
        .and_then(|(_, table)| table.split_once("</table>"))
        .map(|(table, _)| table)
        .expect("project page renders a files table")
}

async fn upload_fixture(router: &axum::Router) {
    let wheel = include_bytes!("../../../fixtures/veloxdemo-1.0.0-py3-none-any.whl");
    upload_file(router, "veloxdemo-1.0.0-py3-none-any.whl", wheel).await;
}

async fn upload_file(router: &axum::Router, filename: &str, content: &[u8]) {
    let boundary = "peryxuitest";
    let mut body = Vec::new();
    let sha256 = Digest::of(content);
    for (name, value) in [
        (":action", "file_upload"),
        ("name", "veloxdemo"),
        ("version", "1.0.0"),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", sha256.as_str()),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; \
             filename=\"{filename}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .uri("/root/pypi/")
        .method("POST")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("__token__:s3cret")),
        )
        .body(Body::from(body))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

fn put_file(state: &peryx_driver::AppState, filename: &str, content: &[u8], core_metadata: CoreMetadata) -> Digest {
    let digest = Digest::of(content);
    state.serving.blobs.blocking().put_bytes_as(content, &digest).unwrap();
    let uploaded = Uploaded {
        version: "1.0.0".to_owned(),
        file: File {
            filename: filename.to_owned(),
            url: local_artifact_url("hosted", digest.as_str(), filename),
            hashes: std::collections::BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
            requires_python: None,
            size: Some(content.len() as u64),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
        trashed: None,
    };
    state
        .serving
        .meta
        .put_upload("hosted", "veloxdemo", filename, &to_json(&uploaded).into_bytes())
        .unwrap();
    state
        .serving
        .meta
        .put_project("hosted", "veloxdemo", "veloxdemo")
        .unwrap();
    digest
}

fn put_legacy_file(state: &peryx_driver::AppState, filename: &str, content: &[u8]) -> Digest {
    put_file(state, filename, content, CoreMetadata::Absent)
}

fn put_filter_files(state: &peryx_driver::AppState) {
    put_legacy_file(state, "veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl", b"wheel 1");
    put_legacy_file(state, "veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl", b"wheel 2");
    put_legacy_file(state, "veloxdemo-1.0.0.tar.gz", b"sdist");
}

#[tokio::test]
async fn test_ui_dashboard_renders_indexes_and_counters() {
    let (_dir, router, authorization) = ui_router_admin().await;
    let (status, body) = get_authorized(&router, "/", &authorization).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        [
            body.contains("change serial"),
            body.contains("root/pypi"),
            body.contains("badge kind-virtual"),
            body.contains("badge uploads"),
            body.contains("layer-stack"),
            body.contains("writes land here"),
            body.contains("resolves top to bottom"),
            body.contains("/pkg/peryx_web.js"),
        ],
        [true; 8],
        "{body}"
    );
}

#[rstest]
#[tokio::test]
async fn test_ui_dashboard_withholds_counters_from_anonymous(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    let (status, body) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("root/pypi"), "{body}");
    assert!(!body.contains("PEP 658"), "{body}");
    assert!(!body.contains("metadata hits"), "{body}");
}

#[rstest]
#[tokio::test]
async fn test_ui_admin_status_renders_read_only_state_without_secrets() {
    let (_dir, router, authorization) = ui_router_admin().await;
    let (status, body) = get_authorized(&router, "/admin/status", &authorization).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Admin status"));
    assert!(body.contains("read-only"));
    assert!(body.contains("root/pypi"));
    assert!(body.contains("/root/pypi/simple/"));
    assert!(body.contains("/browse?index=hosted"));
    assert!(body.contains("Usage and health"));
    assert!(body.contains("Recent writes"));
    assert!(body.contains("No writes recorded yet."));
    assert!(body.contains("token configured"));
    assert!(body.contains("redacted"));
    assert!(body.contains("http://127.0.0.1:9/simple/"));
    assert!(body.contains("upload-enabled"));
    assert!(!body.contains("s3cret"));
    assert!(!body.contains("type=\"password\""));
    assert!(!body.contains("delete whole project"));
}

#[rstest]
#[tokio::test]
async fn test_ui_admin_status_withholds_sensitive_fields_from_anonymous(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    let (status, body) = get(&router, "/admin/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Admin status"));
    assert!(body.contains("root/pypi"), "{body}");
    assert!(!body.contains("http://127.0.0.1:9/simple/"), "{body}");
    assert!(!body.contains("token configured"), "{body}");
    assert!(!body.contains("redacted"), "{body}");
}

#[rstest]
#[tokio::test]
async fn test_ui_admin_status_lists_counts_and_recent_uploads() {
    let (_dir, router, authorization) = ui_router_admin().await;
    upload_fixture(&router).await;
    let (status, body) = get_authorized(&router, "/admin/status", &authorization).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("uploads"));
    assert!(body.contains("veloxdemo"));
    assert!(body.contains("veloxdemo-1.0.0-py3-none-any.whl"));
    assert!(body.contains("1.2 kB"));
    assert!(!body.contains("A demonstration package"));
}

#[rstest]
#[tokio::test]
async fn test_ui_browse_lists_projects_after_upload(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    upload_fixture(&router).await;
    let (status, body) = get(&router, "/browse?index=hosted").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"class="links-list""#));
    assert!(body.contains(r#"href="/browse?index=hosted&amp;project=veloxdemo""#));
}

#[tokio::test]
async fn test_ui_project_command_uses_trusted_request_origin() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ui_config(&dir, false);
    config.rate_limit.trusted_proxies = vec!["127.0.0.1/32".parse().unwrap()];
    let router = build_router(&config).unwrap();
    upload_fixture(&router).await;
    let uri = "/browse?index=hosted&project=veloxdemo";
    let (_, untrusted) = get_with_origin(&router, uri, None).await;
    let (_, trusted) = get_with_origin(&router, uri, Some("127.0.0.1:443")).await;
    let (_, json) = get_with_origin(&router, &format!("/+ui{uri}"), Some("127.0.0.1:443")).await;
    let command = serde_json::from_str::<serde_json::Value>(&json).unwrap()["command"]
        .as_str()
        .unwrap()
        .to_owned();

    assert_eq!(
        (
            untrusted.contains("--index-url http://internal.test:8080/hosted/simple/"),
            trusted.contains("--index-url https://packages.example/hosted/simple/"),
            command,
        ),
        (
            true,
            true,
            "uv pip install --index-url https://packages.example/hosted/simple/ veloxdemo==1.0.0".to_owned()
        )
    );
}

#[rstest]
#[tokio::test]
async fn test_ui_browse_empty_index_hint(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    let (status, body) = get(&router, "/browse?index=hosted").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No projects observed"));
}

#[tokio::test]
async fn test_ui_project_page_shows_source_and_availability_cells() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir, true)).unwrap();
    let cached = Digest::of(b"a cached wheel");
    ArtifactPlacementStore::insert_artifact_placement(
        &state.serving.meta,
        cached.as_str(),
        &ArtifactPlacement::record(ArtifactSource::Proxy, true),
    )
    .unwrap();
    let remote = Digest::of(b"a wheel this proxy has never fetched");
    let detail = serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "veloxdemo",
        "versions": ["1.0"],
        "files": [
            {
                "filename": "veloxdemo-1.0-py3-none-any.whl",
                "url": "https://files.example/veloxdemo-1.0-py3-none-any.whl",
                "hashes": {"sha256": cached.as_str()},
            },
            {
                "filename": "veloxdemo-1.0.tar.gz",
                "url": "https://files.example/veloxdemo-1.0.tar.gz",
                "hashes": {"sha256": remote.as_str()},
            },
        ],
    });
    state
        .serving
        .meta
        .put_index(
            "pypi/veloxdemo",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 0,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: None,
                body: serde_json::to_vec(&detail).unwrap(),
            },
        )
        .unwrap();
    let router = router_for(state);
    let (status, body) = get(&router, "/browse?index=pypi&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rendered_file_cells(&body),
        vec![
            (
                "veloxdemo-1.0-py3-none-any.whl".to_owned(),
                "proxy".to_owned(),
                "local".to_owned()
            ),
            (
                "veloxdemo-1.0.tar.gz".to_owned(),
                "proxy".to_owned(),
                "remote_only".to_owned(),
            ),
        ],
    );
}

fn rendered_file_cells(body: &str) -> Vec<(String, String, String)> {
    body.split("__RESOLVED_RESOURCES[")
        .filter_map(|resource| resource.split_once(" = ").map(|(_, assignment)| assignment))
        .filter_map(|assignment| assignment.split_once(';').map(|(value, _)| value))
        .filter_map(|value| serde_json::from_str::<String>(value).ok())
        .filter_map(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
        .find_map(|resource| {
            resource["Ok"]["sections"].as_array()?.iter().find_map(|section| {
                (section["heading"] == "Files").then(|| {
                    section["rows"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|row| {
                            let cells = row["cells"].as_array().unwrap();
                            (
                                cells[0]["text"].as_str().unwrap().to_owned(),
                                cells[4]["text"].as_str().unwrap().to_owned(),
                                cells[5]["text"].as_str().unwrap().to_owned(),
                            )
                        })
                        .collect()
                })
            })
        })
        .unwrap()
}

#[tokio::test]
async fn test_ui_project_page_selects_latest_pep440_version() {
    let (_dir, router) = version_router(&["2.0", "1!1.0rc1", "10.0", "1!1.0.post01", "1!1.0.post1", "1.0"]);
    let (status, body) = get(&router, "/browse?index=pypi&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"<p class="browse-subtitle">1!1.0.post1</p>"#), "{body}");
}

#[rstest]
#[case::ascending(&["legacy-a", "legacy-z"])]
#[case::descending(&["legacy-z", "legacy-a"])]
#[tokio::test]
async fn test_ui_project_page_selects_stable_legacy_version(#[case] versions: &[&str]) {
    let (_dir, router) = version_router(versions);
    let (status, body) = get(&router, "/browse?index=pypi&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"<p class="browse-subtitle">legacy-z</p>"#), "{body}");
}

#[tokio::test]
async fn test_ui_project_page_groups_each_file_under_one_ordered_release() {
    let (_dir, router) = detail_router(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "veloxdemo",
        "versions": ["1.0", "2.0rc1", "2.0", "2.0+local.1", "legacy"],
        "files": [
            {"filename": "veloxdemo-1.0-py3-none-any.whl", "url": "/files/1.0.whl"},
            {"filename": "veloxdemo-2.0rc1-py3-none-any.whl", "url": "/files/2.0rc1.whl"},
            {"filename": "veloxdemo-2.0-py3-none-any.whl", "url": "/files/2.0.whl"},
            {"filename": "veloxdemo-2.0+local.1-py3-none-any.whl", "url": "/files/2.0-local.whl"},
            {"filename": "veloxdemo-legacy-py3-none-any.whl", "url": "/files/legacy.whl"},
            {"filename": "notes.txt", "url": "/files/notes.txt"},
        ],
    }));

    let (status, body) = get(&router, "/browse?index=pypi&project=veloxdemo").await;

    assert_eq!(status, StatusCode::OK);
    let main = rendered_main(&body);
    let releases = ["2.0%2Blocal.1", "2.0", "2.0rc1", "1.0", "legacy"];
    let positions: Vec<usize> = releases
        .iter()
        .map(|release| {
            let href = format!(r#"href="/browse?index=pypi&amp;project=veloxdemo&amp;version={release}""#);
            main.find(&href).expect("release link is present")
        })
        .collect();
    let mut sorted_positions = positions.clone();
    sorted_positions.sort_unstable();
    assert_eq!(positions, sorted_positions, "{body}");
    let files = rendered_files(main);
    assert_eq!(files.matches("<tr>").count(), 7, "{main}");
    assert_eq!(
        [
            files.contains("veloxdemo-1.0-py3-none-any.whl"),
            files.contains("veloxdemo-2.0rc1-py3-none-any.whl"),
            files.contains("veloxdemo-2.0-py3-none-any.whl"),
            files.contains("veloxdemo-2.0+local.1-py3-none-any.whl"),
            files.contains("veloxdemo-legacy-py3-none-any.whl"),
            files.contains("notes.txt"),
        ],
        [true; 6],
        "{main}"
    );
}

#[tokio::test]
async fn test_ui_project_page_keeps_ambiguous_equivalent_releases_unassociated() {
    let (_dir, router) = detail_router(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "veloxdemo",
        "versions": ["1.0", "1.0.0"],
        "files": [{"filename": "veloxdemo-1.0-py3-none-any.whl", "url": "/files/demo.whl"}],
    }));

    let (status, body) = get(&router, "/browse?index=pypi&project=veloxdemo").await;

    assert_eq!(status, StatusCode::OK);
    let main = rendered_main(&body);
    let files = rendered_files(main);
    assert_eq!(
        (
            main.matches("/browse?index=pypi&amp;project=veloxdemo&amp;version=1.0\"")
                .count(),
            main.matches("/browse?index=pypi&amp;project=veloxdemo&amp;version=1.0.0\"")
                .count(),
            files.matches("<tr>").count(),
            files.contains("veloxdemo-1.0-py3-none-any.whl"),
            !files.contains(">1.0</span>"),
        ),
        (1, 1, 2, true, true),
        "{main}"
    );
}

#[tokio::test]
async fn test_ui_project_page_selects_an_empty_declared_release() {
    let (_dir, router) = detail_router(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "veloxdemo",
        "versions": ["1.0", "2.0"],
        "files": [{"filename": "veloxdemo-1.0-py3-none-any.whl", "url": "/files/demo.whl"}],
    }));

    let (status, body) = get(&router, "/browse?index=pypi&project=veloxdemo&version=2.0").await;

    assert_eq!(status, StatusCode::OK);
    let main = rendered_main(&body);
    assert_eq!(
        [
            main.contains(r#"<p class="browse-subtitle">2.0</p>"#),
            main.contains(r#"href="/browse?index=pypi&amp;project=veloxdemo&amp;version=2.0""#),
            main.contains("No files match this query."),
            !main.contains("veloxdemo-1.0-py3-none-any.whl"),
            !main.contains("is not listed for this project"),
        ],
        [true; 5],
        "{main}"
    );
}

#[tokio::test]
async fn test_ui_project_page_distinguishes_an_unknown_release() {
    let (_dir, router) = version_router(&["1.0"]);

    let (status, body) = get(&router, "/browse?index=pypi&project=veloxdemo&version=missing").await;

    assert_eq!(status, StatusCode::OK);
    let main = rendered_main(&body);
    assert_eq!(
        [
            main.contains(r#"<p class="browse-subtitle">1.0</p>"#),
            main.contains(r#"href="/browse?index=pypi&amp;project=veloxdemo&amp;version=1.0""#),
            main.contains("No files match this query."),
            !main.contains("missing</span>"),
        ],
        [true; 4],
        "{main}"
    );
}

fn version_router(versions: &[&str]) -> (tempfile::TempDir, axum::Router) {
    detail_router(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "veloxdemo",
        "versions": versions,
        "files": [],
    }))
}

fn detail_router(detail: &serde_json::Value) -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir, true)).unwrap();
    state
        .serving
        .meta
        .put_index(
            "pypi/veloxdemo",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 0,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: None,
                body: serde_json::to_vec(detail).unwrap(),
            },
        )
        .unwrap();
    (dir, router_for(state))
}

#[tokio::test]
async fn test_ui_project_page_renders_metadata_and_sanitizes_description_links() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir, false)).unwrap();
    let metadata = concat!(
        "Metadata-Version: 2.1\n",
        "Name: veloxdemo\n",
        "Version: 1.0.0\n",
        "Project-URL: Documentation, https://example.com/docs\n",
        "Project-URL: Unsafe, JaVaScRiPt:alert(1)\n",
        "Description-Content-Type: text/markdown\n\n",
        "[guide](https://example.com/guide) [unsafe](data:text/html;base64,PHNjcmlwdD4=)\n",
    );
    let metadata_digest = state.serving.blobs.put_bytes(metadata.as_bytes()).await.unwrap();
    put_file(
        &state,
        "veloxdemo-1.0.0-py3-none-any.whl",
        &wheel_with_metadata(metadata),
        CoreMetadata::Hashes(std::collections::BTreeMap::from([(
            "sha256".to_owned(),
            metadata_digest.as_str().to_owned(),
        )])),
    );
    let router = router_for(state);
    let (status, body) = get(&router, "/browse?index=hosted&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    let main = rendered_main(&body);
    let links = main
        .split_once("<h2>Links</h2>")
        .and_then(|(_, links)| links.split_once("</section>"))
        .map(|(links, _)| links)
        .expect("metadata renders a links section");
    let description = main
        .split_once("<h2>Description</h2>")
        .and_then(|(_, description)| description.split_once("</section>"))
        .map(|(description, _)| description)
        .expect("metadata renders a description section");
    assert_eq!(
        [
            links.contains("href=\"https://example.com/docs\""),
            links.contains("Unsafe"),
            !links.contains(r#"href="JaVaScRiPt:alert(1)""#),
            description.contains("href=\"https://example.com/guide\""),
            description.contains("rel=\"external nofollow noopener noreferrer\""),
            description.contains(">guide</a>"),
            description.contains(" unsafe</p>"),
            !description.contains("href=\"data:text/html"),
        ],
        [true; 8],
        "{body}"
    );
}

#[rstest]
#[case::javascript("JaVaScRiPt:alert(1)", false)]
#[case::data("data:text/html;base64,PHNjcmlwdD4=", false)]
#[case::mailto("mailto:maintainer@example.com", false)]
#[case::malformed("http://[invalid", false)]
#[case::http("http://example.com/veloxdemo.whl", true)]
#[case::https("https://example.com/veloxdemo.whl", true)]
#[case::relative("/pypi/files/veloxdemo.whl", true)]
#[tokio::test]
async fn test_ui_project_page_sanitizes_artifact_links(#[case] url: &str, #[case] linked: bool) {
    let (_dir, router) = artifact_router(url);
    let (status, body) = get(&router, "/browse?index=pypi&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (
            body.contains(ARTIFACT_FILENAME),
            body.contains(&format!("href=\"{url}\""))
        ),
        (true, linked),
        "{body}"
    );
}

#[rstest]
#[case::http("http://example.com/veloxdemo.whl", true)]
#[case::https("https://example.com/veloxdemo.whl", true)]
#[case::local_route("/pypi/files/veloxdemo.whl", false)]
#[tokio::test]
async fn test_ui_project_page_marks_outbound_artifact_links_external(#[case] url: &str, #[case] external: bool) {
    let (_dir, router) = artifact_router(url);
    let (status, body) = get(&router, "/browse?index=pypi&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.split_once(&format!("href=\"{url}\""))
            .and_then(|(_, link)| link.split_once("</a>"))
            .is_some_and(|(link, _)| link.contains(&format!("rel=\"{EXTERNAL_LINK_REL}\""))),
        external,
        "{body}"
    );
}

const EXTERNAL_LINK_REL: &str = "external nofollow noopener noreferrer";
const ARTIFACT_FILENAME: &str = "veloxdemo-1.0.tar.bz2";
const ARCHIVE_DIGEST: &str = "5a105e8b9d40e1329780d62ea2265d8a4d4ef6a0d4b2f6c0c1a5b9a0f0d1c2e3";

#[rstest]
#[case::wheel("veloxdemo-1.0-py3-none-any.whl", ARCHIVE_DIGEST, true)]
#[case::zip("veloxdemo-1.0.zip", ARCHIVE_DIGEST, true)]
#[case::egg("veloxdemo-1.0.egg", ARCHIVE_DIGEST, true)]
#[case::tar("veloxdemo-1.0.tar", ARCHIVE_DIGEST, true)]
#[case::tar_gz("veloxdemo-1.0.tar.gz", ARCHIVE_DIGEST, true)]
#[case::tgz("veloxdemo-1.0.tgz", ARCHIVE_DIGEST, true)]
#[case::unsupported_format("veloxdemo-1.0.tar.bz2", ARCHIVE_DIGEST, false)]
#[case::digest_free_wheel("veloxdemo-1.0-py3-none-any.whl", "", true)]
#[case::truncated_digest_wheel("veloxdemo-1.0-py3-none-any.whl", "5a105e8b9d40e132", true)]
#[tokio::test]
async fn test_ui_project_page_links_contents_only_for_browsable_archives(
    #[case] filename: &str,
    #[case] sha256: &str,
    #[case] browsable: bool,
) {
    let hashes = if sha256.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({"sha256": sha256})
    };
    let (_dir, router) = file_router(&serde_json::json!({
        "filename": filename,
        "url": format!("https://example.com/{filename}"),
        "hashes": hashes,
    }));
    let (status, body) = get(&router, "/browse?index=pypi&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(filename), "{body}");
    assert_eq!(
        body.contains(&format!(
            r#"href="/browse?index=pypi&amp;project=veloxdemo&amp;sha256={sha256}&amp;file={filename}""#
        )),
        browsable,
        "{body}"
    );
    assert!(!body.contains("class=\"inspect\""), "{body}");
}

fn artifact_router(url: &str) -> (tempfile::TempDir, axum::Router) {
    file_router(&serde_json::json!({
        "filename": ARTIFACT_FILENAME,
        "url": url,
        "hashes": {"sha256": ARCHIVE_DIGEST},
    }))
}

fn file_router(file: &serde_json::Value) -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir, true)).unwrap();
    let record = CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 0,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: None,
        body: serde_json::to_vec(&serde_json::json!({
            "meta": {"api-version": "1.1"},
            "name": "veloxdemo",
            "versions": ["1.0"],
            "files": [file],
        }))
        .unwrap(),
    };
    let url = file["url"].as_str().filter(|url| {
        url.starts_with('/') || url::Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
    });
    let files = file["hashes"]["sha256"]
        .as_str()
        .filter(|sha256| !sha256.is_empty())
        .zip(url)
        .map(|(sha256, url)| vec![(sha256.to_owned(), url.to_owned(), None)])
        .unwrap_or_default();
    state
        .serving
        .meta
        .put_cached_page(CachedPageWrite {
            key: "pypi/veloxdemo",
            record: &record,
            index: "pypi",
            normalized: "veloxdemo",
            display: "veloxdemo",
            source: "pypi",
            upstream: url,
            project_status: None,
            project_status_reason: None,
            files: &files,
            metadata: &[],
            attestations: &[],
        })
        .unwrap();
    (dir, router_for(state))
}

fn wheel_with_metadata(metadata: &str) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    let dist_info = "veloxdemo-1.0.0.dist-info";
    let entries = [
        (format!("{dist_info}/METADATA"), metadata.as_bytes().to_vec()),
        (
            format!("{dist_info}/WHEEL"),
            b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n".to_vec(),
        ),
    ];
    for (path, bytes) in &entries {
        zip.start_file(path, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    let record_path = format!("{dist_info}/RECORD");
    zip.start_file(&record_path, options).unwrap();
    zip.write_all(wheel_record(&entries, &record_path).as_bytes()).unwrap();
    zip.finish().unwrap().into_inner()
}

#[rstest]
#[case::substring(
    "/browse?index=hosted&project=veloxdemo&filename=cp312",
    None,
    &["veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl"][..],
    &["veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl", "veloxdemo-1.0.0.tar.gz"][..]
)]
#[case::regex(
    "/browse?index=hosted&project=veloxdemo&filename=cp31%5B12%5D.*whl&filename_match=regex",
    None,
    &["veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl", "veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl"][..],
    &["veloxdemo-1.0.0.tar.gz"][..]
)]
#[case::invalid_regex(
    "/browse?index=hosted&project=veloxdemo&filename=%5B&filename_match=regex",
    Some("invalid regex: regex parse error:"),
    &[][..],
    &[][..]
)]
#[tokio::test]
async fn test_ui_project_page_filters_files(
    filter_router: (tempfile::TempDir, axum::Router),
    #[case] query: &str,
    #[case] error: Option<&str>,
    #[case] present: &[&str],
    #[case] absent: &[&str],
) {
    let (_dir, router) = filter_router;
    let (status, body) = get(&router, query).await;
    assert_eq!(status, StatusCode::OK);
    if let Some(error) = error {
        assert!(body.contains(error), "{body}");
        assert!(!body.contains(r#"class="browse-table""#), "{body}");
        return;
    }
    let table = rendered_files(&body);
    assert_eq!(table.matches("<tr>").count(), present.len() + 1, "{body}");
    for file in present {
        assert!(table.contains(file), "{body}");
    }
    for file in absent {
        assert!(!table.contains(file), "{body}");
    }
}

#[rstest]
#[tokio::test]
async fn test_ui_project_page_missing_project(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    let (status, body) = get(&router, "/browse?index=hosted&project=ghost").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Nothing matched this browse query."));
}

#[tokio::test]
async fn test_ui_project_page_shows_contents_for_zipped_eggs() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir, false)).unwrap();
    let mut egg = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut egg));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("EGG-INFO/PKG-INFO", options).unwrap();
        zip.write_all(b"Metadata-Version: 1.2\nName: veloxdemo\nVersion: 1.0.0\n")
            .unwrap();
        zip.finish().unwrap();
    }
    let digest = put_legacy_file(&state, "veloxdemo-1.0.0.egg", &egg);
    let router = router_for(state);
    let listing_url = format!(
        "/browse?index=hosted&project=veloxdemo&sha256={}&file=veloxdemo-1.0.0.egg",
        digest.as_str()
    );
    let (status, body) = get(&router, "/browse?index=hosted&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&format!("href=\"{}\"", listing_url.replace('&', "&amp;"))),
        "{body}"
    );

    let (status, body) = get(&router, &listing_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("EGG-INFO/PKG-INFO"), "{body}");
}

#[tokio::test]
async fn test_ui_project_page_hides_contents_for_unsupported_legacy_tar() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir, false)).unwrap();
    let digest = put_legacy_file(&state, "veloxdemo-1.0.0.tar.bz2", b"legacy tarball");
    let router = router_for(state);
    let (status, body) = get(&router, "/browse?index=hosted&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("veloxdemo-1.0.0.tar.bz2"));
    assert!(!body.contains(&format!("sha256={}", digest.as_str())));
}

#[rstest]
#[tokio::test]
async fn test_ui_archive_listing_and_member(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    upload_fixture(&router).await;
    let (_, detail) = get(&router, "/hosted/simple/veloxdemo/").await;
    let sha = detail
        .split("files/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();

    let file = "veloxdemo-1.0.0-py3-none-any.whl";
    let listing_url = format!("/browse?index=hosted&project=veloxdemo&sha256={sha}&file={file}");
    let (status, listing) = get(&router, &listing_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("dist-info/METADATA"));
    assert!(listing.contains("__init__.py"));

    let member = format!("{listing_url}&member=veloxdemo-1.0.0.dist-info%2FMETADATA");
    let (status, content) = get(&router, &member).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("Metadata-Version: 2.1"));
    assert!(content.contains(&format!("href=\"{}\"", listing_url.replace('&', "&amp;"))));
}

#[rstest]
#[tokio::test]
async fn test_ui_archive_tree_links_nested_archives_and_blocks_binary_preview(
    ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = ui_router;
    let mut inner = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut inner));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("pkg/mod.py", options).unwrap();
        zip.write_all(b"x = 1\n").unwrap();
        zip.finish().unwrap();
    }
    let mut wheel = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut wheel));
        let options = zip::write::SimpleFileOptions::default();
        let dist_info = "veloxdemo-1.0.0.dist-info";
        let entries = vec![
            ("veloxdemo/__init__.py".to_owned(), Vec::new()),
            ("veloxdemo/data.bin".to_owned(), vec![0xff, 0xfe]),
            ("vendor/inner.zip".to_owned(), inner),
            (
                format!("{dist_info}/METADATA"),
                b"Metadata-Version: 2.1\nName: veloxdemo\nVersion: 1.0.0\n".to_vec(),
            ),
            (
                format!("{dist_info}/WHEEL"),
                b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n".to_vec(),
            ),
        ];
        for (path, bytes) in &entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let record_path = format!("{dist_info}/RECORD");
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(wheel_record(&entries, &record_path).as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    upload_file(&router, "veloxdemo-1.0.0-py3-none-any.whl", &wheel).await;
    let (_, detail) = get(&router, "/hosted/simple/veloxdemo/").await;
    let sha = detail
        .split("files/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();

    let file = "veloxdemo-1.0.0-py3-none-any.whl";
    let listing_url = format!("/browse?index=hosted&project=veloxdemo&sha256={sha}&file={file}");
    let (status, listing) = get(&router, &listing_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("class=\"browse-table\""));
    assert!(listing.contains("vendor"));
    assert!(listing.contains("inner.zip"));
    assert!(listing.contains("container=vendor%2Finner.zip"));
    assert!(listing.contains("data.bin"));
    assert!(!listing.contains("member=veloxdemo%2Fdata.bin"));

    let binary_url = format!("{listing_url}&member=veloxdemo%2Fdata.bin");
    let (status, binary) = get(&router, &binary_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(binary.contains("archive member"));
    assert!(binary.contains("cannot be previewed inline"));

    let nested_url = format!("{listing_url}&container=vendor%2Finner.zip");
    let (status, nested) = get(&router, &nested_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(nested.contains("pkg"));
    assert!(nested.contains("mod.py"));

    let member_url = format!("{nested_url}&member=pkg%2Fmod.py");
    let (status, content) = get(&router, &member_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("x = 1"));
}

#[rstest]
#[tokio::test]
async fn test_ui_private_archive_listing_rejects_anonymous_reads(
    private_pypi_ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = private_pypi_ui_router;
    let sha = upload_private_fixture(&router).await;
    let file = "veloxdemo-1.0.0-py3-none-any.whl";
    let listing_url = format!("/browse?index=vault&project=veloxdemo&sha256={sha}&file={file}");

    let (status, listing) = get(&router, &listing_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("read access denied"), "{listing}");
    assert!(!listing.contains("dist-info/METADATA"), "{listing}");

    let member = format!("{listing_url}&member=veloxdemo-1.0.0.dist-info%2FMETADATA");
    let (status, content) = get(&router, &member).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("read access denied"), "{content}");
    assert!(!content.contains("Metadata-Version"), "{content}");
}

#[rstest]
#[tokio::test]
async fn test_ui_private_archive_listing_serves_an_authorized_reader(
    private_pypi_ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = private_pypi_ui_router;
    let sha = upload_private_fixture(&router).await;
    let file = "veloxdemo-1.0.0-py3-none-any.whl";
    let listing_url = format!("/browse?index=vault&project=veloxdemo&sha256={sha}&file={file}");

    let (status, listing) = get_authorized(&router, &listing_url, &reader_authorization()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("dist-info/METADATA"), "{listing}");

    let member = format!("{listing_url}&member=veloxdemo-1.0.0.dist-info%2FMETADATA");
    let (status, content) = get_authorized(&router, &member, &reader_authorization()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("Metadata-Version"), "{content}");
}

#[rstest]
#[case::project("", "veloxdemo-1.0.0-py3-none-any.whl")]
#[case::archive("&member=veloxdemo-1.0.0.dist-info%2FMETADATA", "Metadata-Version")]
#[tokio::test]
async fn test_ui_private_browse_rejects_a_reader_for_another_project(
    #[case] suffix: &str,
    #[case] private_content: &str,
    private_pypi_ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = private_pypi_ui_router;
    let sha = upload_private_fixture(&router).await;
    let uri = if suffix.is_empty() {
        "/browse?index=vault&project=veloxdemo".to_owned()
    } else {
        format!("/browse?index=vault&project=veloxdemo&sha256={sha}&file=veloxdemo-1.0.0-py3-none-any.whl{suffix}")
    };
    let authorization = other_reader_authorization();

    let (render_status, rendered) = get_authorized(&router, &uri, &authorization).await;
    let (data_status, data) = get_authorized(&router, &format!("/+ui{uri}"), &authorization).await;

    assert_eq!(
        (
            render_status,
            rendered.contains("read access denied"),
            rendered.contains(private_content),
            data_status,
            data,
        ),
        (StatusCode::OK, true, false, StatusCode::FORBIDDEN, String::new())
    );
}

#[rstest]
#[tokio::test]
async fn test_ui_private_archive_listing_rejects_a_foreign_digest(
    private_pypi_ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = private_pypi_ui_router;
    upload_private_fixture(&router).await;
    let foreign = Digest::of(b"another project's wheel");
    let file = "veloxdemo-1.0.0-py3-none-any.whl";
    let listing_url = format!(
        "/browse?index=vault&project=veloxdemo&sha256={}&file={file}",
        foreign.as_str()
    );

    let (status, listing) = get_authorized(&router, &listing_url, &reader_authorization()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("is not a member of project"), "{listing}");
    assert!(!listing.contains("dist-info/METADATA"), "{listing}");

    let member = format!("{listing_url}&member=veloxdemo-1.0.0.dist-info%2FMETADATA");
    let (status, content) = get_authorized(&router, &member, &reader_authorization()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("is not a member of project"), "{content}");
}

fn wheel_record(entries: &[(String, Vec<u8>)], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes) in entries {
        let digest = URL_SAFE_NO_PAD.encode(Sha256::digest(bytes));
        writeln!(record, "{path},sha256={digest},{}", bytes.len()).unwrap();
    }
    writeln!(record, "{record_path},,").unwrap();
    record
}

#[tokio::test]
async fn test_ui_stats_drills_from_index_to_files() {
    let (_dir, state, router, authorization) = ui_router_admin_stateful().await;
    upload_fixture(&router).await;
    state.serving.metrics.flush().unwrap();
    let (status, body) = get_authorized(&router, "/stats?index=root%2Fpypi", &authorization).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("veloxdemo"));
    assert!(body.contains("uploads"));
    assert!(
        body.contains("/stats?index=root%2Fpypi&amp;resource=veloxdemo"),
        "{body}"
    );

    let (status, top) = get_authorized(&router, "/stats", &authorization).await;
    assert_eq!(status, StatusCode::OK);
    assert!(top.contains("/stats?index=root%2Fpypi"));

    let (status, files) = get_authorized(&router, "/stats?index=root%2Fpypi&resource=veloxdemo", &authorization).await;
    assert_eq!(status, StatusCode::OK);
    assert!(files.contains("rejected reads"), "{files}");
}

#[tokio::test]
async fn test_ui_stats_withholds_usage_from_anonymous() {
    let (_dir, router) = ui_router();
    upload_fixture(&router).await;
    let (status, body) = get(&router, "/stats?index=root%2Fpypi").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("veloxdemo"), "{body}");
}
