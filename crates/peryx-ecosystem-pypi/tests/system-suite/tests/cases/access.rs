use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_identity::Action;
use peryx_storage::blob::Digest;
use rstest::{fixture, rstest};
use tower::ServiceExt as _;

use super::{get_authorized, reader};
use crate::config::{Config, IndexConfig, IndexKind, SecretSource, TokenConfig};
use crate::server::build_router;

const WHEEL: &str = "veloxdemo-1.0.0-py3-none-any.whl";

#[fixture]
fn private_router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&private_config(&dir)).unwrap();
    (dir, router)
}

fn private_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![IndexConfig {
            name: "vault".to_owned(),
            route: "vault".to_owned(),
            policy: peryx_policy::PolicyConfig::default(),
            ecosystem_policy: toml::Table::new(),
            ecosystem_settings: toml::Table::new(),
            webhooks: Vec::new(),
            ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
            anonymous_read: Some(false),
            tokens: vec![
                crate::tests::writer_token(SecretSource::Literal("s3cret".to_owned())),
                TokenConfig {
                    name: "reader".to_owned(),
                    secret: SecretSource::Literal("read-secret".to_owned()),
                    resources: vec!["*".to_owned()],
                    actions: BTreeSet::from([Action::Read]),
                    expires_at: None,
                },
            ],
            kind: IndexKind::Hosted { volatile: true },
        }],
        ..Config::default()
    }
}

async fn upload_private_fixture(router: &axum::Router) -> String {
    let wheel = include_bytes!("../../../fixtures/veloxdemo-1.0.0-py3-none-any.whl");
    let boundary = "peryxaccesstest";
    let sha256 = Digest::of(wheel);
    let mut body = Vec::new();
    for (name, value) in [
        (":action", "file_upload"),
        ("name", "veloxdemo"),
        ("version", "1.0.0"),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", sha256.as_str()),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"{WHEEL}\"\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(wheel);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .uri("/vault/")
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
    sha256.as_str().to_owned()
}

fn routes(sha256: &str) -> Vec<String> {
    vec![
        "/vault/simple/".to_owned(),
        "/vault/simple/veloxdemo/".to_owned(),
        "/vault/veloxdemo/json".to_owned(),
        format!("/vault/files/{sha256}/{WHEEL}"),
        format!("/vault/files/{sha256}/{WHEEL}.metadata"),
        format!("/vault/inspect/{sha256}/{WHEEL}"),
    ]
}

#[rstest]
#[tokio::test]
async fn test_private_index_challenges_every_anonymous_read(private_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = private_router;
    let sha256 = upload_private_fixture(&router).await;

    for uri in routes(&sha256) {
        assert_eq!(
            get_authorized(&router, &uri, "").await,
            (StatusCode::UNAUTHORIZED, "unauthorized".to_owned()),
            "{uri}"
        );
    }
}

#[rstest]
#[tokio::test]
async fn test_private_index_serves_every_read_to_a_reader(private_router: (tempfile::TempDir, axum::Router)) {
    let (_dir, router) = private_router;
    let sha256 = upload_private_fixture(&router).await;

    for uri in routes(&sha256) {
        let (status, body) = get_authorized(&router, &uri, &reader()).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(!body.is_empty(), "{uri}");
    }
}
