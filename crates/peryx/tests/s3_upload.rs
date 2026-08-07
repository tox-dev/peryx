use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx::config::{BlobStorageConfig, Config, IndexConfig, IndexKind, S3StorageConfig, SecretSource, TokenConfig};
use peryx::server::build_router;
use peryx_ecosystem_registry::pypi::store::PypiStore as _;
use peryx_identity::Action;
use peryx_storage::blob::{BlobStorage, Digest, S3Config, S3Settings};
use peryx_storage::meta::MetaStore;
use testcontainers::ContainerAsync;
use testcontainers::core::ImageExt as _;
use testcontainers::runners::AsyncRunner as _;
use testcontainers_modules::minio::MinIO;
use tokio::process::Command;
use tower::ServiceExt as _;

const ACCESS_KEY: &str = "peryx-minio";
const SECRET_KEY: &str = "peryx-minio-secret";
const BUCKET: &str = "peryx-tests";
const CHILD: &str = "PERYX_S3_UPLOAD_CHILD";
const ENDPOINT: &str = "PERYX_S3_UPLOAD_ENDPOINT";
const DATA_DIR: &str = "PERYX_S3_UPLOAD_DATA_DIR";
const RUN_CONTAINERS: &str = "PERYX_RUN_CONTAINER_TESTS";
const FILENAME: &str = "veloxdemo-1.0.0-py3-none-any.whl";
const WHEEL: &[u8] = include_bytes!("../../../tests/frontend/fixtures/veloxdemo-1.0.0-py3-none-any.whl");

struct Minio {
    _container: ContainerAsync<MinIO>,
    endpoint: String,
}

fn containers_enabled() -> bool {
    cfg!(target_os = "linux") || std::env::var_os(RUN_CONTAINERS).is_some()
}

fn settings(endpoint: String) -> S3Settings {
    S3Settings {
        endpoint,
        bucket: BUCKET.to_owned(),
        prefix: "cache".to_owned(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: Duration::from_secs(10),
        max_retries: 2,
        multipart_threshold: 5 << 20,
        part_size: 5 << 20,
        upload_concurrency: 2,
        conditional_writes: true,
        checksum_writes: true,
    }
}

fn admin_client(endpoint: &str) -> aws_sdk_s3::Client {
    aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .credentials_provider(Credentials::new(ACCESS_KEY, SECRET_KEY, None, None, "test"))
            .region(Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .build(),
    )
}

async fn minio() -> Minio {
    let container = MinIO::default()
        .with_tag("RELEASE.2025-04-22T22-12-26Z")
        .with_env_var("MINIO_ROOT_USER", ACCESS_KEY)
        .with_env_var("MINIO_ROOT_PASSWORD", SECRET_KEY)
        .start()
        .await
        .unwrap();
    let endpoint = format!(
        "http://127.0.0.1:{}",
        container.get_host_port_ipv4(9_000).await.unwrap()
    );
    admin_client(&endpoint)
        .create_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .unwrap();
    Minio {
        _container: container,
        endpoint,
    }
}

fn hosted() -> IndexConfig {
    IndexConfig {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_ecosystem_registry::PYPI,
        anonymous_read: None,
        tokens: vec![TokenConfig {
            name: "uploader".to_owned(),
            secret: SecretSource::Literal("s3cret".to_owned()),
            projects: vec!["*".to_owned()],
            actions: [Action::Write, Action::Delete].into_iter().collect(),
            expires_at: None,
        }],
        kind: IndexKind::Hosted { volatile: true },
    }
}

fn config(data_dir: PathBuf, endpoint: String) -> Config {
    Config {
        data_dir,
        blob: BlobStorageConfig::S3(S3StorageConfig {
            endpoint,
            bucket: BUCKET.to_owned(),
            prefix: "cache".to_owned(),
            region: "us-east-1".to_owned(),
            path_style: true,
            request_timeout: Duration::from_secs(10),
            max_retries: 2,
            multipart_threshold: 5 << 20,
            part_size: 5 << 20,
            upload_concurrency: 2,
            conditional_writes: true,
            checksum_writes: true,
        }),
        indexes: vec![hosted()],
        ..Config::default()
    }
}

fn upload_request() -> Request<Body> {
    let boundary = "peryxs3upload";
    let mut body = Vec::new();
    for (name, value) in [
        (":action", "file_upload"),
        ("name", "veloxdemo"),
        ("version", "1.0.0"),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", Digest::of(WHEEL).as_str()),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"{FILENAME}\"\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(WHEEL);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Request::builder()
        .uri("/hosted/")
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
        .unwrap()
}

async fn child(endpoint: &str, data_dir: &Path) -> Output {
    tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("test_s3_upload_child")
            .arg("--nocapture")
            .env(CHILD, "1")
            .env(ENDPOINT, endpoint)
            .env(DATA_DIR, data_dir)
            .env("AWS_ACCESS_KEY_ID", ACCESS_KEY)
            .env("AWS_SECRET_ACCESS_KEY", SECRET_KEY)
            .env("AWS_REGION", "us-east-1")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env_remove("AWS_PROFILE")
            .env_remove("AWS_SHARED_CREDENTIALS_FILE")
            .output(),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn test_s3_upload_metadata_failure_leaves_a_detectable_orphan() {
    if !containers_enabled() {
        return;
    }
    let minio = minio().await;
    let data_dir = tempfile::tempdir().unwrap();
    MetaStore::open(data_dir.path().join("peryx.redb"))
        .unwrap()
        .put_upload("hosted", "veloxdemo", FILENAME, b"invalid-json")
        .unwrap();
    let output = child(&minio.endpoint, data_dir.path()).await;
    assert!(
        output.status.success(),
        "child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn test_s3_upload_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let endpoint = std::env::var(ENDPOINT).unwrap();
    let data_dir = PathBuf::from(std::env::var_os(DATA_DIR).unwrap());
    assert_eq!(
        build_router(&config(data_dir.clone(), endpoint.clone()))
            .unwrap()
            .oneshot(upload_request())
            .await
            .unwrap()
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(
        BlobStorage::s3(
            S3Config::new(settings(endpoint)).unwrap(),
            data_dir.join("orphan-check"),
        )
        .head(&Digest::of(WHEEL))
        .await
        .unwrap()
        .is_some()
    );
}
