use super::support::*;
use crate::tests::search_total;
use peryx_identity::IndexAcl;

#[tokio::test]
async fn test_upstream_tag_removal_refreshes_search() {
    let dir = tempfile::tempdir().unwrap();
    let (_server, app) = proxy_after_upstream_tag_removal(&dir).await;

    assert_eq!(search_total(&app, "nginx").await, 0);
}

#[tokio::test]
async fn test_upstream_tag_removal_disables_stale_serve() {
    let dir = tempfile::tempdir().unwrap();
    let (server, app) = proxy_after_upstream_tag_removal(&dir).await;
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;

    assert_eq!(
        send(&app, Method::GET, "/v2/hub/library/nginx/manifests/latest")
            .await
            .0,
        StatusCode::BAD_GATEWAY
    );
}

async fn proxy_after_upstream_tag_removal(dir: &tempfile::TempDir) -> (MockServer, axum::Router) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .expect(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = crate::tests::proxy_with_clock(
        dir,
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = "/v2/hub/library/nginx/manifests/latest";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);
    assert_eq!(search_total(&app, "nginx").await, 1);

    server.reset().await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    now.store(1000 + 61, Ordering::Relaxed);
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::NOT_FOUND);
    (server, app)
}

#[tokio::test]
async fn test_proxy_tag_is_cached_within_ttl_then_revalidated() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    // The mock count distinguishes a cache hit from TTL revalidation.
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .expect(2)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = crate::tests::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = "/v2/hub/library/nginx/manifests/latest";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);
    now.store(1000 + 61, Ordering::Relaxed);
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);
}
#[tokio::test]
async fn test_moved_tag_is_refetched_after_the_window() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let first = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let second = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","x":1}"#;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(first.to_vec(), MANIFEST_TYPE))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = crate::tests::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = "/v2/hub/library/nginx/manifests/latest";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    // Revalidation must not pin a moved tag.
    server.reset().await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).insert_header("docker-content-digest", oci_digest(second).as_str()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(second.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    now.store(1000 + 61, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, second.to_vec());
}
#[tokio::test]
async fn test_proxy_tag_list_is_cached_within_the_window_then_revalidated() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    // The mock count distinguishes a cached list from TTL revalidation.
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/tags/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(br#"{"name":"library/nginx","tags":["1"]}"#.to_vec(), "application/json"),
        )
        .expect(2)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = crate::tests::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = "/v2/hub/library/nginx/tags/list";
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("\"1\""));

    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);
    now.store(1000 + 61, Ordering::Relaxed);
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);
}
#[tokio::test]
async fn test_proxy_tag_list_survives_an_outage_within_the_stale_bound() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/tags/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(br#"{"name":"library/nginx","tags":["1"]}"#.to_vec(), "application/json"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = crate::tests::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = "/v2/hub/library/nginx/tags/list";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/tags/list"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    now.store(1000 + 100, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("\"1\""));

    now.store(1000 + 400, Ordering::Relaxed);
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_proxy_tag_revalidates_when_the_cached_manifest_is_gone() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .expect(2)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let uri = "/v2/hub/library/nginx/manifests/latest";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);
    assert_eq!(
        state
            .serving
            .meta
            .remove_driver_values_if("oci\0m\0", 1, |_| Ok(true))
            .unwrap()
            .len(),
        1,
    );
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);
}
#[tokio::test]
async fn test_offline_proxy_serves_cached_tag() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, "http://127.0.0.1:1/", true);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    store::record_manifest(
        &state.serving.meta,
        "hub",
        "library/nginx",
        &digest,
        &Manifest {
            media_type: MANIFEST_TYPE.to_owned(),
            bytes: body.to_vec(),
        },
    )
    .unwrap();
    store::put_tag(&state.serving.meta, "hub", "app", "stable", &digest).unwrap();
    let (status, headers, got) = send(&app, Method::GET, "/v2/hub/app/manifests/stable").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], digest);
    assert_eq!(got, &body[..]);
}
#[tokio::test]
async fn test_offline_proxy_unknown_tag_is_manifest_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", true);
    let (status, _, body) = send(&app, Method::GET, "/v2/hub/app/manifests/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}
#[tokio::test]
async fn test_non_sha256_blob_digest_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, _, body) = send(&app, Method::GET, "/v2/hub/app/blobs/sha512:abcdef").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
}
#[tokio::test]
async fn test_unresolvable_name_is_name_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    for path in [
        "/v2/other/app/manifests/latest",
        "/v2/other/app/blobs/sha256:abc",
        "/v2/other/app/tags/list",
    ] {
        let (status, _, body) = send(&app, Method::GET, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(body_has_code(&body, "NAME_UNKNOWN"), "{path}: {body:?}");
    }
}
#[tokio::test]
async fn test_resolution_skips_a_non_oci_index() {
    use peryx_index::{Index, IndexKind};
    let dir = tempfile::tempdir().unwrap();
    let pypi = Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: peryx_core::Ecosystem::new("other"),
        kind: IndexKind::Hosted { volatile: false },
        policy: peryx_policy::Policy::default(),
        acl: IndexAcl::default(),
    };
    let store = oci_index("store", "store", IndexKind::Hosted { volatile: false });
    let (state, app) = app_with_indexes(&dir, vec![pypi, store]);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    store::record_manifest(
        &state.serving.meta,
        "store",
        "app",
        &digest,
        &Manifest {
            media_type: MANIFEST_TYPE.to_owned(),
            bytes: body.to_vec(),
        },
    )
    .unwrap();
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);
}
#[tokio::test]
async fn test_root_route_resolves_the_whole_name_as_the_repository() {
    use peryx_index::IndexKind;
    let dir = tempfile::tempdir().unwrap();
    let root = oci_index("root", "", IndexKind::Hosted { volatile: false });
    let (state, app) = app_with_indexes(&dir, vec![root]);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    store::record_manifest(
        &state.serving.meta,
        "root",
        "library/nginx",
        &digest,
        &Manifest {
            media_type: MANIFEST_TYPE.to_owned(),
            bytes: body.to_vec(),
        },
    )
    .unwrap();
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/library/nginx/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);
}
#[tokio::test]
async fn test_longest_route_wins_among_overlapping_oci_indexes() {
    use peryx_index::IndexKind;
    let dir = tempfile::tempdir().unwrap();
    let hosted = |name: &str, route: &str| oci_index(name, route, IndexKind::Hosted { volatile: false });
    let (state, app) = app_with_indexes(
        &dir,
        vec![hosted("a", "a"), hosted("abc", "a/b/c"), hosted("ab", "a/b")],
    );
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    store::record_manifest(
        &state.serving.meta,
        "abc",
        "app",
        &digest,
        &Manifest {
            media_type: MANIFEST_TYPE.to_owned(),
            bytes: body.to_vec(),
        },
    )
    .unwrap();
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/a/b/c/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);
}
#[cfg(unix)]
#[tokio::test]
async fn test_unreadable_blob_is_a_gateway_error() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let blob = b"unreadable";
    let stored = state.serving.blobs.put_bytes(blob).await.unwrap();
    let lease = state.serving.blobs.materialize(&stored).await.unwrap();
    let path = lease.path();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let digest = format!("sha256:{}", stored.as_str());
    store::record_blob_membership(&state.serving.meta, "store", "app", &digest).unwrap();
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    // Restore permissions so the temporary directory can be removed.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
// RFC 9110 section 14.2 excludes range processing from `HEAD`.
#[rstest]
#[case::valid(&[("range", "bytes=0-3")])]
#[case::unsatisfiable(&[("range", "bytes=99-100")])]
#[case::malformed(&[("range", "chunks=1-2")])]
#[case::if_range(&[("if-range", "\"sha256:0000\""), ("range", "bytes=0-3")])]
#[tokio::test]
async fn test_proxy_blob_head_ignores_a_range_it_has_not_cached(#[case] extra: &[(&str, &str)]) {
    let server = MockServer::start().await;
    let blob = b"a-real-layer";
    let digest = oci_digest(blob);
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/library/nginx/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", blob.len().to_string().as_str()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let uri = format!("/v2/hub/library/nginx/blobs/{digest}");

    let (status, headers, _) = send_with(&app, Method::HEAD, &uri, extra).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], blob.len().to_string());
    assert!(!headers.contains_key(header::CONTENT_RANGE));
}

#[tokio::test]
async fn test_proxy_blob_head_without_upstream_length_omits_length_and_ignores_range() {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let upstream = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let _ = socket.read(&mut request).unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n")
            .unwrap();
    });
    let digest = oci_digest(b"unknown-length");
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &base, false);
    let uri = format!("/v2/hub/library/nginx/blobs/{digest}");

    let (status, headers, body) = send_with(&app, Method::HEAD, &uri, &[("range", "bytes=0-3")]).await;
    upstream.join().unwrap();
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::CONTENT_LENGTH));
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert!(body.is_empty());
}

#[tokio::test]
async fn test_proxy_blob_head_uses_an_upstream_head_not_a_download() {
    let server = MockServer::start().await;
    let blob = b"a-real-layer";
    let digest = oci_digest(blob);
    // A successful response proves the proxy used `HEAD` without downloading the blob.
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/library/nginx/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", blob.len().to_string().as_str()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, body) = send(&app, Method::HEAD, &format!("/v2/hub/library/nginx/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], blob.len().to_string());
    assert!(body.is_empty());
    assert!(
        state
            .serving
            .blobs
            .head(&crate::store::blob_digest(&digest).unwrap())
            .await
            .unwrap()
            .is_none()
    );
}
#[tokio::test]
async fn test_proxy_blob_head_absent_and_upstream_error() {
    let server = MockServer::start().await;
    let present = oci_digest(b"nope");
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/library/nginx/blobs/{present}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let absent = oci_digest(b"absent");
    let (status, _, _) = send(&app, Method::HEAD, &format!("/v2/hub/library/nginx/blobs/{absent}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send(&app, Method::HEAD, &format!("/v2/hub/library/nginx/blobs/{present}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
