use std::io::Write as _;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use peryx_core::{BrowsePage, BrowseSection};
use peryx_driver::serving::EcosystemBrowse as _;
use rstest::rstest;

use super::{
    app_with, auth, hosted_writable, oci_digest, oci_index, proxy, proxy_with_settings, send, send_body, virtual_stack,
    writer_acl,
};
use crate::{IndexSettings, LibraryPrefix, OciPlugin};

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
    let binary = [0xff];
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, "app/logo.bin", &binary[..]).unwrap();
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

async fn populated_with_layer(
    layer: &[u8],
    layer_media_type: &str,
) -> (tempfile::TempDir, Arc<peryx_driver::AppState>, axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let config_digest = upload(&app, config).await;
    let layer_digest = upload(&app, layer).await;
    let image = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{config_size}}},"layers":[{{"mediaType":"{layer_media_type}","digest":"{layer_digest}","size":{layer_size}}}]}}"#,
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

async fn populated() -> (tempfile::TempDir, Arc<peryx_driver::AppState>, axum::Router, String) {
    populated_with_layer(&gzip_layer(), "application/vnd.oci.image.layer.v1.tar+gzip").await
}

async fn browse(state: &Arc<peryx_driver::AppState>, position: usize, query: impl Into<String>) -> Option<BrowsePage> {
    state
        .driver_set()
        .get_browse(&crate::ECOSYSTEM)
        .unwrap()
        .browse(state.serving.clone(), position, query.into(), None)
        .await
        .unwrap()
}

fn section_json(section: &BrowseSection) -> serde_json::Value {
    serde_json::to_value(section).unwrap()
}

fn link_labels(section: &serde_json::Value) -> Vec<&str> {
    section["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["label"].as_str().unwrap())
        .collect()
}

async fn browse_response(
    state: &Arc<peryx_driver::AppState>,
    uri: &str,
    authorization: Option<&str>,
) -> (StatusCode, bytes::Bytes) {
    let mut request = Request::builder().uri(uri).header("host", "registry.test:5000");
    if let Some(authorization) = authorization {
        request = request.header("authorization", authorization);
    }
    let response = OciPlugin
        .dispatch(state.clone(), request.body(Body::empty()).unwrap())
        .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4 << 20).await.unwrap();
    (status, body)
}

#[tokio::test]
async fn test_browse_index_lists_stored_repositories() {
    let (_dir, state, _app, _layer) = populated().await;
    let page = browse(&state, 0, "").await.unwrap();
    let section = section_json(&page.sections[0]);
    assert_eq!(
        (
            page.actions,
            section["heading"].as_str().unwrap(),
            link_labels(&section),
        ),
        (Vec::new(), "Repositories", vec!["app"]),
    );
}

#[tokio::test]
async fn test_browse_repository_lists_tags() {
    let (_dir, state, _app, _layer) = populated().await;
    let page = browse(&state, 0, "project=app").await.unwrap();
    let section = section_json(&page.sections[0]);
    assert_eq!(
        (
            page.title,
            page.actions,
            section["heading"].as_str().unwrap(),
            link_labels(&section),
        ),
        ("app".to_owned(), Vec::new(), "Tags", vec!["1.0", "multi"]),
    );
}

#[tokio::test]
async fn test_browse_repository_hides_trashed_tag() {
    let (_dir, state, app, _layer) = populated().await;
    let (status, ..) = send_body(
        &app,
        Method::DELETE,
        "/v2/store/app/manifests/1.0",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let page = browse(&state, 0, "project=app").await.unwrap();
    assert_eq!(link_labels(&section_json(&page.sections[0])), vec!["multi"]);
}

#[tokio::test]
async fn test_browse_repository_on_root_route_uses_bare_name() {
    let dir = tempfile::tempdir().unwrap();
    let index = oci_index("root", "", peryx_index::IndexKind::Hosted { volatile: false });
    let (state, _app) = super::app_with(&dir, index);
    let digest = format!("sha256:{}", "a".repeat(64));
    crate::store::put_tag(&state.serving.meta, "root", "library/nginx", "1.0", &digest).unwrap();
    let page = browse(&state, 0, "project=library%2Fnginx").await.unwrap();
    assert_eq!(link_labels(&section_json(&page.sections[0])), vec!["1.0"]);
}

#[tokio::test]
async fn test_browse_manifest_lists_layer() {
    let (_dir, state, _app, layer_digest) = populated().await;
    let page = browse(&state, 0, "project=app&ref=1.0").await.unwrap();
    let section = section_json(&page.sections[1]);
    assert_eq!(
        (
            page.title,
            page.actions,
            section["heading"].as_str().unwrap(),
            section["columns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect::<Vec<_>>(),
            section["rows"][0]["cells"][0]["text"].as_str().unwrap(),
            !section["rows"][0]["cells"][0]["href"].is_null(),
        ),
        (
            "app:1.0".to_owned(),
            Vec::new(),
            "Layers",
            vec!["Digest", "Size", "Media type", "Contents"],
            layer_digest.as_str(),
            true
        ),
    );
}

#[tokio::test]
async fn test_browse_zstd_layer_is_not_linked_or_parsed() {
    let layer = [0x28, 0xb5, 0x2f, 0xfd, 0x20, 0, 0x15, 0, 0, 0, 0];
    let (_dir, state, _app, layer_digest) =
        populated_with_layer(&layer, "application/vnd.oci.image.layer.v1.tar+zstd").await;
    let page = browse(&state, 0, "project=app&ref=1.0").await.unwrap();
    let section = section_json(&page.sections[1]);
    let error = state
        .driver_set()
        .get_browse(&crate::ECOSYSTEM)
        .unwrap()
        .browse(
            state.serving.clone(),
            0,
            format!("project=app&ref=1.0&layer={layer_digest}"),
            None,
        )
        .await
        .unwrap_err();

    assert_eq!(
        (
            section["rows"][0]["cells"][0]["href"].is_null(),
            section["rows"][0]["cells"][3]["href"].is_null(),
            section["rows"][0]["cells"][3]["text"].as_str(),
            error,
        ),
        (
            true,
            true,
            Some(""),
            format!(
                "layer contents for {layer_digest} on \"store/app\": 422 Unprocessable Entity: unsupported archive type"
            ),
        ),
    );
}

#[tokio::test]
async fn test_browse_index_manifest_lists_platform() {
    let (_dir, state, _app, _layer) = populated().await;
    let page = browse(&state, 0, "project=app&ref=multi").await.unwrap();
    let section = section_json(&page.sections[1]);
    assert_eq!(
        (
            page.actions,
            section["heading"].as_str().unwrap(),
            section["rows"][0]["cells"][1]["text"].as_str().unwrap(),
        ),
        (Vec::new(), "Platform manifests", "linux/amd64")
    );
}

#[tokio::test]
async fn test_browse_invalid_reference_is_absent() {
    let (_dir, state, _app, _layer) = populated().await;
    assert!(browse(&state, 0, "project=app&ref=not+a+ref%21").await.is_none());
}

#[tokio::test]
async fn test_browse_unknown_tag_is_absent() {
    let (_dir, state, _app, _layer) = populated().await;
    assert!(browse(&state, 0, "project=app&ref=9.9").await.is_none());
}

#[tokio::test]
async fn test_browse_layer_lists_members() {
    let (_dir, state, _app, layer_digest) = populated().await;
    let page = browse(&state, 0, format!("project=app&ref=1.0&layer={layer_digest}"))
        .await
        .unwrap();
    let section = section_json(&page.sections[0]);
    assert_eq!(
        (
            page.title,
            page.actions,
            section["heading"].as_str().unwrap(),
            section["rows"][0]["cells"][0]["text"].as_str().unwrap(),
        ),
        ("Layer contents".to_owned(), Vec::new(), "Members", "app/config.toml"),
    );
}

#[tokio::test]
async fn test_browse_absent_layer_reports_error() {
    let (_dir, state, _app, _layer) = populated().await;
    let absent = oci_digest(b"never uploaded");
    let error = state
        .driver_set()
        .get_browse(&crate::ECOSYSTEM)
        .unwrap()
        .browse(
            state.serving.clone(),
            0,
            format!("project=app&ref=1.0&layer={absent}"),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.contains("layer contents"), "{error}");
}

#[tokio::test]
async fn test_browse_member_previews_text() {
    let (_dir, state, _app, layer_digest) = populated().await;
    let page = browse(
        &state,
        0,
        format!("project=app&ref=1.0&layer={layer_digest}&member=app%2Fconfig.toml&offset=0"),
    )
    .await
    .unwrap();
    let section = section_json(&page.sections[0]);
    assert_eq!(
        (
            page.title.as_str(),
            page.actions,
            section["heading"].as_str().unwrap(),
            section["text"].as_str().unwrap(),
            section["offset"].as_u64().unwrap(),
        ),
        ("app/config.toml", Vec::new(), "Preview", "name = \"peryx\"\n", 0),
    );
}

#[tokio::test]
async fn test_browse_member_of_absent_layer_reports_error() {
    let (_dir, state, _app, _layer) = populated().await;
    let absent = oci_digest(b"never uploaded");
    let error = state
        .driver_set()
        .get_browse(&crate::ECOSYSTEM)
        .unwrap()
        .browse(
            state.serving.clone(),
            0,
            format!("project=app&ref=1.0&layer={absent}&member=app%2Fconfig.toml&offset=0"),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.contains("layer contents"), "{error}");
}

#[tokio::test]
async fn test_browse_repository_unions_proxy_tags() {
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
    let page = browse(&state, 0, "project=library%2Fnginx").await.unwrap();
    assert_eq!(link_labels(&section_json(&page.sections[0])), vec!["1.25", "latest"]);
}

#[rstest]
#[case::always(LibraryPrefix::Always, "library/")]
#[case::never(LibraryPrefix::Never, "")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_registered_browse_uses_compiled_library_prefix(
    #[case] library_prefix: LibraryPrefix,
    #[case] upstream_prefix: &str,
) {
    let server = wiremock::MockServer::start().await;
    let protocol_request = observe_tag_list(&server, "protocol").await;
    let browse_request = observe_tag_list(&server, "browse").await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy_with_settings(&dir, &format!("{}/", server.uri()), IndexSettings { library_prefix });
    let protocol = tokio::spawn(async move { send(&app, Method::GET, "/v2/hub/protocol/tags/list").await });
    let protocol_path = protocol_request.await.unwrap();
    let browse = tokio::spawn({
        let driver = state.driver_set().get_browse(&crate::ECOSYSTEM).unwrap().clone();
        let serving = state.serving.clone();
        async move { driver.browse(serving, 0, "project=browse".to_owned(), None).await }
    });
    let browse_path = browse_request.await.unwrap();

    assert_eq!(
        (protocol_path, browse_path),
        (
            format!("/v2/{upstream_prefix}protocol/tags/list"),
            format!("/v2/{upstream_prefix}browse/tags/list"),
        )
    );
    assert_eq!(protocol.await.unwrap().0, StatusCode::OK);
    assert!(browse.await.unwrap().unwrap().is_some());
}

async fn observe_tag_list(server: &wiremock::MockServer, repository: &str) -> tokio::sync::oneshot::Receiver<String> {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, ResponseTemplate};

    let (sender, receiver) = tokio::sync::oneshot::channel();
    let sender = Mutex::new(Some(sender));
    let response = ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "name": repository,
        "tags": [],
    }));
    Mock::given(method("GET"))
        .and(path_regex(format!(r"^/v2/(library/)?{repository}/tags/list$")))
        .respond_with(move |request: &wiremock::Request| {
            sender
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(request.url.path().to_owned())
                .unwrap();
            response.clone()
        })
        .mount(server)
        .await;
    receiver
}

#[tokio::test]
async fn test_browse_manifest_of_unreachable_proxy_is_absent() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, "http://127.0.0.1:1/", false);
    assert!(browse(&state, 0, "project=library%2Fnginx&ref=1.0").await.is_none());
}

#[rstest]
#[case::layer("")]
#[case::member("&member=f&offset=0")]
#[tokio::test]
async fn test_browse_content_of_unreachable_proxy_reports_error(#[case] suffix: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let digest = oci_digest(b"whatever");
    assert!(
        state
            .driver_set()
            .get_browse(&crate::ECOSYSTEM)
            .unwrap()
            .browse(
                state.serving.clone(),
                0,
                format!("project=library%2Fnginx&ref=1.0&layer={digest}{suffix}"),
                None,
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn test_browse_virtual_index_lists_member_repositories() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = virtual_stack(&dir, "http://127.0.0.1:1/");
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
    let page = browse(&state, 2, "").await.unwrap();
    let section = section_json(&page.sections[0]);
    assert_eq!(
        (
            page.actions,
            section["heading"].as_str().unwrap(),
            link_labels(&section),
        ),
        (Vec::new(), "Repositories", vec!["team/app"]),
    );
}

#[rstest::rstest]
#[case::missing_index("/+ui/projects", StatusCode::BAD_REQUEST)]
#[case::unknown_index("/+ui/projects?index=missing", StatusCode::NOT_FOUND)]
#[case::invalid_offset("/+ui/projects?index=store&offset=bad", StatusCode::BAD_REQUEST)]
#[case::unknown_path("/+ui/unknown?index=store", StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_plugin_browse_validates_requests(#[case] uri: &str, #[case] expected: StatusCode) {
    let dir = tempfile::tempdir().unwrap();
    let (state, _app) = hosted_writable(&dir, TOKEN);

    assert_eq!(browse_response(&state, uri, None).await.0, expected);
}

#[tokio::test]
async fn test_plugin_browse_enforces_read_access() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = oci_index("store", "store", peryx_index::IndexKind::Hosted { volatile: false });
    index.acl = writer_acl(TOKEN);
    index.acl.anonymous_read = false;
    let (state, _app) = app_with(&dir, index);

    assert_eq!(
        (
            browse_response(&state, "/+ui/projects?index=store", None).await.0,
            browse_response(&state, "/+ui/projects?index=store", Some(&auth(TOKEN)))
                .await
                .0,
        ),
        (StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN)
    );
}

#[tokio::test]
async fn test_plugin_browse_serves_each_resource() {
    let (_dir, state, _app, layer_digest) = populated().await;
    let (browse_status, browse_body) =
        browse_response(&state, "/+ui/browse?index=store&project=app&ref=1.0", None).await;
    let (repositories_status, repositories_body) = browse_response(&state, "/+ui/projects?index=store", None).await;
    let (references_status, references_body) =
        browse_response(&state, "/+ui/project?index=store&project=app", None).await;
    let (manifest_status, manifest) =
        browse_response(&state, "/+ui/manifest?index=store&project=app&ref=1.0", None).await;
    let (listing_status, listing_body) = browse_response(
        &state,
        &format!("/+ui/members?index=store&project=app&layer={layer_digest}"),
        None,
    )
    .await;
    let (preview_status, preview_body) = browse_response(
        &state,
        &format!("/+ui/member?index=store&project=app&layer={layer_digest}&member=app%2Fconfig.toml&offset=0"),
        None,
    )
    .await;

    assert_eq!(
        (
            browse_status,
            repositories_status,
            references_status,
            manifest_status,
            listing_status,
            preview_status,
        ),
        (
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::OK
        )
    );
    let browse: serde_json::Value = serde_json::from_slice(&browse_body).unwrap();
    assert_eq!(
        (browse["title"].as_str(), browse["command"].as_str()),
        (Some("app:1.0"), Some("docker pull registry.test:5000/store/app:1.0"))
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&repositories_body).unwrap(),
        serde_json::json!(["app"])
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&references_body).unwrap()["names"],
        serde_json::json!(["1.0", "multi"])
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&manifest).unwrap()["is_index"],
        false
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&listing_body).unwrap()[0]["path"],
        "app/config.toml"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&preview_body).unwrap()["text"],
        "name = \"peryx\"\n"
    );
}

#[tokio::test]
async fn test_plugin_browse_returns_not_found_for_a_missing_manifest() {
    let (_dir, state, _app, _layer_digest) = populated().await;

    assert_eq!(
        browse_response(&state, "/+ui/browse?index=store&project=app&ref=missing", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[rstest::rstest]
#[case::project("/+ui/project?index=store", StatusCode::BAD_REQUEST)]
#[case::manifest_missing_query("/+ui/manifest?index=store&project=app", StatusCode::BAD_REQUEST)]
#[case::manifest_absent("/+ui/manifest?index=store&project=app&ref=absent", StatusCode::NOT_FOUND)]
#[case::members_missing_query("/+ui/members?index=store&project=app", StatusCode::BAD_REQUEST)]
#[case::member_missing_query("/+ui/member?index=store&project=app", StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn test_plugin_browse_reports_missing_resources(#[case] uri: &str, #[case] expected: StatusCode) {
    let (_dir, state, _app, _layer) = populated().await;

    assert_eq!(browse_response(&state, uri, None).await.0, expected);
}

#[tokio::test]
async fn test_plugin_browse_reports_layer_and_member_failures() {
    let (_dir, state, _app, layer_digest) = populated().await;
    let absent = oci_digest(b"absent");
    let member = format!("/+ui/member?index=store&project=app&layer={layer_digest}&member=app%2Flogo.bin&offset=0");

    assert_eq!(
        (
            browse_response(
                &state,
                &format!("/+ui/members?index=store&project=app&layer={absent}"),
                None,
            )
            .await
            .0,
            browse_response(&state, &member, None).await.0,
        ),
        (StatusCode::INTERNAL_SERVER_ERROR, StatusCode::INTERNAL_SERVER_ERROR)
    );
}

#[tokio::test]
async fn test_browse_manifest_on_root_route_uses_bare_repository_name() {
    let dir = tempfile::tempdir().unwrap();
    let index = oci_index("root", "", peryx_index::IndexKind::Hosted { volatile: false });
    let (state, _app) = super::app_with(&dir, index);
    assert!(browse(&state, 0, "project=library%2Fnginx&ref=1.0").await.is_none());
}
