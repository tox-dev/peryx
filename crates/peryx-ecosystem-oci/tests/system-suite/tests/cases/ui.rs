use std::collections::BTreeSet;
use std::io::Write as _;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_identity::{Action, Glob, Grant, Principal, Signer};
use peryx_storage::blob::Digest;
use rstest::{fixture, rstest};
use tower::ServiceExt as _;

use super::{get, get_authorized, get_with_origin, seed_administrator};
use crate::config::{Config, IndexConfig, IndexKind, SecretSource, TokenConfig};
use crate::server::{build_router, build_state, router_for};

const TOKEN_SIGNING_KEY: &str = "private-oci-ui-test-signing-key-32-bytes";

#[fixture]
fn private_oci_ui_router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&private_oci_ui_config(&dir)).unwrap();
    (dir, router)
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
            ecosystem: peryx_ecosystem_oci::ECOSYSTEM,
            anonymous_read: None,
            tokens: vec![crate::tests::writer_token(SecretSource::Literal("s3cret".to_owned()))],
            kind: IndexKind::Hosted { volatile: true },
        }],
        ..Config::default()
    }
}

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
    assert!(body.contains(&config), "config blob missing: {body}");
    assert!(body.contains(&layer), "layer blob missing: {body}");
    assert!(body.contains("Layers"), "layer heading missing: {body}");
}

#[tokio::test]
async fn test_ui_oci_manifest_command_uses_trusted_request_origin() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = oci_ui_config(&dir);
    config.rate_limit.trusted_proxies = vec!["127.0.0.1/32".parse().unwrap()];
    let router = build_router(&config).unwrap();
    push_oci_image(&router).await;
    let uri = "/browse?index=images&project=app&ref=1.0";
    let (_, untrusted) = get_with_origin(&router, uri, None).await;
    let (_, trusted) = get_with_origin(&router, uri, Some("127.0.0.1:443")).await;
    let (_, json) = get_with_origin(&router, &format!("/+ui{uri}"), Some("127.0.0.1:443")).await;
    let command = serde_json::from_str::<serde_json::Value>(&json).unwrap()["command"]
        .as_str()
        .unwrap()
        .to_owned();

    assert_eq!(
        (
            untrusted.contains("docker pull internal.test:8080/images/app:1.0"),
            trusted.contains("docker pull packages.example/images/app:1.0"),
            command,
        ),
        (true, true, "docker pull packages.example/images/app:1.0".to_owned())
    );
}

#[rstest]
#[case::anonymous(String::new(), false)]
#[case::reader(reader_authorization(), true)]
#[case::other_reader(other_reader_authorization(), false)]
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
#[tokio::test]
async fn test_ui_private_oci_browse_api_rejects_a_reader_for_another_repository(
    private_oci_ui_router: (tempfile::TempDir, axum::Router),
) {
    let (_dir, router) = private_oci_ui_router;
    push_oci_image(&router).await;

    let (status, body) = get_authorized(
        &router,
        "/+ui/browse?index=images&project=app&ref=1.0",
        &other_reader_authorization(),
    )
    .await;

    assert_eq!((status, body), (StatusCode::FORBIDDEN, String::new()));
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
    let token = Signer::new(TOKEN_SIGNING_KEY.as_bytes(), peryx_ecosystem_oci::TOKEN_SERVICE).mint(
        &Principal::Named {
            subject: "reader".to_owned(),
        },
        &[Grant {
            resources: vec![Glob::new("other/app")],
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
                document["results"][0]["resource_key"].clone()
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
        resources: vec!["app".to_owned()],
        actions: BTreeSet::from([Action::Read]),
        expires_at: None,
    });
    config.indexes[0].tokens.push(TokenConfig {
        name: "other-reader".to_owned(),
        secret: SecretSource::Literal("other-read-secret".to_owned()),
        resources: vec!["other".to_owned()],
        actions: BTreeSet::from([Action::Read]),
        expires_at: None,
    });
    let mut public = config.indexes[0].clone();
    "public".clone_into(&mut public.name);
    "public".clone_into(&mut public.route);
    public.anonymous_read = None;
    public.tokens.clear();
    config.indexes.push(public);
    config.auth.signing_key = Some(SecretSource::Literal(TOKEN_SIGNING_KEY.to_owned()));
    config
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

#[tokio::test]
async fn test_ui_dashboard_shows_the_oci_registry_endpoint_not_simple() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state(&oci_ui_config(&dir)).unwrap();
    let authorization = seed_administrator(&state).await;
    let router = router_for(state, axum::Router::new());
    let (status, body) = get_authorized(&router, "/", &authorization).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/v2/images/"), "OCI endpoint missing: {body}");
    assert!(
        !body.contains("/images/simple/"),
        "OCI card wrongly shows a Simple URL: {body}"
    );
}

fn reader_authorization() -> String {
    format!("Basic {}", STANDARD.encode("_:read-secret"))
}

fn other_reader_authorization() -> String {
    format!("Basic {}", STANDARD.encode("_:other-read-secret"))
}
