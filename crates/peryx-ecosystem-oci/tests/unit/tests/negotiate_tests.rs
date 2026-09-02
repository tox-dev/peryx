use std::sync::LazyLock;

use axum::http::{Method, StatusCode, header};
use rstest::rstest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    auth, body_has_code, hosted_writable, image_manifest, mount_head_without_digest, oci_digest, proxy, seed_config,
    send_body, send_with,
};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const IMAGE_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json";
const LIST_TYPE: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

static CHILD: LazyLock<Vec<u8>> = LazyLock::new(|| image_manifest(MANIFEST_TYPE, ""));
static DOCKER_CHILD: LazyLock<Vec<u8>> = LazyLock::new(|| image_manifest(IMAGE_ACCEPT, ""));

fn amd64_list(media_type: &str, child_media_type: &str, child_digest: &str, child_size: usize) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{media_type}","manifests":[{{"mediaType":"{child_media_type}","digest":"{child_digest}","size":{child_size},"platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
    )
    .into_bytes()
}

fn amd64_index(child_digest: &str) -> Vec<u8> {
    amd64_list(INDEX_TYPE, MANIFEST_TYPE, child_digest, CHILD.len())
}

fn amd64_docker_list(child_digest: &str) -> Vec<u8> {
    amd64_list(LIST_TYPE, IMAGE_ACCEPT, child_digest, DOCKER_CHILD.len())
}

async fn push_to(
    app: &axum::Router,
    repository: &str,
    reference: &str,
    media_type: &str,
    body: &[u8],
) -> (StatusCode, bytes::Bytes) {
    let (status, _, body) = send_body(
        app,
        Method::PUT,
        &format!("/v2/{repository}/manifests/{reference}"),
        &[("authorization", &auth(TOKEN)), ("content-type", media_type)],
        body.to_vec(),
    )
    .await;
    (status, body)
}

async fn push(app: &axum::Router, reference: &str, media_type: &str, body: &[u8]) {
    let (status, body) = push_to(app, "store/app", reference, media_type, body).await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
}

async fn hosted_index() -> (tempfile::TempDir, axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let child_digest = oci_digest(&CHILD);
    push(&app, &child_digest, MANIFEST_TYPE, &CHILD).await;
    push(&app, "multi", INDEX_TYPE, &amd64_index(&child_digest)).await;
    (dir, app, child_digest)
}

async fn hosted_docker_list() -> (tempfile::TempDir, axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let child_digest = oci_digest(&DOCKER_CHILD);
    push(&app, &child_digest, IMAGE_ACCEPT, &DOCKER_CHILD).await;
    push(&app, "multi", LIST_TYPE, &amd64_docker_list(&child_digest)).await;
    (dir, app, child_digest)
}

#[tokio::test]
async fn test_get_serves_the_amd64_child_when_accept_excludes_the_docker_list() {
    let (_dir, app, child_digest) = hosted_docker_list().await;
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], child_digest);
    assert_eq!(headers[header::CONTENT_TYPE], IMAGE_ACCEPT);
    assert_eq!(headers[header::VARY], "accept");
    assert_eq!(body, *DOCKER_CHILD);
}

#[rstest]
#[case::wildcard_type("*/*")]
#[case::bare_star("*")]
#[case::empty("")]
#[tokio::test]
async fn test_get_with_a_wildcard_or_empty_accept_serves_the_index(#[case] accept: &str) {
    let (_dir, app, child_digest) = hosted_index().await;
    let index = amd64_index(&child_digest);
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", accept)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(&index));
    assert_eq!(headers[header::CONTENT_TYPE], INDEX_TYPE);
    assert_eq!(headers[header::VARY], "accept");
    assert_eq!(body, index);
}

#[rstest]
#[case::type_wildcard("application/*")]
#[case::exact_with_positive_quality("application/vnd.oci.image.index.v1+json;q=0.5")]
#[case::higher_quality_duplicate_wins("application/*;q=0.7, application/*;q=0")]
#[case::non_quality_parameters_ignored("application/*;charset=utf-8;profile")]
#[tokio::test]
async fn test_get_serves_the_index_when_a_media_range_covers_it(#[case] accept: &str) {
    let (_dir, app, child_digest) = hosted_index().await;
    let index = amd64_index(&child_digest);
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", accept)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(&index));
    assert_eq!(headers[header::CONTENT_TYPE], INDEX_TYPE);
    assert_eq!(body, index);
}

#[rstest]
#[case::explicit_zero("application/vnd.oci.image.index.v1+json;q=0")]
#[case::exclusion_outranks_wildcard("application/vnd.oci.image.index.v1+json;q=0, */*")]
#[tokio::test]
async fn test_get_rejects_an_oci_index_when_a_media_range_excludes_it(#[case] accept: &str) {
    let (_dir, app, _) = hosted_index().await;
    let (status, _, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", accept)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[rstest]
#[case::missing_subtype("application")]
#[case::missing_type("/json")]
#[case::empty_subtype("application/")]
#[case::concrete_subtype_under_wildcard_type("*/json")]
#[case::unparseable_quality("application/*;q=high")]
#[case::out_of_range_quality("application/*;q=2")]
#[case::only_separators(",,")]
#[tokio::test]
async fn test_get_serves_the_index_when_every_media_range_is_malformed(#[case] accept: &str) {
    let (_dir, app, child_digest) = hosted_index().await;
    let index = amd64_index(&child_digest);
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", accept)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(&index));
    assert_eq!(body, index);
}

#[tokio::test]
async fn test_get_honors_a_list_type_named_on_a_later_accept_field_line() {
    let (_dir, app, child_digest) = hosted_index().await;
    let index = amd64_index(&child_digest);
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", IMAGE_ACCEPT), ("accept", INDEX_TYPE)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(&index));
    assert_eq!(headers[header::CONTENT_TYPE], INDEX_TYPE);
    assert_eq!(body, index);
}

#[tokio::test]
async fn test_get_combines_an_exclusion_on_a_later_accept_field_line() {
    let (_dir, app, _) = hosted_index().await;
    let (status, _, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[
            ("accept", "*/*"),
            ("accept", "application/vnd.oci.image.index.v1+json;q=0"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_get_by_digest_serves_the_amd64_child() {
    let (_dir, app, child_digest) = hosted_docker_list().await;
    let index_digest = oci_digest(&amd64_docker_list(&child_digest));
    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        &format!("/v2/store/app/manifests/{index_digest}"),
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], child_digest);
    assert_eq!(body, *DOCKER_CHILD);
}

#[tokio::test]
async fn test_head_serves_the_amd64_child_headers_with_no_body() {
    let (_dir, app, child_digest) = hosted_docker_list().await;
    let (status, headers, body) = send_with(
        &app,
        Method::HEAD,
        "/v2/store/app/manifests/multi",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], child_digest);
    assert_eq!(headers[header::CONTENT_TYPE], IMAGE_ACCEPT);
    assert_eq!(headers[header::CONTENT_LENGTH], DOCKER_CHILD.len().to_string());
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
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], child_digest);
    assert_eq!(body, *CHILD);
}

#[tokio::test]
async fn test_get_of_a_docker_list_without_an_amd64_child_returns_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let child_digest = oci_digest(&CHILD);
    push(&app, &child_digest, MANIFEST_TYPE, &CHILD).await;
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"{LIST_TYPE}","manifests":[{{"mediaType":"{IMAGE_ACCEPT}","digest":"{child_digest}","size":{},"platform":{{"os":"linux","architecture":"arm64"}}}}]}}"#,
        CHILD.len(),
    )
    .into_bytes();
    push(&app, "multi", LIST_TYPE, &index).await;
    let (status, _, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_get_of_a_docker_list_with_a_missing_amd64_child_returns_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let child_digest = format!("sha256:{}", "e".repeat(64));
    let index = amd64_docker_list(&child_digest);
    let index_digest = oci_digest(&index);
    crate::store::record_manifest(
        &state.serving.meta,
        "store",
        "app",
        &index_digest,
        &crate::store::Manifest {
            media_type: LIST_TYPE.to_owned(),
            bytes: index.clone(),
        },
    )
    .unwrap();
    crate::store::put_tag(&state.serving.meta, "store", "app", "multi", &index_digest).unwrap();
    let (status, _, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[rstest]
#[case::only_child("application/vnd.docker.distribution.manifest.v2+json;q=0")]
#[case::specific_ranges(
    "*/*, application/vnd.docker.distribution.manifest.list.v2+json;q=0, application/vnd.docker.distribution.manifest.v2+json;q=0"
)]
#[tokio::test]
async fn test_get_rejects_an_unacceptable_docker_list_child(#[case] accept: &str) {
    let (_dir, app, _) = hosted_docker_list().await;
    let (status, _, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", accept)],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "MANIFEST_INVALID"), "{body:?}");
}

#[tokio::test]
async fn test_get_rejects_a_trashed_docker_list_child() {
    let (_dir, app, child_digest) = hosted_docker_list().await;
    let (delete_status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{child_digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(delete_status, StatusCode::ACCEPTED);

    let (status, _, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_get_propagates_a_docker_list_child_fetch_error() {
    let server = MockServer::start().await;
    let child_digest = format!("sha256:{}", "e".repeat(64));
    Mock::given(method("GET"))
        .and(path("/v2/library/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(amd64_docker_list(&child_digest), LIST_TYPE))
        .mount(&server)
        .await;
    mount_head_without_digest(&server, "/v2/library/app/manifests/latest").await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/app/manifests/{child_digest}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, _, body) = send_with(
        &app,
        Method::GET,
        "/v2/hub/library/app/manifests/latest",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body_has_code(&body, "UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_push_rejects_a_docker_list_naming_a_child_from_another_repository() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/private", &auth(TOKEN)).await;
    let child_digest = oci_digest(&DOCKER_CHILD);
    let (status, body) = push_to(&app, "store/private", &child_digest, IMAGE_ACCEPT, &DOCKER_CHILD).await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");

    let (status, body) = push_to(
        &app,
        "store/app",
        "latest",
        LIST_TYPE,
        &amd64_docker_list(&child_digest),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "MANIFEST_BLOB_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_get_does_not_negotiate_a_child_held_only_by_another_repository() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let child_digest = oci_digest(&DOCKER_CHILD);
    let list = amd64_docker_list(&child_digest);
    let list_digest = oci_digest(&list);
    // Both the list and its child belong to `private`; only the tag naming the list lands in `app`,
    // which is the state a cached index reaches when it names a digest peryx already holds elsewhere.
    for (digest, media_type, bytes) in [
        (&child_digest, IMAGE_ACCEPT, DOCKER_CHILD.to_vec()),
        (&list_digest, LIST_TYPE, list),
    ] {
        let manifest = crate::store::Manifest {
            media_type: media_type.to_owned(),
            bytes,
        };
        crate::store::record_manifest(&state.serving.meta, "store", "private", digest, &manifest).unwrap();
    }
    crate::store::put_tag(&state.serving.meta, "store", "app", "latest", &list_digest).unwrap();

    let (status, _, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/latest",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_get_fetches_the_amd64_child_from_a_proxy_member() {
    let server = MockServer::start().await;
    let child_digest = oci_digest(&DOCKER_CHILD);
    let index = amd64_docker_list(&child_digest);
    for (reference, body, media_type) in [
        ("latest", index.clone(), LIST_TYPE),
        (child_digest.as_str(), DOCKER_CHILD.to_vec(), IMAGE_ACCEPT),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/v2/library/app/manifests/{reference}")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, media_type))
            .mount(&server)
            .await;
    }
    mount_head_without_digest(&server, "/v2/library/app/manifests/latest").await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, headers, body) = send_with(
        &app,
        Method::GET,
        "/v2/hub/library/app/manifests/latest",
        &[("accept", IMAGE_ACCEPT)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], child_digest);
    assert_eq!(body, *DOCKER_CHILD);
}
