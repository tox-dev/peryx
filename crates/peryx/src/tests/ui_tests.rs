use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write as _;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use http_body_util::BodyExt as _;
use peryx_core::path::local_file_url;
use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_ecosystem_pypi::upload::Uploaded;
use peryx_ecosystem_pypi::{CoreMetadata, File, Provenance, Yanked, to_json};
use peryx_identity::{Action, Glob, Grant, Principal, Signer};
use peryx_storage::blob::Digest;
use rstest::{fixture, rstest};
use sha2::{Digest as _, Sha256};
use tower::ServiceExt as _;

use crate::config::{Config, IndexConfig, IndexKind, SecretSource, TokenConfig};
use crate::server::{build_router, build_state, router_for};

#[fixture]
fn ui_router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    (dir, router)
}

#[fixture]
fn filter_router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir)).unwrap();
    put_filter_files(&state);
    (dir, router_for(state))
}

#[fixture]
fn private_oci_ui_router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&private_oci_ui_config(&dir)).unwrap();
    (dir, router)
}

fn ui_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![
            IndexConfig {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                policy: peryx_policy::PolicyConfig::default(),
                ecosystem_policy: toml::Table::new(),
                ecosystem_settings: toml::Table::new(),
                webhooks: Vec::new(),
                ecosystem: peryx_core::Ecosystem::Pypi,
                anonymous_read: None,
                tokens: Vec::new(),
                kind: IndexKind::Cached {
                    upstream: "http://127.0.0.1:9/simple/".to_owned(),
                    username: None,
                    password: None,
                    token: None,
                    upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
                    offline: false,
                    prefetch: Box::default(),
                },
            },
            IndexConfig {
                name: "hosted".to_owned(),
                route: "hosted".to_owned(),
                policy: peryx_policy::PolicyConfig::default(),
                ecosystem_policy: toml::Table::new(),
                ecosystem_settings: toml::Table::new(),
                webhooks: Vec::new(),
                ecosystem: peryx_core::Ecosystem::Pypi,
                anonymous_read: None,
                tokens: Vec::new(),
                kind: IndexKind::Hosted {
                    upload_token: Some(SecretSource::Literal("s3cret".to_owned())),
                    volatile: true,
                },
            },
            IndexConfig {
                name: "root/pypi".to_owned(),
                route: "root/pypi".to_owned(),
                policy: peryx_policy::PolicyConfig::default(),
                ecosystem_policy: toml::Table::new(),
                ecosystem_settings: toml::Table::new(),
                webhooks: Vec::new(),
                ecosystem: peryx_core::Ecosystem::Pypi,
                anonymous_read: None,
                tokens: Vec::new(),
                kind: IndexKind::Virtual {
                    layers: vec!["hosted".to_owned(), "pypi".to_owned()],
                    upload: Some("hosted".to_owned()),
                },
            },
        ],
        ..Config::default()
    }
}

fn empty_ui_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: Vec::new(),
        ..Config::default()
    }
}

fn oci_ui_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![IndexConfig {
            name: "images".to_owned(),
            route: "images".to_owned(),
            policy: peryx_policy::PolicyConfig::default(),
            ecosystem_policy: toml::Table::new(),
            ecosystem_settings: toml::Table::new(),
            webhooks: Vec::new(),
            ecosystem: peryx_core::Ecosystem::Oci,
            anonymous_read: None,
            tokens: Vec::new(),
            kind: IndexKind::Hosted {
                upload_token: Some(SecretSource::Literal("s3cret".to_owned())),
                volatile: true,
            },
        }],
        ..Config::default()
    }
}

/// Push a tagged image manifest through the `/v2/` API so the browse pages have an OCI repository.
/// Upload a blob to the hosted `images/app` repo and return its OCI digest.
async fn upload_blob(router: &axum::Router, bytes: &[u8]) -> String {
    let digest = format!("sha256:{}", Digest::of(bytes).as_str());
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v2/images/app/blobs/uploads/?digest={digest}"))
                .header(header::AUTHORIZATION, format!("Basic {}", STANDARD.encode("_:s3cret")))
                .body(Body::from(bytes.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    digest
}

/// Upload a config and a layer blob, then push a manifest referencing them, returning both digests.
async fn push_oci_image(router: &axum::Router) -> (String, String) {
    let config = upload_blob(router, b"{}").await;
    let layer = upload_blob(router, b"layer-bytes").await;
    let manifest = format!(
        concat!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","#,
            r#""config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config}","size":2}},"#,
            r#""layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{layer}","size":11}}]}}"#,
        ),
        config = config,
        layer = layer,
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v2/images/app/manifests/1.0")
                .header(header::AUTHORIZATION, format!("Basic {}", STANDARD.encode("_:s3cret")))
                .header(header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")
                .body(Body::from(manifest))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    (config, layer)
}

/// A gzip-tar layer carrying one text and one binary member, plus its OCI digest.
fn oci_layer() -> (Vec<u8>, String) {
    let mut tar_bytes = Vec::new();
    let mut builder = tar::Builder::new(&mut tar_bytes);
    for (path, bytes) in [
        ("etc/app.conf", b"debug = true\n".as_slice()),
        ("bin/app", &[0x7f, 0x45, 0x4c, 0x46]),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, bytes).unwrap();
    }
    builder.into_inner().unwrap();
    let mut gz = Vec::new();
    let mut encoder = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
    encoder.write_all(&tar_bytes).unwrap();
    encoder.finish().unwrap();
    let digest = format!("sha256:{}", Digest::of(&gz).as_str());
    (gz, digest)
}

/// Upload a real gzip layer blob and push a manifest referencing it, so the layer browser has a
/// stored layer to open.
async fn push_oci_image_with_layer(router: &axum::Router) -> String {
    let (layer, digest) = oci_layer();
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v2/images/app/blobs/uploads/?digest={digest}"))
                .header(header::AUTHORIZATION, format!("Basic {}", STANDARD.encode("_:s3cret")))
                .body(Body::from(layer))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let config = upload_blob(router, b"{}").await;
    let manifest = format!(
        concat!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","#,
            r#""config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config}","size":2}},"#,
            r#""layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{digest}","size":42}}]}}"#,
        ),
        config = config,
        digest = digest,
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v2/images/app/manifests/1.0")
                .header(header::AUTHORIZATION, format!("Basic {}", STANDARD.encode("_:s3cret")))
                .header(header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")
                .body(Body::from(manifest))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    digest
}

#[tokio::test]
async fn test_ui_oci_manifest_links_layer_contents() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&oci_ui_config(&dir)).unwrap();
    let digest = push_oci_image_with_layer(&router).await;

    let (status, body) = get(&router, "/browse?index=images&project=app&ref=1.0").await;
    assert_eq!(status, StatusCode::OK);
    // The layer row carries a contents link into the layer browser, keyed on the layer digest (its
    // colon percent-encoded in the query).
    assert!(body.contains("class=\"inspect\""), "contents link missing: {body}");
    let hex = digest.strip_prefix("sha256:").unwrap();
    assert!(
        body.contains(&format!("layer=sha256%3A{hex}")),
        "layer link missing: {body}"
    );
}

#[tokio::test]
async fn test_ui_oci_layer_lists_and_previews_members() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&oci_ui_config(&dir)).unwrap();
    let digest = push_oci_image_with_layer(&router).await;

    let listing = format!("/browse?index=images&project=app&ref=1.0&layer={digest}");
    let (status, body) = get(&router, &listing).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("etc/app.conf"), "text member missing: {body}");
    assert!(body.contains("bin/app"), "binary member missing: {body}");

    let member = format!("{listing}&member=etc%2Fapp.conf");
    let (status, body) = get(&router, &member).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("debug = true"), "member preview missing: {body}");
}

#[tokio::test]
async fn test_ui_oci_repository_lists_its_tags() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&oci_ui_config(&dir)).unwrap();
    push_oci_image(&router).await;

    let (status, body) = get(&router, "/browse?index=images&project=app").await;
    assert_eq!(status, StatusCode::OK);
    // The repository page shows the pushed tag, linking to its manifest.
    assert!(body.contains("1.0"), "tag missing: {body}");
    assert!(body.contains("ref=1.0"), "manifest link missing: {body}");
}

#[tokio::test]
async fn test_ui_oci_manifest_shows_config_and_layers() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&oci_ui_config(&dir)).unwrap();
    let (config, layer) = push_oci_image(&router).await;

    let (status, body) = get(&router, "/browse?index=images&project=app&ref=1.0").await;
    assert_eq!(status, StatusCode::OK);
    // The manifest page shows the config and layer blob digests.
    assert!(body.contains(&config), "config blob missing: {body}");
    assert!(body.contains(&layer), "layer blob missing: {body}");
    assert!(body.contains("Layers"), "layer heading missing: {body}");
}

#[rstest]
#[case::anonymous(String::new(), false)]
#[case::reader(reader_authorization(), true)]
#[tokio::test]
async fn test_ui_private_oci_repository_rendering_follows_read_acl(
    #[case] authorization: String,
    #[case] expected: bool,
    private_oci_ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = private_oci_ui_router;
    push_oci_image(&router).await;

    let (status, body) = get_authorized(&router, "/browse?index=images&project=app", &authorization).await;
    assert_eq!((status, body.contains("ref=1.0")), (StatusCode::OK, expected), "{body}");
}

#[rstest]
#[case::projects("/+ui/projects?index=images")]
#[case::project("/+ui/project?index=images&project=app")]
#[case::manifest("/+ui/manifest?index=images&project=app&ref=1.0")]
#[case::members("/+ui/members?index=images&project=app&digest=sha256:a")]
#[case::member("/+ui/member?index=images&project=app&digest=sha256:a&member=f")]
#[tokio::test]
async fn test_ui_private_oci_data_routes_reject_anonymous_reads(
    #[case] uri: &str,
    private_oci_ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = private_oci_ui_router;

    let (status, _) = get(&router, uri).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[rstest]
#[tokio::test]
async fn test_ui_private_oci_data_route_challenges_for_basic_credentials(
    private_oci_ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = private_oci_ui_router;
    let response = router
        .oneshot(
            Request::builder()
                .uri("/+ui/project?index=images&project=app")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        (
            response.status(),
            response.headers()[header::WWW_AUTHENTICATE].to_str().unwrap(),
        ),
        (StatusCode::UNAUTHORIZED, "Basic realm=\"peryx\"")
    );
}

#[rstest]
#[tokio::test]
async fn test_ui_private_oci_data_api_accepts_its_bearer(private_oci_ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = private_oci_ui_router;
    push_oci_image(&router).await;
    let bearer = reader_bearer(&router).await;

    let (status, body) = get_authorized(&router, "/+ui/manifest?index=images&project=app&ref=1.0", &bearer).await;
    assert_eq!(
        (status, body.contains("application/vnd.oci.image.manifest.v1+json")),
        (StatusCode::OK, true),
        "{body}"
    );
}

#[rstest]
#[tokio::test]
async fn test_ui_private_oci_project_list_accepts_its_bearer(private_oci_ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = private_oci_ui_router;
    push_oci_image(&router).await;
    let bearer = reader_bearer(&router).await;

    let (status, body) = get_authorized(&router, "/+ui/projects?index=images", &bearer).await;

    assert_eq!(
        (status, serde_json::from_str::<serde_json::Value>(&body).unwrap()),
        (StatusCode::OK, serde_json::json!(["app"]))
    );
}

#[rstest]
#[tokio::test]
async fn test_ui_private_oci_project_list_rejects_bearer_for_another_index(
    private_oci_ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = private_oci_ui_router;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .cast_signed();
    let token = Signer::new(b"signing-secret", peryx_ecosystem_oci::TOKEN_SERVICE).mint(
        &Principal::Named {
            subject: "reader".to_owned(),
        },
        &[Grant {
            projects: vec![Glob::new("other/app")],
            actions: BTreeSet::from([Action::Read]),
        }],
        now,
        300,
    );

    let (status, _) = get_authorized(&router, "/+ui/projects?index=images", &format!("Bearer {token}")).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[rstest]
#[tokio::test]
async fn test_ui_private_oci_search_follows_read_acl(private_oci_ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = private_oci_ui_router;
    push_oci_image(&router).await;
    for (case, authorization, expected) in [
        ("anonymous", String::new(), (0, serde_json::Value::Null)),
        ("basic", reader_authorization(), (1, serde_json::json!("app"))),
        ("bearer", reader_bearer(&router).await, (1, serde_json::json!("app"))),
    ] {
        let (status, body) = get_authorized(&router, "/+search?q=app", &authorization).await;
        let document = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        assert_eq!(
            (
                status,
                document["total"].clone(),
                document["results"][0]["normalized_name"].clone()
            ),
            (StatusCode::OK, serde_json::json!(expected.0), expected.1),
            "{case}"
        );
    }
}

fn private_oci_ui_config(dir: &tempfile::TempDir) -> Config {
    let mut config = oci_ui_config(dir);
    config.indexes[0].anonymous_read = Some(false);
    config.indexes[0].tokens.push(TokenConfig {
        name: "reader".to_owned(),
        secret: SecretSource::Literal("read-secret".to_owned()),
        projects: vec!["app".to_owned()],
        actions: BTreeSet::from([Action::Read]),
        expires_at: None,
    });
    let mut public = config.indexes[0].clone();
    public.name = "public".to_owned();
    public.route = "public".to_owned();
    public.anonymous_read = None;
    public.tokens.clear();
    config.indexes.push(public);
    config.auth.signing_key = Some(SecretSource::Literal("signing-secret".to_owned()));
    config
}

fn reader_authorization() -> String {
    format!("Basic {}", STANDARD.encode("_:read-secret"))
}

async fn reader_bearer(router: &axum::Router) -> String {
    let (status, body) = get_authorized(
        router,
        "/v2/token?service=peryx&scope=repository:images/app:pull",
        &reader_authorization(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    format!("Bearer {token}")
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, String) {
    get_authorized(router, uri, "").await
}

async fn get_authorized(router: &axum::Router, uri: &str, authorization: &str) -> (StatusCode, String) {
    let mut request = Request::builder().uri(uri);
    if !authorization.is_empty() {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn first_files_table(body: &str) -> &str {
    let table = body.find("<table class=\"files\"").unwrap();
    let rest = &body[table..];
    &rest[..rest.find("</table>").unwrap()]
}

/// Upload the frontend fixture wheel through the router, so UI pages have a metadata-rich package.
async fn upload_fixture(router: &axum::Router) {
    let wheel = include_bytes!("../../../../tests/frontend/fixtures/veloxdemo-1.0.0-py3-none-any.whl");
    upload_file(router, "veloxdemo-1.0.0-py3-none-any.whl", wheel).await;
}

async fn upload_file(router: &axum::Router, filename: &str, content: &[u8]) {
    let boundary = "peryxuitest";
    let mut body = Vec::new();
    let filetype = if filename.ends_with(".tar.gz") {
        "sdist"
    } else {
        "bdist_wheel"
    };
    let sha256 = Digest::of(content);
    for (name, value) in [
        (":action", "file_upload"),
        ("name", "veloxdemo"),
        ("version", "1.0.0"),
        ("filetype", filetype),
        ("sha256_digest", sha256.as_str()),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; \
             filename=\"{filename}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .uri("/root/pypi/")
        .method("POST")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("__token__:s3cret")),
        )
        .body(Body::from(body))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

fn put_file(state: &peryx_driver::AppState, filename: &str, content: &[u8], core_metadata: CoreMetadata) -> Digest {
    let digest = Digest::of(content);
    state.blobs.write_verified(content, &digest).unwrap();
    let uploaded = Uploaded {
        version: "1.0.0".to_owned(),
        file: File {
            filename: filename.to_owned(),
            url: local_file_url("hosted", digest.as_str(), filename),
            hashes: std::collections::BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
            requires_python: None,
            size: Some(content.len() as u64),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
    };
    state
        .meta
        .put_upload("hosted", "veloxdemo", filename, &to_json(&uploaded).into_bytes())
        .unwrap();
    state.meta.put_project("hosted", "veloxdemo", "veloxdemo").unwrap();
    digest
}

fn put_legacy_file(state: &peryx_driver::AppState, filename: &str, content: &[u8]) -> Digest {
    put_file(state, filename, content, CoreMetadata::Absent)
}

fn put_filter_files(state: &peryx_driver::AppState) {
    put_legacy_file(state, "veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl", b"wheel 1");
    put_legacy_file(state, "veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl", b"wheel 2");
    put_legacy_file(state, "veloxdemo-1.0.0.tar.gz", b"sdist");
}

#[rstest]
#[tokio::test]
async fn test_ui_dashboard_renders_indexes_and_counters(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    let (status, body) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("change serial"));
    assert!(body.contains("root/pypi"));
    assert!(body.contains("badge kind-virtual"));
    assert!(body.contains("badge uploads"));
    // The virtual index renders its layers as an ordered stack with the upload target marked.
    assert!(body.contains("layer-stack"));
    assert!(body.contains("uploads land here"));
    assert!(body.contains("resolves top to bottom"));
    assert!(body.contains("/pkg/peryx_web.js"));
}

#[tokio::test]
async fn test_ui_dashboard_shows_the_oci_registry_endpoint_not_simple() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&oci_ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    // An OCI index card advertises the `/v2/` registry endpoint, never a PyPI `/simple/` URL.
    assert!(body.contains("/v2/images/"), "OCI endpoint missing: {body}");
    assert!(
        !body.contains("/images/simple/"),
        "OCI card wrongly shows a Simple URL: {body}"
    );
}

#[rstest]
#[tokio::test]
async fn test_ui_admin_status_renders_read_only_state_without_secrets(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    let (status, body) = get(&router, "/admin/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Admin status"));
    assert!(body.contains("read-only"));
    assert!(body.contains("root/pypi"));
    assert!(body.contains("/root/pypi/simple/"));
    assert!(body.contains("/browse?index=hosted"));
    assert!(body.contains("Usage and health"));
    assert!(body.contains("Recent uploads"));
    assert!(body.contains("No uploads recorded yet."));
    assert!(body.contains("token configured"));
    assert!(body.contains("redacted"));
    assert!(body.contains("http://127.0.0.1:9/simple/"));
    assert!(body.contains("upload-enabled"));
    assert!(!body.contains("s3cret"));
    assert!(!body.contains("type=\"password\""));
    assert!(!body.contains("delete whole project"));
}

#[tokio::test]
async fn test_ui_admin_status_empty_state() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&empty_ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/admin/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No indexes configured."));
    assert!(body.contains("No usage recorded yet."));
    assert!(body.contains("No uploads recorded yet."));
}

#[rstest]
#[tokio::test]
async fn test_ui_admin_status_lists_counts_and_recent_uploads(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    upload_fixture(&router).await;
    let (status, body) = get(&router, "/admin/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("uploads"));
    assert!(body.contains("veloxdemo"));
    assert!(body.contains("veloxdemo-1.0.0-py3-none-any.whl"));
    assert!(body.contains("1.2 kB"));
    assert!(!body.contains("A demonstration package"));
}

#[rstest]
#[tokio::test]
async fn test_ui_browse_lists_projects_after_upload(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    upload_fixture(&router).await;
    let (status, body) = get(&router, "/browse?index=hosted").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("veloxdemo"));
    assert!(body.contains("Filter projects"));
}

#[rstest]
#[tokio::test]
async fn test_ui_browse_empty_index_hint(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    let (status, body) = get(&router, "/browse?index=hosted").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No projects observed"));
}

#[rstest]
#[tokio::test]
async fn test_ui_project_page_renders_metadata(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    upload_fixture(&router).await;
    let (status, body) = get(&router, "/browse?index=hosted&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("A demonstration package for the velox web UI"));
    assert!(body.contains("uv pip install --index-url /hosted/simple/ veloxdemo"));
    assert!(body.contains("A demo package served by <strong>velox</strong>"));
    assert!(body.contains("requests&gt;=2"));
    assert!(body.contains("Development Status"));
    assert!(body.contains("badge meta-badge"));
    assert!(body.contains("Manage uploads"));
    assert!(body.contains("1.2 kB"));
}

#[tokio::test]
async fn test_ui_project_page_sanitizes_metadata_links() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir)).unwrap();
    let metadata = concat!(
        "Metadata-Version: 2.1\n",
        "Name: veloxdemo\n",
        "Version: 1.0.0\n",
        "Project-URL: Documentation, https://example.com/docs\n",
        "Project-URL: Unsafe, JaVaScRiPt:alert(1)\n",
        "Description-Content-Type: text/markdown\n\n",
        "[guide](https://example.com/guide) [unsafe](data:text/html;base64,PHNjcmlwdD4=)\n",
    );
    let metadata_digest = state.blobs.write(metadata.as_bytes()).unwrap();
    put_file(
        &state,
        "veloxdemo-1.0.0-py3-none-any.whl",
        &wheel_with_metadata(metadata),
        CoreMetadata::Hashes(std::collections::BTreeMap::from([(
            "sha256".to_owned(),
            metadata_digest.as_str().to_owned(),
        )])),
    );
    let router = router_for(state);
    let (status, body) = get(&router, "/browse?index=hosted&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("href=\"https://example.com/docs\""));
    assert!(body.contains("href=\"https://example.com/guide\""));
    assert!(body.contains("rel=\"external nofollow noopener noreferrer\""));
    assert!(body.contains("<li>Unsafe</li>"));
    assert!(body.contains(">guide</a> unsafe</p>"));
    assert!(!body.contains("href=\"JaVaScRiPt:"), "{body}");
    assert!(!body.contains("href=\"data:text/html"), "{body}");
}

fn wheel_with_metadata(metadata: &str) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    let dist_info = "veloxdemo-1.0.0.dist-info";
    let entries = [
        (format!("{dist_info}/METADATA"), metadata.as_bytes().to_vec()),
        (
            format!("{dist_info}/WHEEL"),
            b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n".to_vec(),
        ),
    ];
    for (path, bytes) in &entries {
        zip.start_file(path, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    let record_path = format!("{dist_info}/RECORD");
    zip.start_file(&record_path, options).unwrap();
    zip.write_all(wheel_record(&entries, &record_path).as_bytes()).unwrap();
    zip.finish().unwrap().into_inner()
}

#[rstest]
#[case::substring(
    "/browse?index=hosted&project=veloxdemo&filename=cp312",
    &["1 of 3 files"][..],
    &["veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl"][..],
    &["veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl", "veloxdemo-1.0.0.tar.gz"][..]
)]
#[case::regex(
    "/browse?index=hosted&project=veloxdemo&filename=cp31%5B12%5D.*whl&filename_match=regex",
    &["2 of 3 files"][..],
    &["veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl", "veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl"][..],
    &["veloxdemo-1.0.0.tar.gz"][..]
)]
#[case::invalid_regex(
    "/browse?index=hosted&project=veloxdemo&filename=%5B&filename_match=regex",
    &["Invalid regex", "3 files"][..],
    &[
        "veloxdemo-1.0.0-cp311-cp311-macosx_14_0_arm64.whl",
        "veloxdemo-1.0.0-cp312-cp312-macosx_14_0_arm64.whl",
        "veloxdemo-1.0.0.tar.gz",
    ][..],
    &[][..]
)]
#[tokio::test]
async fn test_ui_project_page_filters_files(
    filter_router: (tempfile::TempDir, axum::Router),
    #[case] query: &str,
    #[case] count_text: &[&str],
    #[case] present: &[&str],
    #[case] absent: &[&str],
) {
    let (_dir, router) = filter_router;
    let (status, body) = get(&router, query).await;
    assert_eq!(status, StatusCode::OK);
    for text in count_text {
        assert!(body.contains(text), "{body}");
    }
    let table = first_files_table(&body);
    for file in present {
        assert!(table.contains(file), "{body}");
    }
    for file in absent {
        assert!(!table.contains(file), "{body}");
    }
}

#[rstest]
#[tokio::test]
async fn test_ui_project_page_missing_project(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    let (status, body) = get(&router, "/browse?index=hosted&project=ghost").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Project not found on this index."));
}

#[tokio::test]
async fn test_ui_project_page_shows_contents_for_zipped_eggs() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir)).unwrap();
    let mut egg = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut egg));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("EGG-INFO/PKG-INFO", options).unwrap();
        zip.write_all(b"Metadata-Version: 1.2\nName: veloxdemo\nVersion: 1.0.0\n")
            .unwrap();
        zip.finish().unwrap();
    }
    let digest = put_legacy_file(&state, "veloxdemo-1.0.0.egg", &egg);
    let router = router_for(state);
    let (status, body) = get(&router, "/browse?index=hosted&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("veloxdemo-1.0.0.egg"));
    assert!(body.contains("class=\"inspect\""));

    let url = format!(
        "/browse?index=hosted&project=veloxdemo&sha256={}&file=veloxdemo-1.0.0.egg",
        digest.as_str()
    );
    let (status, body) = get(&router, &url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("EGG-INFO/PKG-INFO"));
}

#[tokio::test]
async fn test_ui_project_page_hides_contents_for_unsupported_legacy_tar() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&ui_config(&dir)).unwrap();
    put_legacy_file(&state, "veloxdemo-1.0.0.tar.bz2", b"legacy tarball");
    let router = router_for(state);
    let (status, body) = get(&router, "/browse?index=hosted&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("veloxdemo-1.0.0.tar.bz2"));
    assert!(!body.contains("class=\"inspect\""));
}

#[rstest]
#[tokio::test]
async fn test_ui_archive_listing_and_member(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    upload_fixture(&router).await;
    let (_, detail) = get(&router, "/hosted/simple/veloxdemo/").await;
    let sha = detail
        .split("files/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();

    let file = "veloxdemo-1.0.0-py3-none-any.whl";
    let listing_url = format!("/browse?index=hosted&project=veloxdemo&sha256={sha}&file={file}");
    let (status, listing) = get(&router, &listing_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("dist-info/METADATA"));
    assert!(listing.contains("__init__.py"));

    let member = format!("{listing_url}&member=veloxdemo-1.0.0.dist-info%2FMETADATA");
    let (status, content) = get(&router, &member).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("Metadata-Version: 2.1"));
    assert!(content.contains("back to archive"));
}

#[rstest]
#[tokio::test]
async fn test_ui_archive_tree_links_nested_archives_and_blocks_binary_preview(
    ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = ui_router;
    let mut inner = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut inner));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("pkg/mod.py", options).unwrap();
        zip.write_all(b"x = 1\n").unwrap();
        zip.finish().unwrap();
    }
    let mut wheel = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut wheel));
        let options = zip::write::SimpleFileOptions::default();
        let dist_info = "veloxdemo-1.0.0.dist-info";
        let entries = vec![
            ("veloxdemo/__init__.py".to_owned(), Vec::new()),
            ("veloxdemo/data.bin".to_owned(), vec![0xff, 0xfe]),
            ("vendor/inner.zip".to_owned(), inner),
            (
                format!("{dist_info}/METADATA"),
                b"Metadata-Version: 2.1\nName: veloxdemo\nVersion: 1.0.0\n".to_vec(),
            ),
            (
                format!("{dist_info}/WHEEL"),
                b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n".to_vec(),
            ),
        ];
        for (path, bytes) in &entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let record_path = format!("{dist_info}/RECORD");
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(wheel_record(&entries, &record_path).as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    upload_file(&router, "veloxdemo-1.0.0-py3-none-any.whl", &wheel).await;
    let (_, detail) = get(&router, "/hosted/simple/veloxdemo/").await;
    let sha = detail
        .split("files/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();

    let file = "veloxdemo-1.0.0-py3-none-any.whl";
    let listing_url = format!("/browse?index=hosted&project=veloxdemo&sha256={sha}&file={file}");
    let (status, listing) = get(&router, &listing_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("class=\"archive-tree\""));
    assert!(listing.contains("vendor"));
    assert!(listing.contains("inner.zip"));
    assert!(listing.contains("container=vendor%2Finner.zip"));
    assert!(listing.contains("data.bin"));
    assert!(!listing.contains("member=veloxdemo%2Fdata.bin"));

    let binary_url = format!("{listing_url}&member=veloxdemo%2Fdata.bin");
    let (status, binary) = get(&router, &binary_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(binary.contains("archive member"));
    assert!(binary.contains("cannot be previewed inline"));

    let nested_url = format!("{listing_url}&container=vendor%2Finner.zip");
    let (status, nested) = get(&router, &nested_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(nested.contains("pkg"));
    assert!(nested.contains("mod.py"));

    let member_url = format!("{nested_url}&member=pkg%2Fmod.py");
    let (status, content) = get(&router, &member_url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("x = 1"));
}

fn wheel_record(entries: &[(String, Vec<u8>)], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes) in entries {
        let digest = URL_SAFE_NO_PAD.encode(Sha256::digest(bytes));
        writeln!(record, "{path},sha256={digest},{}", bytes.len()).unwrap();
    }
    writeln!(record, "{record_path},,").unwrap();
    record
}

#[rstest]
#[tokio::test]
async fn test_ui_unknown_route_falls_back(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    let (status, body) = get(&router, "/nosuchpage").await;
    // The catch-all API dispatcher answers for non-UI paths.
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

#[rstest]
#[tokio::test]
async fn test_ui_stats_drills_from_index_to_files(ui_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = ui_router;
    upload_fixture(&router).await;
    // The aggregator applies the upload event on its own thread; poll the rendered page.
    let mut body = String::new();
    for _ in 0..500 {
        let (status, page) = get(&router, "/stats?index=root%2Fpypi").await;
        assert_eq!(status, StatusCode::OK);
        if page.contains("veloxdemo") {
            body = page;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(body.contains("uploads"));
    // Leptos escapes attribute ampersands in server output.
    assert!(body.contains("/stats?index=root%2Fpypi&amp;project=veloxdemo"));

    let (status, top) = get(&router, "/stats").await;
    assert_eq!(status, StatusCode::OK);
    assert!(top.contains("/stats?index=root%2Fpypi"));

    let (status, files) = get(&router, "/stats?index=root%2Fpypi&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(files.contains("rejected downloads"));
}
