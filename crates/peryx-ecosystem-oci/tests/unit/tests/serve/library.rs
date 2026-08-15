use super::support::*;
use crate::{IndexSettings, LibraryPrefix};
use wiremock::matchers::query_param;

fn settings(library_prefix: LibraryPrefix) -> IndexSettings {
    IndexSettings { library_prefix }
}

#[tokio::test]
async fn test_library_prefix_rewrites_the_upstream_path_and_token_scope() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    Mock::given(method("GET"))
        .and(path("/v2/library/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"Bearer realm="{}/token",service="reg""#, server.uri()).as_str(),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Docker Hub authorizes single-segment names under `library/`.
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param("scope", "repository:library/app:pull"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"abc"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/app/manifests/latest"))
        .and(match_header("authorization", "Bearer abc"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy_with_settings(&dir, &format!("{}/", server.uri()), settings(LibraryPrefix::Always));
    let (status, headers, got) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(body));
    assert_eq!(got, &body[..]);
    // The `library/` rewrite must not alter the client-facing repository name.
    assert_eq!(
        store::get_tag(&state.serving.meta, "hub", "app", "latest").unwrap(),
        Some(oci_digest(body))
    );
    assert_eq!(
        store::get_tag(&state.serving.meta, "hub", "library/app", "latest").unwrap(),
        None
    );
}

#[tokio::test]
async fn test_library_prefix_rewrites_a_blob_pull() {
    let server = MockServer::start().await;
    let layer = b"layer bytes";
    let digest = oci_digest(layer);
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/app/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(layer.to_vec(), "application/octet-stream"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy_with_settings(&dir, &format!("{}/", server.uri()), settings(LibraryPrefix::Always));
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/hub/app/blobs/{digest}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &layer[..]);
}

#[rstest]
#[case::multi_segment_under_always(LibraryPrefix::Always, "acme/app")]
#[case::single_segment_under_auto_on_a_non_hub_upstream(LibraryPrefix::Auto, "app")]
#[case::single_segment_under_never(LibraryPrefix::Never, "app")]
#[tokio::test]
async fn test_upstream_sees_the_client_name_unrewritten(#[case] prefix: LibraryPrefix, #[case] repo: &str) {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/latest")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy_with_settings(&dir, &format!("{}/", server.uri()), settings(prefix));
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/hub/{repo}/manifests/latest")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);
}
