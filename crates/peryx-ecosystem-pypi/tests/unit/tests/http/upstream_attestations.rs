//! Upstream PEP 740 discovery, routing, proxying, and lifecycle tests.

use super::attestations::{attestations_field, upload_with_attestations};
use super::support::*;
use crate::store::UpstreamAttestation;
use peryx_driver::serving::BrowseDriver as _;
use peryx_policy::RemoteMetadataMode;

pub(super) const FILENAME: &str = "peryxpkg-1.0-py3-none-any.whl";
pub(super) const PYPI_PROVENANCE: &str = concat!(
    r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"PyPI","claims":{}},"attestations":[{"version":1,"#,
    r#""verification_material":{"certificate":"Zm9v","transparency_entries":[]},"#,
    r#""envelope":{"statement":"e30=","signature":"YmFy"}}]}]}"#,
);
pub(super) const REVISED_PYPI_PROVENANCE: &str = concat!(
    r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"PyPI","claims":{}},"attestations":[{"version":1,"#,
    r#""verification_material":{"certificate":"Zm9v","transparency_entries":[]},"#,
    r#""envelope":{"statement":"e30=","signature":"YmFy"}}]}],"revision":2}"#,
);

pub(super) fn upstream_policy(mode: RemoteMetadataMode) -> Policy {
    policy(move |_neutral, pypi| pypi.upstream_attestations = mode)
}

pub(super) async fn upstream_harness(mode: RemoteMetadataMode) -> Harness {
    harness_with_policies(true, true, upstream_policy(mode), Policy::default(), Policy::default()).await
}

async fn virtual_upstream_harness(mode: RemoteMetadataMode) -> Harness {
    harness_with_policies(true, true, Policy::default(), Policy::default(), upstream_policy(mode)).await
}

async fn mount_upstream_attestation_page(harness: &Harness, digest: &str) {
    mount_upstream_attestation_page_at(
        harness,
        digest,
        &format!("{}/integrity/{FILENAME}.provenance", harness.server.uri()),
    )
    .await;
}

pub(super) async fn mount_upstream_attestation_page_at(harness: &Harness, digest: &str, provenance: &str) {
    let body = format!(
        r#"{{"meta":{{"api-version":"1.4"}},"name":"peryxpkg","versions":["1.0"],"files":[{{"filename":"{FILENAME}","url":"{server}/files/{FILENAME}","hashes":{{"sha256":"{digest}"}},"provenance":"{provenance}"}}]}}"#,
        server = harness.server.uri(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "max-age=60")
                .set_body_raw(body, "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&harness.server)
        .await;
}

pub(super) fn put_cached_attestation_page(harness: &Harness, digest: &str) {
    let body = format!(
        r#"{{"meta":{{"api-version":"1.4"}},"name":"peryxpkg","versions":["1.0"],"files":[{{"filename":"{FILENAME}","url":"https://files.example/{FILENAME}","hashes":{{"sha256":"{digest}"}},"provenance":"https://files.example/{FILENAME}.provenance"}}]}}"#,
    );
    harness
        .state
        .meta
        .put_cached_page(
            "pypi/peryxpkg",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 1_000,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: Some(60),
                body: body.into_bytes(),
            },
            "pypi",
            "peryxpkg",
            "peryxpkg",
            "pypi",
            None,
            None,
            None,
            &[(digest.to_owned(), format!("https://files.example/{FILENAME}"), None)],
            &[],
            &[],
        )
        .unwrap();
}

async fn mount_upstream_page_without_attestation(harness: &Harness, digest: &str) {
    let body = format!(
        r#"{{"meta":{{"api-version":"1.4"}},"name":"peryxpkg","versions":["1.0"],"files":[{{"filename":"{FILENAME}","url":"{server}/files/{FILENAME}","hashes":{{"sha256":"{digest}"}}}}]}}"#,
        server = harness.server.uri(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "max-age=60")
                .set_body_raw(body, "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&harness.server)
        .await;
}

pub(super) async fn chunked_provenance_server(body: Vec<u8>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let chunks: Vec<_> = body.chunks(64 * 1024).map(bytes::Bytes::copy_from_slice).collect();
    let app = axum::Router::new().fallback(move || {
        let chunks = chunks.clone();
        async move {
            (
                [(header::CONTENT_TYPE, "application/json")],
                axum::body::Body::from_stream(futures_util::stream::iter(
                    chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
                )),
            )
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}/integrity/{FILENAME}.provenance")
}

pub(super) async fn upstream_page(harness: &Harness, digest: &str, accept: &str) -> String {
    mount_upstream_attestation_page(harness, digest).await;
    let (status, _, body) = get(&harness.state, "/pypi/simple/peryxpkg/", Some(accept)).await;
    assert_eq!(status, StatusCode::OK);
    body
}

pub(super) fn upstream_provenance_uri(digest: &str) -> String {
    format!("/pypi/files/{digest}/{FILENAME}.provenance")
}

async fn wait_for_attestation_retirement(harness: &Harness, digest: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while harness
            .state
            .meta
            .get_upstream_attestation("pypi", "peryxpkg", digest, FILENAME)
            .unwrap()
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background revalidation never retired the attestation locator");
}

pub(super) async fn mount_provenance(harness: &Harness, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(format!("/integrity/{FILENAME}.provenance")))
        .respond_with(response)
        .mount(&harness.server)
        .await;
}

pub(super) async fn provenance_requests(harness: &Harness) -> usize {
    harness
        .server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path().starts_with("/integrity/"))
        .count()
}

#[tokio::test]
async fn test_project_page_flags_a_mirrored_provenance_claim() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "2".repeat(64);
    mount_upstream_attestation_page(&harness, &digest).await;
    // Cache the upstream detail so the browse view resolves the claim from stored metadata.
    get(&harness.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;

    let view = crate::serving::PypiServing
        .browse_project(harness.state.serving.clone(), 0, "peryxpkg".to_owned())
        .await
        .unwrap()
        .unwrap();
    let peryx_core::UiProjectView::Files { project, .. } = view else {
        panic!("expected a file listing view");
    };
    let detail = project
        .files
        .into_iter()
        .find(|file| file.filename == FILENAME)
        .and_then(|file| file.provenance_detail)
        .expect("the mirrored file carries a provenance panel");

    assert_eq!(detail.source, peryx_core::UiProvenanceSource::Mirrored);
    assert!(!detail.malformed);
    assert!(detail.attestations.is_empty());
}

#[tokio::test]
async fn test_upstream_attestation_direct_mode_preserves_the_source_url() {
    let harness = upstream_harness(RemoteMetadataMode::Direct).await;
    let digest = "1".repeat(64);

    let page = upstream_page(&harness, &digest, "application/json").await;

    assert!(page.contains(&format!("{}/integrity/{FILENAME}.provenance", harness.server.uri())));
    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(provenance_requests(&harness).await, 0);
}

#[tokio::test]
async fn test_removed_upstream_attestation_does_not_survive_a_new_project_generation() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "11".repeat(32);
    upstream_page(&harness, &digest, "application/json").await;
    harness.clock.store(1_061, Ordering::Relaxed);
    harness.server.reset().await;
    mount_upstream_page_without_attestation(&harness, &digest).await;

    let (page_status, ..) = get(&harness.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;
    let (provenance_status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(page_status, StatusCode::OK);
    assert_eq!(provenance_status, StatusCode::NOT_FOUND);
    wait_for_attestation_retirement(&harness, &digest).await;
}

#[tokio::test]
async fn test_upstream_project_404_retires_its_attestation_locators() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "12".repeat(32);
    upstream_page(&harness, &digest, "application/json").await;
    harness.clock.store(1_061, Ordering::Relaxed);
    harness.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&harness.server)
        .await;

    let (stale_status, ..) = get(&harness.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;
    wait_for_attestation_retirement(&harness, &digest).await;
    let (missing_status, ..) = get(&harness.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;
    let (provenance_status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(stale_status, StatusCode::OK);
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    assert_eq!(provenance_status, StatusCode::NOT_FOUND);
    let restarted = restarted_state(&harness);
    let (restart_status, ..) = get(&restarted, "/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(
        (
            restart_status,
            restarted.meta.get_index("pypi/peryxpkg").unwrap(),
            restarted
                .meta
                .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
                .unwrap(),
        ),
        (StatusCode::NOT_FOUND, None, None)
    );
}

#[tokio::test]
async fn test_virtual_provenance_lookup_miss_is_not_found() {
    let harness = virtual_upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "0".repeat(64);

    let (status, ..) = get(
        &harness.state,
        &format!("/root/pypi/files/{digest}/{FILENAME}.provenance"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_virtual_provenance_without_a_registered_layer_source_is_not_found() {
    let harness = virtual_upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "15".repeat(32);
    let body = format!(
        r#"{{"meta":{{"api-version":"1.4"}},"name":"peryxpkg","versions":["1.0"],"files":[{{"filename":"{FILENAME}","url":"https://files.example/{FILENAME}","hashes":{{"sha256":"{digest}"}},"provenance":"https://files.example/{FILENAME}.provenance"}}]}}"#,
    );
    harness
        .state
        .meta
        .put_cached_page(
            "pypi/peryxpkg",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 1_000,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: Some(60),
                body: body.into_bytes(),
            },
            "pypi",
            "peryxpkg",
            "peryxpkg",
            "pypi",
            None,
            None,
            None,
            &[(digest.clone(), format!("https://files.example/{FILENAME}"), None)],
            &[],
            &[],
        )
        .unwrap();

    let (status, ..) = get(
        &harness.state,
        &format!("/root/pypi/files/{digest}/{FILENAME}.provenance"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_upstream_attestation_proxy_mode_fetches_without_retaining() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "2".repeat(64);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200).set_body_raw(PYPI_PROVENANCE, "application/vnd.pypi.integrity.v1+json"),
    )
    .await;

    let page = upstream_page(&harness, &digest, "application/json").await;
    assert!(page.contains(&upstream_provenance_uri(&digest)));
    let html = upstream_page(&harness, &digest, "text/html").await;
    assert!(html.contains(&upstream_provenance_uri(&digest)));

    for expected_requests in 1..=2 {
        let (status, headers, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, PYPI_PROVENANCE);
        assert_eq!(headers[header::CACHE_CONTROL], "public, no-cache");
        assert_eq!(headers["x-peryx-provenance-source"], "pypi");
        assert_eq!(headers["x-peryx-provenance-availability"], "remote-only");
        assert_eq!(provenance_requests(&harness).await, expected_requests);
    }
}

#[tokio::test]
async fn test_authenticated_upstream_attestation_response_is_private() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "14".repeat(32);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200).set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;

    let (status, headers, _) = get_bytes_with_headers(
        &harness.state,
        &upstream_provenance_uri(&digest),
        &[(header::AUTHORIZATION.as_str(), "Basic dW51c2Vk")],
    )
    .await;

    assert_eq!(
        (status, headers[header::CACHE_CONTROL].to_str().unwrap()),
        (StatusCode::OK, "private, no-cache")
    );
}

#[tokio::test]
async fn test_virtual_attestation_policy_fetches_from_its_cached_member() {
    let harness = virtual_upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "7".repeat(64);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200).set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    mount_upstream_attestation_page(&harness, &digest).await;

    let (page_status, _, page) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page_status, StatusCode::OK);
    let uri = format!("/root/pypi/files/{digest}/{FILENAME}.provenance");
    assert!(page.contains(&uri));

    let (status, headers, body) = get(&harness.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, PYPI_PROVENANCE);
    assert_eq!(headers["x-peryx-provenance-source"], "pypi");
}

#[tokio::test]
async fn test_cached_and_virtual_routes_select_the_visible_attestation_source() {
    let harness = harness_with_policies(
        true,
        true,
        upstream_policy(RemoteMetadataMode::Proxy),
        Policy::default(),
        upstream_policy(RemoteMetadataMode::Proxy),
    )
    .await;
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel).as_str().to_owned();
    mount_provenance(
        &harness,
        ResponseTemplate::new(200).set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    assert_eq!(
        upload_with_attestations(&harness.state, &wheel, &attestations_field(FILENAME, &digest)).await,
        StatusCode::OK
    );

    let cached = get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    let virtual_uri = format!("/root/pypi/files/{digest}/{FILENAME}.provenance");
    let hosted = get(&harness.state, &virtual_uri, None).await;

    assert_eq!(
        (
            cached.0,
            cached
                .1
                .get("x-peryx-provenance-source")
                .and_then(|value| value.to_str().ok()),
            cached.2
        ),
        (StatusCode::OK, Some("pypi"), PYPI_PROVENANCE.to_owned())
    );
    assert_eq!(hosted.0, StatusCode::OK);
    assert_eq!(hosted.1["x-peryx-provenance-source"], "hosted");
    assert_ne!(hosted.2, PYPI_PROVENANCE);
}

#[tokio::test]
async fn test_direct_cached_route_does_not_serve_a_hosted_digest_collision() {
    let harness = upstream_harness(RemoteMetadataMode::Direct).await;
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel).as_str().to_owned();
    upstream_page(&harness, &digest, "application/json").await;
    assert_eq!(
        upload_with_attestations(&harness.state, &wheel, &attestations_field(FILENAME, &digest)).await,
        StatusCode::OK
    );

    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_cached_route_ignores_another_projects_attestation_registration() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "17".repeat(32);
    put_cached_attestation_page(&harness, &digest);
    harness
        .state
        .meta
        .put_upstream_attestation(
            "pypi",
            &digest,
            FILENAME,
            &UpstreamAttestation::remote(
                &format!("{}/integrity/{FILENAME}.provenance", harness.server.uri()),
                "pypi",
                "another-project",
                None,
            ),
        )
        .unwrap();
    mount_provenance(
        &harness,
        ResponseTemplate::new(200).set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;

    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(provenance_requests(&harness).await, 0);
}

#[tokio::test]
async fn test_slow_attestation_fetch_does_not_take_the_project_page_slot() {
    let harness =
        harness_with_upstream_concurrency(upstream_policy(RemoteMetadataMode::Proxy), Policy::default(), 1).await;
    let digest = "13".repeat(32);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200)
            .set_delay(std::time::Duration::from_secs(2))
            .set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    let state = harness.state.clone();
    let uri = upstream_provenance_uri(&digest);
    let fetch = tokio::spawn(async move { get(&state, &uri, None).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while provenance_requests(&harness).await == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/simple/other/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"meta":{"api-version":"1.4"},"name":"other","versions":[],"files":[]}"#,
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&harness.server)
        .await;

    let page = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        get(&harness.state, "/pypi/simple/other/", Some("application/json")),
    )
    .await
    .expect("project page waited for an attestation slot");

    fetch.abort();
    let _ = fetch.await;
    assert_eq!(page.0, StatusCode::OK);
}

#[tokio::test]
async fn test_attestation_concurrency_limit_returns_too_many_requests() {
    let harness =
        harness_with_upstream_concurrency(upstream_policy(RemoteMetadataMode::Proxy), Policy::default(), 1).await;
    let digest = "16".repeat(32);
    upstream_page(&harness, &digest, "application/json").await;
    tokio::time::pause();
    let _permit = harness.state.metadata_upstream_limits.acquire("pypi").await.unwrap();

    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_virtual_hosted_file_without_provenance_hides_cached_provenance() {
    let harness = virtual_upstream_harness(RemoteMetadataMode::Proxy).await;
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel).as_str().to_owned();
    mount_provenance(
        &harness,
        ResponseTemplate::new(200).set_body_raw(PYPI_PROVENANCE, "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;
    assert_eq!(
        upload_peryxpkg(&harness.state, "/root/pypi/", &wheel).await,
        StatusCode::OK
    );

    let (page_status, _, page) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    let provenance_uri = format!("/root/pypi/files/{digest}/{FILENAME}.provenance");
    let (provenance_status, ..) = get(&harness.state, &provenance_uri, None).await;

    assert_eq!(page_status, StatusCode::OK);
    assert!(page.contains(FILENAME));
    assert!(!page.contains("provenance"));
    assert_eq!(provenance_status, StatusCode::NOT_FOUND);
    assert_eq!(provenance_requests(&harness).await, 0);
}
