//! The neutral browse views the driver produces for the web UI: an index's repositories, a
//! repository's tags, a manifest view, and a layer's members and text chunks — plus the absent and
//! error branches each surfaces.

use std::io::Write as _;
use std::sync::Arc;

use axum::http::{Method, StatusCode};
use peryx_core::{Ecosystem, UiProjectView};
use peryx_driver::serving::EcosystemDriver;
use peryx_driver::state::ServingState;

use super::{auth, hosted_writable, oci_digest, oci_index, proxy, send_body, virtual_stack};

const TOKEN: &str = "s3cret";

fn tar_layer() -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut builder = tar::Builder::new(&mut bytes);
    let content = b"name = \"peryx\"\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, "app/config.toml", &content[..])
        .unwrap();
    builder.into_inner().unwrap();
    bytes
}

fn gzip_layer() -> Vec<u8> {
    let mut gz = Vec::new();
    let mut encoder = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
    encoder.write_all(&tar_layer()).unwrap();
    encoder.finish().unwrap();
    gz
}

async fn upload(app: &axum::Router, blob: &[u8]) -> String {
    let digest = oci_digest(blob);
    let (status, _, _) = send_body(
        app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    digest
}

async fn put_manifest(app: &axum::Router, reference: &str, media_type: &str, body: &[u8]) {
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

fn oci_driver(state: &Arc<peryx_driver::AppState>) -> (Arc<dyn EcosystemDriver>, Arc<ServingState>) {
    let driver = state.driver_for(Ecosystem::Oci).unwrap().clone();
    (driver, state.serving.clone())
}

/// A hosted store carrying repository `app` with an image manifest at `1.0` (a config blob and one
/// gzip layer) and an image index at `multi`.
async fn populated() -> (tempfile::TempDir, Arc<peryx_driver::AppState>, axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let config_digest = upload(&app, config).await;
    let layer = gzip_layer();
    let layer_digest = upload(&app, &layer).await;
    let image = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{config_size}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{layer_digest}","size":{layer_size}}}]}}"#,
        config_size = config.len(),
        layer_size = layer.len(),
    );
    put_manifest(
        &app,
        "1.0",
        "application/vnd.oci.image.manifest.v1+json",
        image.as_bytes(),
    )
    .await;
    let image_digest = oci_digest(image.as_bytes());
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{image_digest}","size":{size},"platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
        size = image.len(),
    );
    put_manifest(
        &app,
        "multi",
        "application/vnd.oci.image.index.v1+json",
        index.as_bytes(),
    )
    .await;
    (dir, state, app, layer_digest)
}

#[tokio::test]
async fn test_project_names_lists_the_stored_repositories() {
    let (_dir, state, _app, _layer) = populated().await;
    let (driver, serving) = oci_driver(&state);
    assert_eq!(driver.project_names(&serving, 0).unwrap(), vec!["app".to_owned()]);
}

#[tokio::test]
async fn test_browse_project_lists_a_repository_tags() {
    let (_dir, state, _app, _layer) = populated().await;
    let (driver, serving) = oci_driver(&state);
    let view = driver
        .browse_project(serving, 0, "app".to_owned())
        .await
        .unwrap()
        .unwrap();
    match view {
        UiProjectView::References { names } => assert_eq!(names, vec!["1.0".to_owned(), "multi".to_owned()]),
        other => panic!("expected a reference listing, got {other:?}"),
    }
}

#[tokio::test]
async fn test_browse_project_on_a_root_route_index_uses_the_bare_repository_name() {
    let dir = tempfile::tempdir().unwrap();
    let index = oci_index("root", "", peryx_index::IndexKind::Hosted { volatile: false });
    let (state, _app) = super::app_with(&dir, index);
    let digest = format!("sha256:{}", "a".repeat(64));
    crate::store::put_tag(&state.meta, "root", "library/nginx", "1.0", &digest).unwrap();
    let (driver, serving) = oci_driver(&state);
    // With an empty index route the browse name is the bare repository, not a route-prefixed one.
    let view = driver
        .browse_project(serving, 0, "library/nginx".to_owned())
        .await
        .unwrap()
        .unwrap();
    match view {
        UiProjectView::References { names } => assert_eq!(names, vec!["1.0".to_owned()]),
        other => panic!("expected a reference listing, got {other:?}"),
    }
}

#[tokio::test]
async fn test_manifest_view_reads_an_image_manifest() {
    let (_dir, state, _app, layer_digest) = populated().await;
    let (driver, serving) = oci_driver(&state);
    let manifest = driver
        .manifest_view(serving, 0, "app".to_owned(), "1.0".to_owned())
        .await
        .unwrap()
        .unwrap();
    assert!(!manifest.is_index);
    assert!(manifest.config.is_some());
    assert_eq!(manifest.entries.len(), 1);
    assert_eq!(manifest.entries[0].digest, layer_digest);
    assert!(manifest.entries[0].browsable);
}

#[tokio::test]
async fn test_manifest_view_reads_an_image_index_with_platforms() {
    let (_dir, state, _app, _layer) = populated().await;
    let (driver, serving) = oci_driver(&state);
    let manifest = driver
        .manifest_view(serving, 0, "app".to_owned(), "multi".to_owned())
        .await
        .unwrap()
        .unwrap();
    assert!(manifest.is_index);
    assert!(manifest.config.is_none());
    assert_eq!(manifest.entries.len(), 1);
    assert_eq!(manifest.entries[0].platform.as_deref(), Some("linux/amd64"));
}

#[tokio::test]
async fn test_manifest_view_of_an_invalid_reference_is_absent() {
    let (_dir, state, _app, _layer) = populated().await;
    let (driver, serving) = oci_driver(&state);
    assert!(
        driver
            .manifest_view(serving, 0, "app".to_owned(), "not a ref!".to_owned())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_manifest_view_of_an_unknown_tag_is_absent() {
    let (_dir, state, _app, _layer) = populated().await;
    let (driver, serving) = oci_driver(&state);
    assert!(
        driver
            .manifest_view(serving, 0, "app".to_owned(), "9.9".to_owned())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_artifact_members_lists_a_layer() {
    let (_dir, state, _app, layer_digest) = populated().await;
    let (driver, serving) = oci_driver(&state);
    let members = driver
        .artifact_members(serving, 0, "app".to_owned(), layer_digest)
        .await
        .unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].path, "app/config.toml");
}

#[tokio::test]
async fn test_artifact_members_of_an_absent_layer_reports_an_error() {
    let (_dir, state, _app, _layer) = populated().await;
    let (driver, serving) = oci_driver(&state);
    let absent = oci_digest(b"never uploaded");
    let error = driver
        .artifact_members(serving, 0, "app".to_owned(), absent)
        .await
        .unwrap_err();
    assert!(error.contains("layer contents"), "{error}");
}

#[tokio::test]
async fn test_artifact_member_chunk_previews_a_text_member() {
    let (_dir, state, _app, layer_digest) = populated().await;
    let (driver, serving) = oci_driver(&state);
    let chunk = driver
        .artifact_member_chunk(
            serving,
            0,
            "app".to_owned(),
            layer_digest,
            "app/config.toml".to_owned(),
            0,
        )
        .await
        .unwrap();
    assert_eq!(chunk.text, "name = \"peryx\"\n");
    assert_eq!(chunk.offset, 0);
}

#[tokio::test]
async fn test_artifact_member_chunk_of_an_absent_layer_reports_an_error() {
    let (_dir, state, _app, _layer) = populated().await;
    let (driver, serving) = oci_driver(&state);
    let absent = oci_digest(b"never uploaded");
    let error = driver
        .artifact_member_chunk(serving, 0, "app".to_owned(), absent, "app/config.toml".to_owned(), 0)
        .await
        .unwrap_err();
    assert!(error.contains("layer contents"), "{error}");
}

#[tokio::test]
async fn test_browse_project_unions_tags_from_a_proxy_member() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
    let (state, _app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (driver, serving) = oci_driver(&state);

    let view = driver
        .browse_project(serving, 0, "library/nginx".to_owned())
        .await
        .unwrap()
        .unwrap();
    match view {
        UiProjectView::References { names } => assert_eq!(names, vec!["1.25".to_owned(), "latest".to_owned()]),
        other => panic!("expected a reference listing, got {other:?}"),
    }
}

#[tokio::test]
async fn test_manifest_view_of_an_unreachable_proxy_is_absent() {
    // A proxy that cannot reach its upstream answers the manifest read with a non-OK response, which
    // the view surfaces as an absent manifest rather than a hard error.
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (driver, serving) = oci_driver(&state);
    assert!(
        driver
            .manifest_view(serving, 0, "library/nginx".to_owned(), "1.0".to_owned())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_artifact_members_of_an_unreachable_proxy_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (driver, serving) = oci_driver(&state);
    let digest = oci_digest(b"whatever");
    assert!(
        driver
            .artifact_members(serving, 0, "library/nginx".to_owned(), digest)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn test_artifact_member_chunk_of_an_unreachable_proxy_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (driver, serving) = oci_driver(&state);
    let digest = oci_digest(b"whatever");
    assert!(
        driver
            .artifact_member_chunk(serving, 0, "library/nginx".to_owned(), digest, "f".to_owned(), 0)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn test_project_names_of_a_virtual_index_walks_its_members() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = virtual_stack(&dir, "http://127.0.0.1:1/");
    // Push a manifest through the virtual index (`reg`), which routes the write to its hosted member.
    let (status, ..) = send_body(
        &app,
        Method::PUT,
        "/v2/reg/team/app/manifests/1.0",
        &[
            ("authorization", &auth("s3cret")),
            ("content-type", "application/vnd.oci.image.manifest.v1+json"),
        ],
        br#"{"schemaVersion":2}"#.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let driver = state.driver_for(Ecosystem::Oci).unwrap().clone();
    // The virtual index `reg` is the third configured index; its repositories union its members'.
    let names = driver.project_names(&state.serving, 2).unwrap();
    assert_eq!(names, vec!["team/app".to_owned()]);
}

#[tokio::test]
async fn test_manifest_view_on_a_root_route_index_uses_the_bare_repository_name() {
    let dir = tempfile::tempdir().unwrap();
    let index = oci_index("root", "", peryx_index::IndexKind::Hosted { volatile: false });
    let (state, _app) = super::app_with(&dir, index);
    let (driver, serving) = oci_driver(&state);
    // With an empty index route the full name is the bare repository; an unknown reference is absent.
    assert!(
        driver
            .manifest_view(serving, 0, "library/nginx".to_owned(), "1.0".to_owned())
            .await
            .unwrap()
            .is_none()
    );
}
