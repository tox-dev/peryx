use std::sync::Arc;

use axum::http::StatusCode;
use peryx_driver::state::AppState;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::http::{fixture_wheel, get, upload_peryxpkg};

const UPSTREAM_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn mount_peryxpkg(server: &MockServer) {
    let body = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"peryxpkg\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"peryxpkg-1.0-py3-none-any.whl\",\
         \"url\":\"https://upstream.invalid/peryxpkg-1.0-py3-none-any.whl\",\
         \"hashes\":{{\"sha256\":\"{UPSTREAM_DIGEST}\"}}}}]}}"
    );
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(server)
        .await;
}

fn active_policy() -> Policy {
    Policy::compile(
        &PolicyConfig {
            block_resources: vec!["some-other-project".to_owned()],
            ..PolicyConfig::default()
        },
        crate::normalize_name,
    )
}

fn cached_first_indexes(upstream: UpstreamClient, cached_policy: Policy) -> Vec<Index> {
    vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: cached_policy,
            acl: IndexAcl::default(),
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: crate::tests::writer_acl("s3cret".to_owned()),
        },
        Index {
            name: "root-pypi".to_owned(),
            route: "root/pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![0, 1],
                write_target: Some(1),
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
    ]
}

async fn serve_contested_project(state: &Arc<AppState>) -> (String, Digest) {
    let wheel = fixture_wheel();
    assert_eq!(upload_peryxpkg(state, "/root/pypi/", &wheel).await, StatusCode::OK);

    // Control: the cached layer really does serve the contested filename on its own route. Without
    // it a passing shadowing assertion below could just mean the upstream layer contributed nothing.
    let (cached_status, _, cached_detail) = get(state, "/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(cached_status, StatusCode::OK);
    assert!(
        cached_detail.contains(UPSTREAM_DIGEST),
        "the cached layer never served the upstream file, so the assertion below would be vacuous"
    );

    let (status, _, detail) = get(state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    (detail, Digest::of(&wheel))
}

fn assert_upload_shadows_upstream(detail: &str, hosted: &Digest) {
    assert!(
        detail.contains(hosted.as_str()),
        "expected the uploaded wheel to win, got: {detail}"
    );
    assert!(
        !detail.contains(UPSTREAM_DIGEST),
        "the upstream file shadowed the upload: {detail}"
    );
}

fn state_for(server: &MockServer, dir: &tempfile::TempDir, cached_policy: Policy) -> Arc<AppState> {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    super::wired(AppState::new(
        meta,
        blobs,
        60,
        cached_first_indexes(upstream, cached_policy),
    ))
}

#[tokio::test]
async fn test_streaming_virtual_shadows_upstream_when_cached_layer_is_listed_first() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_peryxpkg(&server).await;
    let state = state_for(&server, &dir, Policy::default());

    let (detail, hosted) = serve_contested_project(&state).await;

    assert_upload_shadows_upstream(&detail, &hosted);
}

#[tokio::test]
async fn test_buffered_virtual_shadows_upstream_when_cached_layer_is_listed_first() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_peryxpkg(&server).await;
    let state = state_for(&server, &dir, active_policy());

    let (detail, hosted) = serve_contested_project(&state).await;

    assert_upload_shadows_upstream(&detail, &hosted);
}

async fn mount_status(server: &MockServer, status: &str) {
    let body = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\",\"project-status\":\"{status}\",\
         \"project-status-reason\":\"upstream {status}\"}},\"name\":\"peryxpkg\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"peryxpkg-1.0-py3-none-any.whl\",\
         \"url\":\"https://upstream.invalid/peryxpkg-1.0-py3-none-any.whl\",\
         \"hashes\":{{\"sha256\":\"{UPSTREAM_DIGEST}\"}}}}]}}"
    );
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(server)
        .await;
}

fn two_cached_indexes(archived: UpstreamClient, quarantined: UpstreamClient, layers: Vec<usize>) -> Vec<Index> {
    let cached = |name: &str, client: UpstreamClient| Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Cached { client, offline: false },
        policy: active_policy(),
        acl: IndexAcl::default(),
    };
    vec![
        cached("archived", archived),
        cached("quarantined", quarantined),
        Index {
            name: "root-pypi".to_owned(),
            route: "root/pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers,
                write_target: None,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
    ]
}

async fn assert_quarantine_dominates(layers: Vec<usize>) {
    let dir = tempfile::tempdir().unwrap();
    let archived = MockServer::start().await;
    let quarantined = MockServer::start().await;
    mount_status(&archived, "archived").await;
    mount_status(&quarantined, "quarantined").await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let archived_client = UpstreamClient::new(&format!("{}/simple/", archived.uri())).unwrap();
    let quarantined_client = UpstreamClient::new(&format!("{}/simple/", quarantined.uri())).unwrap();
    let indexes = two_cached_indexes(archived_client, quarantined_client, layers);
    let state = super::wired(AppState::new(meta, blobs, 60, indexes));

    let (status, _, detail) = get(&state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(
        json["meta"]["project-status"], "quarantined",
        "a quarantined member must dominate the merged status: {detail}"
    );
    assert_eq!(
        json["files"].as_array().map(Vec::len),
        Some(0),
        "the quarantine must withhold every member's files: {detail}"
    );
}

#[tokio::test]
async fn test_buffered_virtual_quarantine_dominates_when_listed_after_a_benign_member() {
    assert_quarantine_dominates(vec![0, 1]).await;
}

#[tokio::test]
async fn test_buffered_virtual_quarantine_dominates_when_listed_before_a_benign_member() {
    assert_quarantine_dominates(vec![1, 0]).await;
}

#[tokio::test]
async fn test_buffered_virtual_caps_version_at_a_pre_pep700_layer() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    // A PEP 503-era upstream that advertises no version promises none of PEP 700's fields, so the
    // merged virtual page must fall back to the base version rather than inherit the default ceiling.
    let body = format!(
        "{{\"meta\":{{}},\"name\":\"peryxpkg\",\"files\":[{{\"filename\":\"peryxpkg-1.0-py3-none-any.whl\",\
         \"url\":\"https://upstream.invalid/peryxpkg-1.0-py3-none-any.whl\",\
         \"hashes\":{{\"sha256\":\"{UPSTREAM_DIGEST}\"}}}}]}}"
    );
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&server)
        .await;
    let state = state_for(&server, &dir, active_policy());

    let (status, _, detail) = get(&state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(json["meta"]["api-version"], crate::API_VERSION_BASE);
}
