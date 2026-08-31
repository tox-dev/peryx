//! `If-None-Match` on a manifest pull: the entity tag names the representation `Accept` settled on.

use std::sync::LazyLock;

use axum::http::{Method, StatusCode, header};
use rstest::rstest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{auth, hosted_writable, image_manifest, oci_digest, proxy, seed_config, send, send_body, send_with};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const LIST_TYPE: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const IMAGE_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json";

static MANIFEST: LazyLock<Vec<u8>> = LazyLock::new(|| image_manifest(MANIFEST_TYPE, ""));
static DOCKER_CHILD: LazyLock<Vec<u8>> = LazyLock::new(|| image_manifest(IMAGE_ACCEPT, ""));

fn manifest_etag() -> String {
    format!("\"{}\"", oci_digest(&MANIFEST))
}

async fn push(app: &axum::Router, reference: &str, media_type: &str, body: &[u8]) {
    let (status, _, reply) = send_body(
        app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{reference}"),
        &[("authorization", &auth(TOKEN)), ("content-type", media_type)],
        body.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reply:?}");
}

/// A hosted index serving [`MANIFEST`] under the tag `v1`.
async fn tagged(dir: &tempfile::TempDir) -> axum::Router {
    let (_state, app) = hosted_writable(dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    push(&app, "v1", MANIFEST_TYPE, &MANIFEST).await;
    app
}

#[rstest]
#[case::tag("v1")]
#[case::digest(&oci_digest(&MANIFEST))]
#[tokio::test]
async fn test_manifest_is_served_under_its_digest_as_an_entity_tag(#[case] reference: &str) {
    let dir = tempfile::tempdir().unwrap();
    let app = tagged(&dir).await;

    let (status, headers, _) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{reference}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::ETAG], manifest_etag());
}

#[rstest]
#[case::tag("v1")]
#[case::digest(&oci_digest(&MANIFEST))]
#[tokio::test]
async fn test_manifest_matching_if_none_match_is_not_modified(#[case] reference: &str) {
    let dir = tempfile::tempdir().unwrap();
    let app = tagged(&dir).await;
    let uri = format!("/v2/store/app/manifests/{reference}");

    let (status, headers, body) = send_with(&app, Method::GET, &uri, &[("if-none-match", &manifest_etag())]).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], manifest_etag());
    assert_eq!(headers["docker-content-digest"], oci_digest(&MANIFEST));
    assert_eq!(headers[header::VARY], "accept");
    assert!(body.is_empty());
}

#[rstest]
#[case::weak(&format!("W/{}", manifest_etag()))]
#[case::any("*")]
#[case::list(&format!("\"sha256:0000\", {}", manifest_etag()))]
#[tokio::test]
async fn test_manifest_if_none_match_matches_weakly(#[case] field: &str) {
    let dir = tempfile::tempdir().unwrap();
    let app = tagged(&dir).await;

    let (status, _, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/v1",
        &[("if-none-match", field)],
    )
    .await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
}

#[rstest]
#[case::other_digest("\"sha256:0000\"")]
#[case::malformed("not-a-tag")]
#[tokio::test]
async fn test_manifest_if_none_match_it_does_not_meet_serves_the_document(#[case] field: &str) {
    let dir = tempfile::tempdir().unwrap();
    let app = tagged(&dir).await;

    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/v1",
        &[("if-none-match", field)],
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::ETAG], manifest_etag());
    assert_eq!(body, MANIFEST[..]);
}

#[tokio::test]
async fn test_matching_if_none_match_in_a_later_field_line_is_not_modified() {
    let dir = tempfile::tempdir().unwrap();
    let app = tagged(&dir).await;

    let repeated = [
        ("if-none-match", "\"sha256:0000\""),
        ("if-none-match", &*manifest_etag()),
    ];
    let (status, _, body) = send_with(&app, Method::GET, "/v2/store/app/manifests/v1", &repeated).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
}

#[tokio::test]
async fn test_manifest_head_matching_if_none_match_is_not_modified() {
    let dir = tempfile::tempdir().unwrap();
    let app = tagged(&dir).await;

    let (status, headers, body) = send_with(
        &app,
        Method::HEAD,
        "/v2/store/app/manifests/v1",
        &[("if-none-match", &manifest_etag())],
    )
    .await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], manifest_etag());
    assert!(body.is_empty());
}

/// A `304` validated the client's copy, so it carries the policy of the `200` it refreshes rather
/// than the `no-store` a refusal gets.
#[tokio::test]
async fn test_not_modified_keeps_the_revocation_cache_policy() {
    let dir = tempfile::tempdir().unwrap();
    let app = tagged(&dir).await;

    let (status, headers, _) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/v1",
        &[("if-none-match", &manifest_etag())],
    )
    .await;

    assert_eq!(
        (status, headers[header::CACHE_CONTROL].to_str().unwrap()),
        (
            StatusCode::NOT_MODIFIED,
            "public, max-age=60, must-revalidate, no-transform",
        )
    );
}

/// The tag answers the list or its `linux/amd64` child depending on `Accept`, so the entity tag has
/// to name whichever one this request settled on.
#[tokio::test]
async fn test_negotiated_child_carries_the_child_entity_tag() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let child = oci_digest(&DOCKER_CHILD);
    push(&app, &child, IMAGE_ACCEPT, &DOCKER_CHILD).await;
    let list = format!(
        r#"{{"schemaVersion":2,"mediaType":"{LIST_TYPE}","manifests":[{{"mediaType":"{IMAGE_ACCEPT}","digest":"{child}","size":{},"platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
        DOCKER_CHILD.len(),
    );
    push(&app, "multi", LIST_TYPE, list.as_bytes()).await;
    let uri = "/v2/store/app/manifests/multi";
    let child_etag = format!("\"{child}\"");
    let list_etag = format!("\"{}\"", oci_digest(list.as_bytes()));

    let (served, headers, _) = send_with(&app, Method::GET, uri, &[("accept", IMAGE_ACCEPT)]).await;
    let matched = send_with(
        &app,
        Method::GET,
        uri,
        &[("accept", IMAGE_ACCEPT), ("if-none-match", &child_etag)],
    )
    .await;
    let stale = send_with(
        &app,
        Method::GET,
        uri,
        &[("accept", IMAGE_ACCEPT), ("if-none-match", &list_etag)],
    )
    .await;

    assert_eq!(
        (served, &headers[header::ETAG]),
        (StatusCode::OK, &child_etag.parse().unwrap())
    );
    assert_eq!((matched.0, matched.2.is_empty()), (StatusCode::NOT_MODIFIED, true));
    assert_eq!(
        (stale.0, stale.2.as_ref()),
        (StatusCode::OK, &DOCKER_CHILD[..]),
        "the tag the client holds names the list, not the child it is served"
    );
}

#[tokio::test]
async fn test_proxied_tag_matching_if_none_match_is_not_modified() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(MANIFEST.clone(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let uri = "/v2/hub/library/nginx/manifests/latest";

    let (pulled, _, bytes) = send(&app, Method::GET, uri).await;
    let (status, headers, body) = send_with(&app, Method::GET, uri, &[("if-none-match", &manifest_etag())]).await;

    assert_eq!((pulled, bytes.as_ref()), (StatusCode::OK, &MANIFEST[..]));
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], manifest_etag());
    assert!(body.is_empty());
}

/// A reference no member serves keeps its `404`: there is no representation for the tag to validate.
#[tokio::test]
async fn test_absent_manifest_with_a_matching_if_none_match_is_still_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let uri = format!("/v2/store/app/manifests/{}", oci_digest(&MANIFEST));

    let (status, _, body) = send_with(&app, Method::GET, &uri, &[("if-none-match", &manifest_etag())]).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}
