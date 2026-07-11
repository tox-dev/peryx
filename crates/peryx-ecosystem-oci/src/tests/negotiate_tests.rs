//! Serving an index to a client that will not accept a list media type rewrites it to the
//! `linux/amd64` child, the negotiation `distribution` does for legacy Docker (< 17.06).

use axum::http::{Method, StatusCode, header};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{auth, hosted_writable, oci_digest, proxy, send_body, send_with};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
/// A legacy client that accepts only the schema-2 image manifest, never a list.
const IMAGE_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json";

const CHILD: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;

/// An index whose one `linux/amd64` child is `child_digest`.
fn amd64_index(child_digest: &str) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{INDEX_TYPE}","manifests":[{{"mediaType":"{MANIFEST_TYPE}","digest":"{child_digest}","platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
    )
    .into_bytes()
}

async fn push(app: &axum::Router, reference: &str, media_type: &str, body: &[u8]) {
    let (status, _, body) = send_body(
        app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{reference}"),
        &[("authorization", &auth(TOKEN)), ("content-type", media_type)],
        body.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
}

/// A hosted store carrying the `linux/amd64` child by digest and an index tag `multi` naming it.
async fn hosted_index() -> (tempfile::TempDir, axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let child_digest = oci_digest(CHILD);
    push(&app, &child_digest, MANIFEST_TYPE, CHILD).await;
    push(&app, "multi", INDEX_TYPE, &amd64_index(&child_digest)).await;
    (dir, app, child_digest)
}

#[tokio::test]
async fn test_get_serves_the_amd64_child_when_accept_excludes_the_index() {
    let (_dir, app, child_digest) = hosted_index().await;
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], child_digest);
    assert_eq!(headers[header::CONTENT_TYPE], MANIFEST_TYPE);
    assert_eq!(body, CHILD);
}

#[tokio::test]
async fn test_get_by_digest_serves_the_amd64_child() {
    let (_dir, app, child_digest) = hosted_index().await;
    let index_digest = oci_digest(&amd64_index(&child_digest));
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        &format!("/v2/store/app/manifests/{index_digest}"),
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    // A pull by the index digest negotiates the same way a pull by tag does.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], child_digest);
    assert_eq!(body, CHILD);
}

#[tokio::test]
async fn test_head_serves_the_amd64_child_headers_with_no_body() {
    let (_dir, app, child_digest) = hosted_index().await;
    let (status, headers, body) = send_with(
        &app,
        Method::HEAD,
        "/v2/store/app/manifests/multi",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], child_digest);
    assert_eq!(headers[header::CONTENT_TYPE], MANIFEST_TYPE);
    assert_eq!(headers[header::CONTENT_LENGTH], CHILD.len().to_string());
    assert!(body.is_empty());
}

#[tokio::test]
async fn test_get_serves_the_index_when_accept_includes_it() {
    let (_dir, app, child_digest) = hosted_index().await;
    let index = amd64_index(&child_digest);
    let accept = format!("{INDEX_TYPE}, {IMAGE_ACCEPT}");
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", &accept)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(&index));
    assert_eq!(headers[header::CONTENT_TYPE], INDEX_TYPE);
    assert_eq!(body, index);
}

#[tokio::test]
async fn test_get_without_an_accept_header_serves_the_index() {
    let (_dir, app, child_digest) = hosted_index().await;
    let index = amd64_index(&child_digest);
    let (status, headers, body) = send_with(&app, Method::GET, "/v2/store/app/manifests/multi", &[]).await;
    // No Accept header is not a legacy client refusing the index, so the index is served unchanged.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(&index));
    assert_eq!(body, index);
}

#[tokio::test]
async fn test_get_of_a_plain_image_is_unaffected() {
    let (_dir, app, child_digest) = hosted_index().await;
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        &format!("/v2/store/app/manifests/{child_digest}"),
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    // The child is an image manifest, not an index, so negotiation leaves it untouched.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], child_digest);
    assert_eq!(body, CHILD);
}

#[tokio::test]
async fn test_get_of_an_index_without_an_amd64_child_serves_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let child_digest = oci_digest(CHILD);
    push(&app, &child_digest, MANIFEST_TYPE, CHILD).await;
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"{INDEX_TYPE}","manifests":[{{"mediaType":"{MANIFEST_TYPE}","digest":"{child_digest}","platform":{{"os":"linux","architecture":"arm64"}}}}]}}"#,
    )
    .into_bytes();
    push(&app, "multi", INDEX_TYPE, &index).await;
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    // No linux/amd64 child to substitute, so the index is served as-is.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(&index));
    assert_eq!(body, index);
}

#[tokio::test]
async fn test_get_serves_the_index_when_the_amd64_child_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    // A hosted push validates the child is present, so the index is seeded straight into the store to
    // model a child that resolves to nothing; the loop then walks the lone member and serves the index.
    let child_digest = format!("sha256:{}", "e".repeat(64));
    let index = amd64_index(&child_digest);
    let index_digest = oci_digest(&index);
    crate::store::put_manifest(
        &state.meta,
        &index_digest,
        &crate::store::Manifest {
            media_type: INDEX_TYPE.to_owned(),
            bytes: index.clone(),
        },
    )
    .unwrap();
    crate::store::put_tag(&state.meta, "store", "app", "multi", &index_digest).unwrap();
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], index_digest);
    assert_eq!(body, index);
}

#[tokio::test]
async fn test_get_fetches_the_amd64_child_from_a_proxy_member() {
    let server = MockServer::start().await;
    let child_digest = oci_digest(CHILD);
    let index = amd64_index(&child_digest);
    for (reference, body, media_type) in [
        ("latest", index.clone(), INDEX_TYPE),
        (child_digest.as_str(), CHILD.to_vec(), MANIFEST_TYPE),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/v2/library/app/manifests/{reference}")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, media_type))
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/hub/library/app/manifests/latest",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    // The index cached from the tag pull holds no child locally, so the child is fetched by digest
    // from the proxy's upstream and served.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], child_digest);
    assert_eq!(body, CHILD);
}
