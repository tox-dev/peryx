//! Proxy-pull and cached-serve paths, driven through the router with a wiremock upstream.

use axum::http::{Method, StatusCode, header};
use rstest::rstest;
use wiremock::matchers::{header as match_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{app_with_indexes, body_has_code, hosted, oci_digest, oci_index, proxy, proxy_with_auth, send, send_with};
use crate::store::{self, Manifest};

const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

#[tokio::test]
async fn test_manifest_by_tag_pulls_through_with_the_token_flow() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    // The 401 challenge is mounted first and the authenticated 200 last, so an anonymous request draws
    // the challenge and the token-bearing retry wins the tie.
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                format!(
                    r#"Bearer realm="{}/token",service="reg",scope="repository:library/nginx:pull""#,
                    server.uri()
                )
                .as_str(),
            ),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"abc"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer abc"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, got) = send(&app, Method::GET, "/v2/hub/library/nginx/manifests/latest").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], MANIFEST_TYPE);
    assert_eq!(headers["docker-content-digest"], oci_digest(body));
    assert_eq!(got, &body[..]);
    // The tag mapping and manifest are cached under the canonical digest.
    assert_eq!(
        store::get_tag(&state.meta, "hub", "library/nginx", "latest").unwrap(),
        Some(oci_digest(body))
    );
}

#[tokio::test]
async fn test_token_flow_presents_configured_basic_credentials_to_the_realm() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"Bearer realm="{}/token",service="reg""#, server.uri()).as_str(),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // The token endpoint answers only when the request carries the configured Basic credentials, so a
    // pull that reaches the authenticated 200 proves peryx presented them (an anonymous token request
    // would miss this mock and fail the pull).
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(match_header("authorization", "Basic YWxpY2U6czNjcmV0"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"abc"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer abc"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let auth = peryx_upstream::Auth::Basic {
        username: "alice".to_owned(),
        password: "s3cret".to_owned(),
    };
    let (_state, app) = proxy_with_auth(&dir, &format!("{}/", server.uri()), auth);
    let (status, _, got) = send(&app, Method::GET, "/v2/hub/library/nginx/manifests/latest").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);
}

#[tokio::test]
async fn test_oversized_upstream_manifest_is_rejected_not_buffered() {
    let server = MockServer::start().await;
    // Over MAX_MANIFEST_BYTES (4 MiB): a hostile upstream must not drive peryx out of memory.
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![b'x'; 5 * 1024 * 1024], MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/library/nginx/manifests/latest").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

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
    let (_state, app) = super::proxy_with_clock(
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
async fn test_unchanged_tag_revalidates_without_refetching_the_manifest() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let digest = oci_digest(manifest);
    // Exactly one GET: the cold pull. The revalidation after the window must be answered by the HEAD,
    // or wiremock's expect(1) fails on drop.
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).insert_header("docker-content-digest", digest.as_str()))
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = super::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = "/v2/hub/library/nginx/manifests/latest";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    now.store(1000 + 61, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, manifest.to_vec());
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
    let (_state, app) = super::proxy_with_clock(
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
async fn test_a_tag_staler_than_the_bound_is_not_served_when_upstream_fails() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = super::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = "/v2/hub/library/nginx/manifests/latest";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    // Upstream is down and the cached tag is older than the 60s window plus the 300s stale bound, so
    // the outage surfaces instead of a manifest of unbounded age.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    now.store(1000 + 400, Ordering::Relaxed);
    let (status, _, _) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_unchanged_tag_refetches_when_the_cached_manifest_is_missing() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let digest = oci_digest(manifest);
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).insert_header("docker-content-digest", digest.as_str()))
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (state, app) = super::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = "/v2/hub/library/nginx/manifests/latest";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    // The tag has not moved, so the HEAD shortcut would serve from the store; the manifest it names is
    // gone, so it must fall through and fetch rather than answer with nothing.
    crate::store::delete_manifest(&state.meta, &digest).unwrap();
    now.store(1000 + 61, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, manifest.to_vec());
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
    let (_state, app) = super::proxy_with_clock(
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
    let (_state, app) = super::proxy_with_clock(
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
async fn test_a_zero_stale_bound_serves_a_tag_list_of_any_age() {
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
    let (_state, app) = super::proxy_with_stale(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
        0,
    );
    let uri = "/v2/hub/library/nginx/tags/list";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/tags/list"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    // An operator mirroring a knowingly unreliable upstream asked for exactly this: no bound at all.
    now.store(1_000_000, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("\"1\""));
}

#[tokio::test]
async fn test_expired_upstream_credentials_do_not_delete_a_cached_tag() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let uri = "/v2/hub/library/nginx/manifests/latest";
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = super::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    // The token expires, so the revalidation after the freshness window is answered 401. Docker Hub
    // says that about a repository it will not discuss, not about one that is gone: the cached image
    // must still pull rather than become `manifest unknown`.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    now.store(1000 + 61, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, manifest.to_vec());
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
async fn test_concurrent_by_digest_pulls_share_one_upstream_fetch() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let digest = oci_digest(manifest);
    // expect(1): if single-flight holds, the two concurrent pulls draw exactly one upstream fetch.
    // The gate guarantees the ordering (the follower cannot fetch until the leader releases, by which
    // point the manifest is stored), so the follower serves from the store rather than refetching.
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/nginx/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let uri = format!("/v2/hub/library/nginx/manifests/{digest}");
    let (first, second) = tokio::join!(send(&app, Method::GET, &uri), send(&app, Method::GET, &uri));

    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
}

#[tokio::test]
async fn test_upstream_rate_limit_becomes_429_with_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "17"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, body) = send(&app, Method::GET, "/v2/hub/library/nginx/manifests/latest").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers[header::RETRY_AFTER], "17");
    assert!(body_has_code(&body, "TOOMANYREQUESTS"), "{body:?}");
}

#[tokio::test]
async fn test_upstream_rate_limit_without_retry_after_is_still_429() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, _) = send(&app, Method::GET, "/v2/hub/library/nginx/manifests/latest").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(!headers.contains_key(header::RETRY_AFTER));
}

#[tokio::test]
async fn test_manifest_upstream_401_is_treated_as_absent() {
    let server = MockServer::start().await;
    // A registry answers 401 for a repository that does not exist or is not anonymously visible; the
    // proxy reports the manifest unknown rather than a gateway fault, so a client (or a virtual index)
    // treats it as absent.
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_token_endpoint_failure_is_a_gateway_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"Bearer realm="{}/token",service="reg""#, server.uri()).as_str(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_manifest_head_by_tag_returns_headers_only() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2}"#;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, got) = send(&app, Method::HEAD, "/v2/hub/app/manifests/v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(body));
    assert_eq!(headers[header::CONTENT_LENGTH], body.len().to_string());
    assert!(got.is_empty());
}

#[tokio::test]
async fn test_manifest_by_digest_served_from_cache_without_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
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
    let (status, headers, got) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], digest);
    assert_eq!(got, &body[..]);
}

#[tokio::test]
async fn test_manifest_by_digest_pulls_through_and_verifies() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"config":{}}"#;
    let digest = oci_digest(body);
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);
}

#[tokio::test]
async fn test_manifest_by_digest_mismatch_is_rejected() {
    let server = MockServer::start().await;
    let wrong = format!("sha256:{}", "b".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{wrong}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"different".to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{wrong}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "MANIFEST_INVALID"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_by_digest_upstream_error_is_a_gateway_error() {
    let server = MockServer::start().await;
    let digest = format!("sha256:{}", "3".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_manifest_missing_upstream_is_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/absent"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, "/v2/hub/app/manifests/absent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[rstest]
#[case::manifest("manifests/boom".to_owned())]
#[case::blob(format!("blobs/sha256:{}", "2".repeat(64)))]
#[case::tags("tags/list".to_owned())]
#[tokio::test]
async fn test_upstream_gateway_failure_is_a_gateway_error(#[case] suffix: String) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/{suffix}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/hub/app/{suffix}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
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
async fn test_hosted_tag_pointing_at_a_missing_manifest_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    store::put_tag(&state.meta, "store", "app", "v1", &format!("sha256:{}", "c".repeat(64))).unwrap();
    let (status, _, body) = send(&app, Method::GET, "/v2/store/app/manifests/v1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_blob_pulls_through_then_serves_a_range() {
    let server = MockServer::start().await;
    let blob = b"the-layer-bytes-0123456789";
    let digest = oci_digest(blob);
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/alpine/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(blob.to_vec(), "application/octet-stream"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let uri = format!("/v2/hub/library/alpine/blobs/{digest}");
    let (status, headers, got) = send(&app, Method::GET, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(headers["docker-content-digest"], digest);
    assert_eq!(got, &blob[..]);

    // A second request is a cache hit; a range yields 206 with the slice.
    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("range", "bytes=0-3")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 0-3/{}", blob.len()));
    assert_eq!(got, &blob[..4]);
}

#[tokio::test]
async fn test_concurrent_blob_misses_share_one_upstream_fetch() {
    let server = MockServer::start().await;
    let blob = b"a-layer-two-clients-race-for";
    let digest = oci_digest(blob);
    // A delayed response holds the first fetch open long enough for the second request to reach the
    // single-flight gate and park on it; `expect(1)` then proves both clients were served by one
    // upstream fetch.
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/alpine/blobs/{digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(blob.to_vec(), "application/octet-stream")
                .set_delay(std::time::Duration::from_millis(200)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let uri = format!("/v2/hub/library/alpine/blobs/{digest}");
    let (first, second) = tokio::join!(send(&app, Method::GET, &uri), send(&app, Method::GET, &uri));
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(first.2, &blob[..]);
    assert_eq!(second.2, &blob[..]);
}

#[tokio::test]
async fn test_blob_head_and_unsatisfiable_range() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let blob = b"0123456789";
    let stored = state.blobs.write(blob).unwrap();
    let digest = format!("sha256:{}", stored.as_str());
    let uri = format!("/v2/store/app/blobs/{digest}");

    let (status, headers, got) = send(&app, Method::HEAD, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], blob.len().to_string());
    assert!(got.is_empty());

    let (status, headers, _) = send_with(&app, Method::HEAD, &uri, &[("range", "bytes=5-6")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 5-6/{}", blob.len()));

    let (status, headers, _) = send_with(&app, Method::GET, &uri, &[("range", "bytes=50-60")]).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes */{}", blob.len()));
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");

    // A multi-range request we do not serve as multipart falls back to the whole blob (RFC 7233).
    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("range", "bytes=0-1,3-4")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], blob.len().to_string());
    assert_eq!(got, &blob[..]);

    // A HEAD for a blob a hosted index does not hold, with no proxy member to ask, is unknown.
    let absent = format!("/v2/store/app/blobs/{}", oci_digest(b"absent-blob"));
    let (status, _, _) = send(&app, Method::HEAD, &absent).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_blob_missing_on_hosted_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted(&dir);
    let digest = format!("sha256:{}", "d".repeat(64));
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "BLOB_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_blob_upstream_404_is_unknown() {
    let server = MockServer::start().await;
    let digest = format!("sha256:{}", "e".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "BLOB_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_blob_upstream_digest_mismatch_is_rejected() {
    let server = MockServer::start().await;
    let claimed = format!("sha256:{}", "f".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/blobs/{claimed}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"not-what-was-claimed".to_vec(), "application/octet-stream"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/blobs/{claimed}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
}

#[tokio::test]
async fn test_truncated_upstream_blob_is_a_gateway_error() {
    use std::io::{Read as _, Write as _};
    // A raw server that promises more bytes than it sends, then closes, so the upstream body stream
    // errors mid-transfer and the pull-through surfaces a 502 rather than a corrupt cached blob.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0; 1024];
        let _ = socket.read(&mut buffer);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 4096\r\nconnection: close\r\n\r\nshort")
            .unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &base, false);
    let digest = format!("sha256:{}", "9".repeat(64));
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/hub/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
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
async fn test_tags_list_passes_upstream_through() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/tags/list"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            br#"{"name":"library/nginx","tags":["1.25","latest"]}"#.to_vec(),
            "application/json",
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, "/v2/hub/library/nginx/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(std::str::from_utf8(&body).unwrap().contains("\"1.25\""));
}

#[tokio::test]
async fn test_tags_list_from_cache_when_hosted() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let digest = format!("sha256:{}", "a".repeat(64));
    store::put_tag(&state.meta, "store", "app", "latest", &digest).unwrap();
    store::put_tag(&state.meta, "store", "app", "v2", &digest).unwrap();
    let (status, _, body) = send(&app, Method::GET, "/v2/store/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("\"store/app\""), "{text}");
    assert!(text.contains("\"latest\"") && text.contains("\"v2\""), "{text}");
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
    use peryx_http::{Index, IndexKind};
    let dir = tempfile::tempdir().unwrap();
    let pypi = Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: peryx_format::Ecosystem::Pypi,
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
    use peryx_http::IndexKind;
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
    use peryx_http::IndexKind;
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

#[tokio::test]
async fn test_manifest_upstream_unreachable_is_a_gateway_error() {
    let dir = tempfile::tempdir().unwrap();
    // An online proxy whose upstream refuses every connection surfaces a transport fault as 502.
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_token_endpoint_without_a_token_is_a_gateway_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"Bearer realm="{}/token""#, server.uri()).as_str(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_token_endpoint_with_invalid_json_is_a_gateway_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"Bearer realm="{}/token""#, server.uri()).as_str(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_manifest_by_digest_missing_on_hosted_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted(&dir);
    let digest = format!("sha256:{}", "e".repeat(64));
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_by_digest_missing_upstream_is_unknown() {
    let server = MockServer::start().await;
    let digest = format!("sha256:{}", "1".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_blob_suffix_and_open_ended_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let blob = b"0123456789";
    let stored = state.blobs.write(blob).unwrap();
    let digest = format!("sha256:{}", stored.as_str());
    let uri = format!("/v2/store/app/blobs/{digest}");

    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("range", "bytes=-3")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 7-9/{}", blob.len()));
    assert_eq!(got, &blob[7..]);

    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("range", "bytes=8-")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 8-9/{}", blob.len()));
    assert_eq!(got, &blob[8..]);

    // A malformed range is ignored and the whole blob is served.
    let (status, _, got) = send_with(&app, Method::GET, &uri, &[("range", "chunks=1-2")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
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
async fn test_referrers_merge_upstream_and_filter_by_artifact_type() {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    let sig = format!("sha256:{}", "b".repeat(64));
    // Upstream lists the signature twice (dedup) and one descriptor with no digest (skipped).
    let index = format!(
        concat!(
            r#"{{"schemaVersion":2,"manifests":["#,
            r#"{{"digest":"{sig}","artifactType":"application/vnd.example.sig"}},"#,
            r#"{{"digest":"{sig}","artifactType":"application/vnd.example.sig"}},"#,
            r#"{{"artifactType":"application/vnd.example.sig"}}]}}"#,
        ),
        sig = sig,
    );
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/nginx/referrers/{subject}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(index.into_bytes(), "application/vnd.oci.image.index.v1+json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let base = format!("/v2/hub/library/nginx/referrers/{subject}");

    let (status, headers, body) = send(&app, Method::GET, &base).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("oci-filters-applied"));
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["manifests"].as_array().unwrap().len(), 1);
    assert_eq!(doc["manifests"][0]["digest"], sig);

    let (status, headers, body) = send(
        &app,
        Method::GET,
        &format!("{base}?artifactType=application/vnd.example.sig"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["oci-filters-applied"], "artifactType");
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["manifests"].as_array().unwrap().len(), 1);

    let (_, _, body) = send(&app, Method::GET, &format!("{base}?artifactType=application/vnd.other")).await;
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(doc["manifests"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_referrers_tolerate_an_upstream_without_the_api() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let subject = format!("sha256:{}", "c".repeat(64));
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/library/nginx/referrers/{subject}")).await;
    assert_eq!(status, StatusCode::OK);
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(doc["manifests"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_concurrent_tag_pulls_share_one_upstream_fetch() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    // expect(1): two concurrent pulls of the same uncached tag single-flight to one upstream fetch,
    // and the follower serves the manifest the leader just stamped.
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let uri = "/v2/hub/library/nginx/manifests/latest";
    let (first, second) = tokio::join!(send(&app, Method::GET, uri), send(&app, Method::GET, uri));
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
}

#[tokio::test]
async fn test_upstream_manifest_digest_header_is_verified() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    // A correct Docker-Content-Digest is accepted; a wrong one rejects the manifest as a gateway fault.
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/good"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", oci_digest(body).as_str())
                .set_body_raw(body.to_vec(), MANIFEST_TYPE),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/bad"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", format!("sha256:{}", "e".repeat(64)).as_str())
                .set_body_raw(body.to_vec(), MANIFEST_TYPE),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, _, _) = send(&app, Method::GET, "/v2/hub/library/nginx/manifests/good").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/library/nginx/manifests/bad").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_catalog_lists_oci_repositories_with_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let pypi = peryx_http::Index {
        name: "py".to_owned(),
        route: "py".to_owned(),
        ecosystem: peryx_format::Ecosystem::Pypi,
        kind: peryx_http::IndexKind::Hosted {
            upload_token: None,
            volatile: false,
        },
        policy: peryx_policy::Policy::default(),
    };
    let hosted = |name: &str, route: &str| {
        oci_index(
            name,
            route,
            peryx_http::IndexKind::Hosted {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
        )
    };
    // A root-route index lists its repos under bare names; a prefixed one under the route.
    let (_state, app) = app_with_indexes(&dir, vec![pypi, hosted("store", "store"), hosted("root", "")]);
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    for uri in [
        "/v2/store/app1/manifests/v1",
        "/v2/store/app2/manifests/v1",
        "/v2/bare/manifests/v1",
    ] {
        let (status, _, _) = super::send_body(
            &app,
            Method::PUT,
            uri,
            &[
                ("authorization", &super::auth("s3cret")),
                ("content-type", MANIFEST_TYPE),
            ],
            manifest.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // The catalog lists OCI repos as clients address them; the pypi index contributes nothing.
    let (status, _, body) = send(&app, Method::GET, "/v2/_catalog").await;
    assert_eq!(status, StatusCode::OK);
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        doc["repositories"],
        serde_json::json!(["bare", "store/app1", "store/app2"])
    );

    let (status, headers, body) = send(&app, Method::GET, "/v2/_catalog?n=1").await;
    assert_eq!(status, StatusCode::OK);
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["repositories"], serde_json::json!(["bare"]));
    assert!(
        headers[header::LINK]
            .to_str()
            .unwrap()
            .contains("_catalog?n=1&last=bare")
    );
}

#[tokio::test]
async fn test_blob_range_that_is_not_a_range_serves_the_whole_blob() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let blob = b"0123456789";
    let stored = state.blobs.write(blob).unwrap();
    let uri = format!("/v2/store/app/blobs/sha256:{}", stored.as_str());

    // RFC 9110 s14.2: an unparseable `Range` is ignored, never refused.
    for header in ["bytes=abc-", "bytes=-", "bytes=5-2"] {
        let (status, _, got) = send_with(&app, Method::GET, &uri, &[("range", header)]).await;
        assert_eq!(status, StatusCode::OK, "{header}");
        assert_eq!(got, &blob[..], "{header}");
    }

    // RFC 9110 s14.1.2: a suffix longer than the blob uses the whole blob.
    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("range", "bytes=-99")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 0-9/{}", blob.len()));
    assert_eq!(got, &blob[..]);
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
