use super::attestations::{attestations_field, upload_with_attestations};
use super::support::*;
use crate::policy::RemoteMetadataMode;
use crate::store::UpstreamAttestation;
use peryx_driver::serving::{BrowseDriver as _, BrowseRequest};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;

pub(super) const FILENAME: &str = "peryxpkg-1.0-py3-none-any.whl";
pub(super) const PYPI_PROVENANCE: &str = concat!(
    r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"GitHub","repository":"sigstore/sigstore-python","workflow":"release.yml","environment":null},"attestations":[{"version":1,"#,
    r#""verification_material":{"certificate":"Zm9v","transparency_entries":[]},"#,
    r#""envelope":{"statement":"e30=","signature":"YmFy"}}]}]}"#,
);
pub(super) const REVISED_PYPI_PROVENANCE: &str = concat!(
    r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"GitHub","repository":"sigstore/sigstore-python","workflow":"release.yml","environment":null},"attestations":[{"version":1,"#,
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
        .serving
        .meta
        .put_cached_page(crate::store::CachedPageWrite {
            key: "pypi/peryxpkg",
            record: &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 1_000,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: Some(60),
                body: body.into_bytes(),
            },
            index: "pypi",
            normalized: "peryxpkg",
            display: "peryxpkg",
            source: "pypi",
            upstream: None,
            project_status: None,
            project_status_reason: None,
            files: &[(digest.to_owned(), format!("https://files.example/{FILENAME}"), None)],
            metadata: &[],
            attestations: &[],
        })
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

pub(super) struct ChunkedProvenanceServer {
    pub(super) url: String,
    shutdown: Option<oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    dropped: Option<oneshot::Receiver<()>>,
}

impl ChunkedProvenanceServer {
    pub(super) async fn shutdown(mut self) {
        let _ = self.shutdown.take().unwrap().send(());
        self.handle.take().unwrap().await.unwrap().unwrap();
    }
}

impl Drop for ChunkedProvenanceServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

pub(super) async fn chunked_provenance_server(body: Vec<u8>) -> ChunkedProvenanceServer {
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
    let (shutdown, stop) = oneshot::channel();
    let (task_lifetime, dropped) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let _task_lifetime = task_lifetime;
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stop.await;
            })
            .await
    });
    ChunkedProvenanceServer {
        url: format!("http://{address}/integrity/{FILENAME}.provenance"),
        shutdown: Some(shutdown),
        handle: Some(handle),
        dropped: Some(dropped),
    }
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

struct BlockedProvenanceServer {
    url: String,
    entered: Option<oneshot::Receiver<()>>,
    release: Option<oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
    dropped: Option<oneshot::Receiver<()>>,
}

impl BlockedProvenanceServer {
    async fn wait_until_entered(&mut self) {
        self.entered.take().unwrap().await.unwrap();
    }

    fn release(&mut self) {
        let _ = self.release.take().unwrap().send(());
    }

    async fn join(mut self) {
        self.handle.take().unwrap().await.unwrap();
    }
}

impl Drop for BlockedProvenanceServer {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

async fn blocked_provenance_server() -> BlockedProvenanceServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/integrity/{FILENAME}.provenance",
        listener.local_addr().unwrap()
    );
    let (entered, request_entered) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let (task_lifetime, dropped) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let _task_lifetime = task_lifetime;
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        assert_ne!(socket.read(&mut request).await.unwrap(), 0);
        let _ = entered.send(());
        let _ = released.await;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{PYPI_PROVENANCE}",
            PYPI_PROVENANCE.len(),
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    BlockedProvenanceServer {
        url,
        entered: Some(request_entered),
        release: Some(release),
        handle: Some(handle),
        dropped: Some(dropped),
    }
}

#[tokio::test]
async fn test_chunked_provenance_server_aborts_on_drop() {
    let mut server = chunked_provenance_server(Vec::new()).await;
    let dropped = server.dropped.take().unwrap();

    drop(server);

    assert!(dropped.await.is_err());
}

#[tokio::test]
async fn test_blocked_provenance_server_aborts_on_drop() {
    let mut server = blocked_provenance_server().await;
    let dropped = server.dropped.take().unwrap();

    drop(server);

    assert!(dropped.await.is_err());
}

#[tokio::test]
async fn test_project_page_flags_a_mirrored_provenance_claim() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "2".repeat(64);
    mount_upstream_attestation_page(&harness, &digest).await;
    get(&harness.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;

    let access = peryx_driver::access::ReadAccess::from_headers(&harness.state.serving, &axum::http::HeaderMap::new());
    let page = crate::serving::PypiServing
        .browse(BrowseRequest {
            state: harness.state.serving.clone(),
            position: 0,
            raw_query: format!("index=pypi&project=peryxpkg&filename={FILENAME}"),
            access: &access,
            base: None,
        })
        .await
        .unwrap()
        .unwrap();
    let provenance = page
        .sections
        .into_iter()
        .find_map(|section| match section {
            peryx_core::BrowseSection::Table { heading, rows, .. } if heading == "Files" => rows
                .into_iter()
                .find(|row| row.cells.first().is_some_and(|cell| cell.text == FILENAME))
                .and_then(|row| {
                    row.badges
                        .into_iter()
                        .find(|badge| badge.label.ends_with(" provenance"))
                }),
            _ => None,
        })
        .expect("the mirrored file carries a provenance badge");

    assert_eq!(
        provenance,
        peryx_core::BrowseBadge {
            label: "upstream provenance".to_owned(),
            class: "provenance-valid".to_owned(),
            hint: None,
        }
    );
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
    wait_for_project_revalidation(&harness).await;
    assert!(
        harness
            .state
            .serving
            .meta
            .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
            .unwrap()
            .is_none()
    );
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
    wait_for_project_revalidation(&harness).await;
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
            restarted.serving.meta.get_index("pypi/peryxpkg").unwrap(),
            restarted
                .serving
                .meta
                .get_upstream_attestation("pypi", "peryxpkg", &digest, FILENAME)
                .unwrap(),
        ),
        (StatusCode::NOT_FOUND, None, None)
    );
}

async fn wait_for_project_revalidation(harness: &Harness) {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        get(&harness.state, "/pypi/peryxpkg/json", None),
    )
    .await
    .expect("background revalidation did not finish");
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
        .serving
        .meta
        .put_cached_page(crate::store::CachedPageWrite {
            key: "pypi/peryxpkg",
            record: &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 1_000,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: Some(60),
                body: body.into_bytes(),
            },
            index: "pypi",
            normalized: "peryxpkg",
            display: "peryxpkg",
            source: "pypi",
            upstream: None,
            project_status: None,
            project_status_reason: None,
            files: &[(digest.clone(), format!("https://files.example/{FILENAME}"), None)],
            metadata: &[],
            attestations: &[],
        })
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

#[rstest]
#[case::loopback("http://127.0.0.2/provenance")]
#[case::link_local("https://169.254.169.254/latest/meta-data/")]
#[tokio::test]
async fn test_upstream_attestation_blocks_private_destinations(#[case] destination: &str) {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "18".repeat(32);
    mount_upstream_attestation_page_at(&harness, &digest, destination).await;
    let (page_status, ..) = get(&harness.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;

    let (status, _, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(page_status, StatusCode::OK);
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.ends_with(": upstream destination is not permitted"), "{body}");
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
        .serving
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
    let mut provenance = blocked_provenance_server().await;
    mount_upstream_attestation_page_at(&harness, &digest, &provenance.url).await;
    let (page_status, ..) = get(&harness.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page_status, StatusCode::OK);
    let state = harness.state.clone();
    let uri = upstream_provenance_uri(&digest);
    let fetch = tokio::spawn(async move { get(&state, &uri, None).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), provenance.wait_until_entered())
        .await
        .expect("attestation request did not start");
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

    provenance.release();
    let fetched = tokio::time::timeout(std::time::Duration::from_secs(1), fetch)
        .await
        .expect("attestation fetch did not finish")
        .expect("attestation task failed");
    provenance.join().await;
    assert_eq!(fetched.0, StatusCode::OK);
    assert_eq!(page.0, StatusCode::OK);
}

#[tokio::test]
async fn test_attestation_concurrency_limit_returns_too_many_requests() {
    let harness =
        harness_with_upstream_concurrency(upstream_policy(RemoteMetadataMode::Proxy), Policy::default(), 1).await;
    let digest = "16".repeat(32);
    upstream_page(&harness, &digest, "application/json").await;
    tokio::time::pause();
    let _permit = harness
        .state
        .serving
        .metadata_upstream_limits
        .acquire("pypi")
        .await
        .unwrap();

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
