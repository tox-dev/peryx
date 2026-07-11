//! Resolving a `/v2/` request to an index, and the proxy read-through and offline paths.

use super::support::*;

#[tokio::test]
async fn test_proxy_tag_is_cached_within_ttl_then_revalidated() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    // Exactly two fetches: the cold pull and the one after the TTL lapses. The in-window pull must be
    // served from cache, or wiremock's expect(2) fails on drop.
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

    // The tag now points somewhere else, so the HEAD's digest no longer matches and the manifest is
    // fetched: the shortcut must never pin a moved tag.
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
    // Exactly two upstream lists: the cold one and the one after the window lapses. The request in
    // between must be answered from the store, or wiremock's expect(2) fails on drop.
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

    // Stale but inside the 60s window plus the 300s bound: the last list still answers.
    now.store(1000 + 100, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("\"1\""));

    // Past the bound, the outage surfaces rather than a list of unbounded age.
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
    // Drop the cached manifest but leave its still-fresh tag record: the next pull must revalidate.
    crate::store::delete_manifest(&state.meta, &oci_digest(manifest)).unwrap();
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);
}
#[tokio::test]
async fn test_offline_proxy_serves_cached_tag() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, "http://127.0.0.1:1/", true);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    store::put_manifest(
        &state.meta,
        &digest,
        &Manifest {
            media_type: MANIFEST_TYPE.to_owned(),
            bytes: body.to_vec(),
        },
    )
    .unwrap();
    store::put_tag(&state.meta, "hub", "app", "stable", &digest).unwrap();
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
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Hosted {
            upload_token: None,
            volatile: false,
        },
        policy: peryx_policy::Policy::default(),
    };
    let store = oci_index(
        "store",
        "store",
        IndexKind::Hosted {
            upload_token: None,
            volatile: false,
        },
    );
    let (state, app) = app_with_indexes(&dir, vec![pypi, store]);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    store::put_manifest(
        &state.meta,
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
    let root = oci_index(
        "root",
        "",
        IndexKind::Hosted {
            upload_token: None,
            volatile: false,
        },
    );
    let (state, app) = app_with_indexes(&dir, vec![root]);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    store::put_manifest(
        &state.meta,
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
    // Three routes all prefix `a/b/c/app`; ordered so the middle candidate replaces the first (a
    // longer match) and the last does not (a shorter one), exercising both tie-break outcomes.
    let hosted = |name: &str, route: &str| {
        oci_index(
            name,
            route,
            IndexKind::Hosted {
                upload_token: None,
                volatile: false,
            },
        )
    };
    let (state, app) = app_with_indexes(
        &dir,
        vec![hosted("a", "a"), hosted("abc", "a/b/c"), hosted("ab", "a/b")],
    );
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    store::put_manifest(
        &state.meta,
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
    let stored = state.blobs.write(blob).unwrap();
    let path = state.blobs.path_for(&stored);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let digest = format!("sha256:{}", stored.as_str());
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    // Restore permissions so the temp dir can be cleaned up.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_proxy_blob_head_answers_a_range_it_has_not_cached() {
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

    // A cached blob answers this with 206. Whether the store happens to hold the layer must not change
    // what a client checking a range is told.
    let (status, headers, _) = send_with(&app, Method::HEAD, &uri, &[("range", "bytes=0-3")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 0-3/{}", blob.len()));

    let (status, _, _) = send_with(&app, Method::HEAD, &uri, &[("range", "bytes=99-100")]).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
}
#[tokio::test]
async fn test_proxy_blob_head_uses_an_upstream_head_not_a_download() {
    let server = MockServer::start().await;
    let blob = b"a-real-layer";
    let digest = oci_digest(blob);
    // Only HEAD is mocked; a GET (a full download) would 404 and fail the pull, so reaching 200 proves
    // peryx asked upstream with a HEAD.
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
    // The blob was not downloaded into the store.
    assert!(!state.blobs.exists(&crate::store::blob_digest(&digest).unwrap()));
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
    // No HEAD mock for this digest: upstream 404 -> blob unknown.
    let absent = oci_digest(b"absent");
    let (status, _, _) = send(&app, Method::HEAD, &format!("/v2/hub/library/nginx/blobs/{absent}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A non-absent upstream failure is a gateway error.
    let (status, _, _) = send(&app, Method::HEAD, &format!("/v2/hub/library/nginx/blobs/{present}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
