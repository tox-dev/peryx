//! `If-None-Match` on a blob pull, RFC 9110 section 13.1.2.

use axum::http::{Method, StatusCode, header};
use rstest::rstest;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{hosted, oci_digest, proxy, send, send_with};

const BLOB: &[u8] = b"the-layer-bytes-0123456789";

fn blob_digest() -> String {
    oci_digest(BLOB)
}

fn blob_etag() -> String {
    format!("\"{}\"", blob_digest())
}

/// A hosted index holding [`BLOB`], and the pull URI for it.
async fn stored(dir: &TempDir) -> (axum::Router, String) {
    let (state, app) = hosted(dir);
    let digest = format!("sha256:{}", state.serving.blobs.put_bytes(BLOB).await.unwrap().as_str());
    crate::store::record_blob_membership(&state.serving.meta, "store", "app", &digest).unwrap();
    (app, format!("/v2/store/app/blobs/{digest}"))
}

#[rstest]
#[case::get(Method::GET)]
#[case::head(Method::HEAD)]
#[tokio::test]
async fn test_blob_is_served_under_its_digest_as_an_entity_tag(#[case] verb: Method) {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored(&dir).await;

    let (status, headers, _) = send(&app, verb, &uri).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::ETAG], blob_etag());
}

#[rstest]
#[case::exact(&blob_etag())]
#[case::weak(&format!("W/{}", blob_etag()))]
#[case::any("*")]
#[case::list(&format!("\"sha256:0000\", {}", blob_etag()))]
#[tokio::test]
async fn test_stored_blob_matching_if_none_match_is_not_modified(#[case] field: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored(&dir).await;

    let (status, headers, body) = send_with(&app, Method::GET, &uri, &[("if-none-match", field)]).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], blob_etag());
    assert_eq!(headers["docker-content-digest"], blob_digest());
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert!(body.is_empty());
}

#[rstest]
#[case::other_digest("\"sha256:0000\"")]
#[case::malformed("not-a-tag")]
#[tokio::test]
async fn test_blob_if_none_match_it_does_not_meet_serves_the_bytes(#[case] field: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored(&dir).await;

    let (status, headers, body) = send_with(&app, Method::GET, &uri, &[("if-none-match", field)]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::ETAG], blob_etag());
    assert_eq!(body, BLOB);
}

#[tokio::test]
async fn test_matching_if_none_match_in_a_later_field_line_is_not_modified() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored(&dir).await;

    let repeated = [("if-none-match", "\"sha256:0000\""), ("if-none-match", &*blob_etag())];
    let (status, _, body) = send_with(&app, Method::GET, &uri, &repeated).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
}

#[tokio::test]
async fn test_matching_if_none_match_answers_before_the_range_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored(&dir).await;

    let conditional = [("if-none-match", &*blob_etag()), ("range", "bytes=0-3")];
    let (status, headers, body) = send_with(&app, Method::GET, &uri, &conditional).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert!(body.is_empty());
}

#[tokio::test]
async fn test_range_is_served_when_if_none_match_holds_other_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored(&dir).await;

    let conditional = [("if-none-match", "\"sha256:0000\""), ("range", "bytes=0-3")];
    let (status, headers, body) = send_with(&app, Method::GET, &uri, &conditional).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 0-3/{}", BLOB.len()));
    assert_eq!(body, &BLOB[..4]);
}

#[tokio::test]
async fn test_stored_blob_head_matching_if_none_match_is_not_modified() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored(&dir).await;

    let (status, headers, body) = send_with(&app, Method::HEAD, &uri, &[("if-none-match", &blob_etag())]).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], blob_etag());
    assert!(body.is_empty());
}

/// RFC 9112 s6.2 admits a `Content-Length` on a `304` only at the length the `200` would have sent.
/// The `200` for a layer states the layer's size, and the condition outranks a `Range` (RFC 9110
/// s13.1.2), so neither the whole pull nor a sliced one leaves a length behind on the `304`.
#[rstest]
#[case::whole(None)]
#[case::ranged(Some("bytes=0-3"))]
#[tokio::test]
async fn test_a_not_modified_blob_states_no_length(#[case] range: Option<&str>) {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored(&dir).await;
    let (_, served, _) = send(&app, Method::GET, &uri).await;
    let etag = blob_etag();
    let mut conditional = vec![("if-none-match", etag.as_str())];
    conditional.extend(range.map(|range| ("range", range)));

    let (status, headers, _) = send_with(&app, Method::GET, &uri, &conditional).await;

    assert_eq!(
        (
            status,
            headers.contains_key(header::CONTENT_LENGTH),
            served[header::CONTENT_LENGTH].to_str().unwrap(),
        ),
        (StatusCode::NOT_MODIFIED, false, BLOB.len().to_string().as_str())
    );
}

/// A `304` validated the client's copy, so it carries the policy of the `200` it refreshes rather
/// than the `no-store` a refusal gets.
#[tokio::test]
async fn test_not_modified_keeps_the_revocation_cache_policy() {
    let dir = tempfile::tempdir().unwrap();
    let (app, uri) = stored(&dir).await;

    let (status, headers, _) = send_with(&app, Method::GET, &uri, &[("if-none-match", &blob_etag())]).await;

    assert_eq!(
        (status, headers[header::CACHE_CONTROL].to_str().unwrap()),
        (
            StatusCode::NOT_MODIFIED,
            "public, max-age=60, must-revalidate, no-transform",
        )
    );
}

#[tokio::test]
async fn test_proxied_blob_matching_if_none_match_is_not_modified() {
    let server = MockServer::start().await;
    let digest = blob_digest();
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/alpine/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(BLOB.to_vec(), "application/octet-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let uri = format!("/v2/hub/library/alpine/blobs/{digest}");

    let (pulled, _, bytes) = send(&app, Method::GET, &uri).await;
    let (status, headers, body) = send_with(&app, Method::GET, &uri, &[("if-none-match", &blob_etag())]).await;

    assert_eq!((pulled, bytes.as_ref()), (StatusCode::OK, BLOB));
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], blob_etag());
    assert!(body.is_empty());
}

/// A `HEAD` for bytes this repository is not linked to yet resolves existence upstream, and that
/// answer is conditional too: the digest names the bytes wherever they are held.
#[tokio::test]
async fn test_upstream_head_matching_if_none_match_is_not_modified() {
    let server = MockServer::start().await;
    let digest = blob_digest();
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/library/alpine/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).insert_header("content-length", BLOB.len().to_string().as_str()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let uri = format!("/v2/hub/library/alpine/blobs/{digest}");

    let (status, headers, body) = send_with(&app, Method::HEAD, &uri, &[("if-none-match", &blob_etag())]).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers["docker-content-digest"], digest);
    assert!(body.is_empty());
}

/// The condition is judged on content this repository can serve, so an absent digest keeps its `404`
/// rather than telling a client its copy is still current.
#[tokio::test]
async fn test_absent_blob_with_a_matching_if_none_match_is_still_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted(&dir);
    let uri = format!("/v2/store/app/blobs/{}", blob_digest());

    let (status, _, body) = send_with(&app, Method::GET, &uri, &[("if-none-match", &blob_etag())]).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "BLOB_UNKNOWN"), "{body:?}");
}
