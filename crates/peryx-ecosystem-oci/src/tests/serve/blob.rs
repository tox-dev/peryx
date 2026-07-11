//! Serving, ingesting and range-reading blobs.

use super::support::*;

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
async fn test_blob_upstream_401_reports_the_auth_failure() {
    let server = MockServer::start().await;
    let digest = format!("sha256:{}", "e".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body_has_code(&body, "UNAUTHORIZED"), "{body:?}");
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
