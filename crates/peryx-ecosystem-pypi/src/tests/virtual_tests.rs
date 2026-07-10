//! The virtual `PyPI` role: layer ordering and hosted-shadows-upstream resolution.
//!
//! A virtual index serves a project through one of two paths, and both must shadow the same way. The
//! streaming path tees a single cached layer through the transformer and hands it the hosted layers'
//! files up front. The buffered path resolves every layer and merges the results; it is taken when
//! streaming cannot apply, notably when a layer carries an active policy.

use std::sync::Arc;

use axum::http::StatusCode;
use peryx_driver::state::AppState;
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::http_tests::{fixture_wheel, get, upload_peryxpkg};

/// The digest the cached layer advertises for the contested filename. It differs from the uploaded
/// wheel's, so the served page names exactly one of the two files.
const UPSTREAM_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Serve an upstream page offering the same filename the hosted layer will hold.
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

/// A policy that is active but denies nothing this test serves. An active policy on a layer takes the
/// virtual index off the streaming path and onto the buffered merge.
fn active_policy() -> Policy {
    Policy::compile(&PolicyConfig {
        block_projects: vec!["some-other-project".to_owned()],
        ..PolicyConfig::default()
    })
}

/// A virtual index whose `layers` name the cached layer *before* the hosted one. Shadowing must not
/// depend on the operator ordering the list defensively.
fn cached_first_indexes(upstream: UpstreamClient, cached_policy: Policy) -> Vec<Index> {
    vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: cached_policy,
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
            policy: Policy::default(),
        },
        Index {
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: vec![0, 1],
                upload: Some(1),
            },
            policy: Policy::default(),
        },
    ]
}

/// Upload the wheel into the cached-first virtual index and read the project back, returning the
/// served page alongside the uploaded wheel's digest.
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
    let blobs = BlobStore::new(dir.path().join("blobs"));
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
