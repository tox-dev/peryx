use super::support::*;
use crate::tests::observe_pending;

#[rstest]
#[case::lower("bearer")]
#[case::mixed("bEaReR")]
#[tokio::test]
async fn test_token_flow_accepts_case_insensitive_challenge(#[case] scheme: &str) {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"{scheme} realm="{}/token",service="reg""#, server.uri()).as_str(),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
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
    // The response limit bounds memory used by hostile upstreams.
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
    // The gate makes both pulls contend for one upstream fetch.
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

    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("range", "bytes=0-3")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 0-3/{}", blob.len()));
    assert_eq!(got, &blob[..4]);
}

#[rstest]
#[case::absent(404, StatusCode::NOT_FOUND, Some("BLOB_UNKNOWN"))]
#[case::present(200, StatusCode::OK, None)]
#[case::failure(500, StatusCode::BAD_GATEWAY, None)]
#[tokio::test]
async fn test_cached_blob_checks_the_target_upstream_repository(
    #[case] upstream_status: u16,
    #[case] expected_status: StatusCode,
    #[case] expected_error: Option<&str>,
) {
    let server = MockServer::start().await;
    let blob = b"cached-upstream-layer";
    let digest = oci_digest(blob);
    Mock::given(method("GET"))
        .and(path(format!("/v2/first/app/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(blob.to_vec(), "application/octet-stream"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/second/app/blobs/{digest}")))
        .respond_with(
            ResponseTemplate::new(upstream_status).insert_header("content-length", blob.len().to_string().as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let first = send(&app, Method::GET, &format!("/v2/hub/first/app/blobs/{digest}"))
        .await
        .0;
    let (second, _, body) = send(&app, Method::GET, &format!("/v2/hub/second/app/blobs/{digest}")).await;

    assert_eq!(
        (
            first,
            second,
            body.as_ref() == blob,
            expected_error.map(|code| body_has_code(&body, code)),
        ),
        (
            StatusCode::OK,
            expected_status,
            expected_status == StatusCode::OK,
            expected_error.map(|_| true),
        ),
        "{body:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_blob_misses_share_one_upstream_fetch() {
    let server = MockServer::start().await;
    let blob = b"a-layer-two-clients-race-for";
    let digest = oci_digest(blob);
    let (gate, response) =
        gated_response(ResponseTemplate::new(200).set_body_raw(blob.to_vec(), "application/octet-stream"));
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/alpine/blobs/{digest}")))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let uri = format!("/v2/hub/library/alpine/blobs/{digest}");
    let first = tokio::spawn({
        let app = app.clone();
        let uri = uri.clone();
        async move { send(&app, Method::GET, &uri).await }
    });
    let release = gate.entered().await;
    let (second, second_pending) = observe_pending({
        let app = app.clone();
        let uri = uri.clone();
        async move { send(&app, Method::GET, &uri).await }
    });
    second_pending.await.unwrap();
    drop(release);
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
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
    let stored = state.serving.blobs.put_bytes(blob).await.unwrap();
    let digest = format!("sha256:{}", stored.as_str());
    crate::store::record_blob_membership(&state.serving.meta, "store", "app", &digest).unwrap();
    let uri = format!("/v2/store/app/blobs/{digest}");

    let (status, headers, got) = send(&app, Method::HEAD, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], blob.len().to_string());
    assert!(got.is_empty());

    // RFC 9110 section 14.2 excludes range processing from `HEAD`.
    let (status, headers, _) = send_with(&app, Method::HEAD, &uri, &[("range", "bytes=5-6")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], blob.len().to_string());
    assert!(!headers.contains_key(header::CONTENT_RANGE));

    let (status, headers, _) = send_with(&app, Method::HEAD, &uri, &[("range", "bytes=50-60")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], blob.len().to_string());
    assert!(!headers.contains_key(header::CONTENT_RANGE));

    let (status, headers, _) = send_with(&app, Method::GET, &uri, &[("range", "bytes=50-60")]).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes */{}", blob.len()));
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");

    // RFC 7233 permits serving the full body when multipart ranges are unsupported.
    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("range", "bytes=0-1,3-4")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], blob.len().to_string());
    assert_eq!(got, &blob[..]);

    let absent = format!("/v2/store/app/blobs/{}", oci_digest(b"absent-blob"));
    let (status, _, _) = send(&app, Method::HEAD, &absent).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
const LAYER: &[u8] = b"0123456789";

const STALE: &str = "\"sha256:0000\"";

fn layer_etag() -> String {
    format!("\"{}\"", oci_digest(LAYER))
}

fn stored_layer(dir: &tempfile::TempDir) -> (axum::Router, String) {
    let (state, app) = hosted(dir);
    let digest = oci_digest(LAYER);
    state.serving.blobs.blocking().put_bytes(LAYER).unwrap();
    crate::store::record_blob_membership(&state.serving.meta, "store", "app", &digest).unwrap();
    (app, format!("/v2/store/app/blobs/{digest}"))
}

#[rstest]
#[case::get(Method::GET)]
#[case::head(Method::HEAD)]
#[tokio::test]
async fn test_blob_is_served_under_its_digest_as_an_entity_tag(#[case] method: Method) {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored_layer(&dir);

    let (status, headers, _) = send(&app, method, &uri).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::ETAG], layer_etag());
}
#[tokio::test]
async fn test_blob_get_serves_a_range_an_if_range_still_names() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored_layer(&dir);

    let conditional = [("if-range", &*layer_etag()), ("range", "bytes=5-6")];
    let (status, headers, got) = send_with(&app, Method::GET, &uri, &conditional).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 5-6/{}", LAYER.len()));
    assert_eq!(got, &LAYER[5..=6]);
}
// RFC 9110 section 14.2 excludes range processing from `HEAD`.
#[tokio::test]
async fn test_blob_head_ignores_a_range_a_matching_if_range_names() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored_layer(&dir);

    let conditional = [("if-range", &*layer_etag()), ("range", "bytes=5-6")];
    let (status, headers, got) = send_with(&app, Method::HEAD, &uri, &conditional).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], LAYER.len().to_string());
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert!(got.is_empty());
}
#[rstest]
#[case::stale_tag(STALE)]
#[case::weak_tag(&format!("W/{}", layer_etag()))]
#[case::date("Wed, 21 Oct 2015 07:28:00 GMT")]
#[case::malformed("sha256:0000")]
#[tokio::test]
async fn test_blob_serves_the_whole_layer_for_a_range_a_stale_if_range_asks_for(#[case] field: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored_layer(&dir);

    let conditional = [("if-range", field), ("range", "bytes=5-6")];
    let (status, headers, got) = send_with(&app, Method::GET, &uri, &conditional).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert_eq!(got, LAYER);
}
#[tokio::test]
async fn test_blob_head_drops_a_range_a_stale_if_range_asks_for() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored_layer(&dir);

    let conditional = [("if-range", STALE), ("range", "bytes=5-6")];
    let (status, headers, _) = send_with(&app, Method::HEAD, &uri, &conditional).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], LAYER.len().to_string());
    assert!(!headers.contains_key(header::CONTENT_RANGE));
}
// Stale range metadata falls back to the full body.
#[tokio::test]
async fn test_stale_if_range_serves_the_whole_layer_rather_than_refusing_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored_layer(&dir);

    let conditional = [("if-range", STALE), ("range", "bytes=50-60")];
    let (status, _, got) = send_with(&app, Method::GET, &uri, &conditional).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, LAYER);
}
#[tokio::test]
async fn test_if_range_without_a_range_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored_layer(&dir);

    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("if-range", STALE)]).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert_eq!(got, LAYER);
}
#[tokio::test]
async fn test_repeated_range_lines_serve_the_whole_layer_as_a_multi_range() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored_layer(&dir);

    let repeated = [("range", "bytes=0-1"), ("range", "bytes=4-5")];
    let (status, headers, got) = send_with(&app, Method::GET, &uri, &repeated).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert_eq!(got, LAYER);
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

    // A truncated upstream body must not enter the cache.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}/");
    let server = std::thread::spawn(move || {
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
    server.join().unwrap();
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
    let stored = state.serving.blobs.put_bytes(blob).await.unwrap();
    let digest = format!("sha256:{}", stored.as_str());
    crate::store::record_blob_membership(&state.serving.meta, "store", "app", &digest).unwrap();
    let uri = format!("/v2/store/app/blobs/{digest}");

    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("range", "bytes=-3")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 7-9/{}", blob.len()));
    assert_eq!(got, &blob[7..]);

    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("range", "bytes=8-")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 8-9/{}", blob.len()));
    assert_eq!(got, &blob[8..]);

    let (status, _, got) = send_with(&app, Method::GET, &uri, &[("range", "chunks=1-2")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}
#[tokio::test]
async fn test_concurrent_tag_pulls_share_one_upstream_fetch() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    // The gate makes both pulls contend for one upstream fetch.
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
    let stored = state.serving.blobs.put_bytes(blob).await.unwrap();
    let digest = format!("sha256:{}", stored.as_str());
    crate::store::record_blob_membership(&state.serving.meta, "store", "app", &digest).unwrap();
    let uri = format!("/v2/store/app/blobs/{digest}");

    // RFC 9110 section 14.2 permits ignoring an invalid `Range`.
    for header in ["bytes=abc-", "bytes=-", "bytes=5-2"] {
        let (status, _, got) = send_with(&app, Method::GET, &uri, &[("range", header)]).await;
        assert_eq!(status, StatusCode::OK, "{header}");
        assert_eq!(got, &blob[..], "{header}");
    }

    // RFC 9110 section 14.1.2 expands an oversized suffix to the full body.
    let (status, headers, got) = send_with(&app, Method::GET, &uri, &[("range", "bytes=-99")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 0-9/{}", blob.len()));
    assert_eq!(got, &blob[..]);
}

struct RemoteBlob {
    blobs: peryx_storage::blob::BlobStorage,
    content: bytes::Bytes,
}

struct UnavailableBlob;

#[async_trait::async_trait]
impl peryx_ha::BlobAvailability for UnavailableBlob {
    async fn ensure_local(
        &self,
        _digest: &peryx_storage::blob::Digest,
    ) -> Result<Option<peryx_storage::blob::BlobMetadata>, peryx_ha::BlobAvailabilityError> {
        Err(peryx_ha::BlobAvailabilityError::new(
            peryx_ha::BlobAvailabilityFailure::Transfer,
            std::io::Error::other("peer unavailable"),
        ))
    }
}

#[async_trait::async_trait]
impl peryx_ha::BlobAvailability for RemoteBlob {
    async fn ensure_local(
        &self,
        digest: &peryx_storage::blob::Digest,
    ) -> Result<Option<peryx_storage::blob::BlobMetadata>, peryx_ha::BlobAvailabilityError> {
        self.blobs.put_bytes_as(&self.content, digest).await.unwrap();
        Ok(self.blobs.head(digest).await.unwrap())
    }
}

fn hosted_with_remote_blob(
    dir: &tempfile::TempDir,
    content: bytes::Bytes,
) -> (std::sync::Arc<peryx_driver::AppState>, axum::Router) {
    let blobs = peryx_storage::blob::BlobStorage::filesystem(dir.path().join("blobs"));
    hosted_with_availability(dir, std::sync::Arc::new(RemoteBlob { blobs, content }))
}

fn hosted_with_availability(
    dir: &tempfile::TempDir,
    availability: std::sync::Arc<dyn peryx_ha::BlobAvailability>,
) -> (std::sync::Arc<peryx_driver::AppState>, axum::Router) {
    app_with_setup(
        dir,
        vec![oci_index(
            "store",
            "store",
            peryx_index::IndexKind::Hosted { volatile: false },
        )],
        false,
        move |state| {
            install_test_distributed(state, Some(availability));
        },
    )
}

#[tokio::test]
async fn test_authorized_blob_missing_bytes_serves_from_a_remote_placement() {
    let dir = tempfile::tempdir().unwrap();
    let blob = bytes::Bytes::from_static(b"an oci layer held only by a peer datacenter");
    let (state, app) = hosted_with_remote_blob(&dir, blob.clone());
    let digest = oci_digest(&blob);
    store::record_blob_membership(&state.serving.meta, "store", "app", &digest).unwrap();
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, blob);
}

#[tokio::test]
async fn test_remote_placement_failure_returns_blob_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_with_availability(&dir, std::sync::Arc::new(UnavailableBlob));
    let digest = oci_digest(b"remote layer");
    store::record_blob_membership(&state.serving.meta, "store", "app", &digest).unwrap();

    let (status, _, body) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "BLOB_UNKNOWN"));
}
