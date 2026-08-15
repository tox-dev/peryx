use std::path::PathBuf;
use std::time::Duration;

use anyhow::ensure;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::{Parser, Subcommand};
use peryx::config::{BlobStorageConfig, Config, IndexConfig, IndexKind, S3StorageConfig, SecretSource, TokenConfig};
use peryx::server::build_router;
use peryx_core::Ecosystem;
use peryx_identity::Action;
use peryx_storage::blob::{BlobStorage, Digest, S3Config, S3Settings};
use tower::ServiceExt as _;

const BUCKET: &str = "peryx-tests";
const FILENAME: &str = "veloxdemo-1.0.0-py3-none-any.whl";
const WHEEL: &[u8] = include_bytes!("veloxdemo-1.0.0-py3-none-any.whl");

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: FixtureCommand,
}

#[derive(Subcommand)]
enum FixtureCommand {
    UploadOrphan { endpoint: String, data_dir: PathBuf },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Args::parse().command {
        FixtureCommand::UploadOrphan { endpoint, data_dir } => upload_orphan(endpoint, data_dir).await,
    }
}

async fn upload_orphan(endpoint: String, data_dir: PathBuf) -> anyhow::Result<()> {
    ensure!(
        build_router(&config(data_dir.clone(), endpoint.clone()))?
            .oneshot(upload_request())
            .await?
            .status()
            == StatusCode::INTERNAL_SERVER_ERROR,
        "upload did not fail after the metadata write"
    );
    ensure!(
        BlobStorage::s3(S3Config::new(settings(endpoint))?, data_dir.join("orphan-check"),)
            .head(&Digest::of(WHEEL))
            .await?
            .is_some(),
        "failed upload did not leave a detectable blob"
    );
    Ok(())
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

fn hosted() -> IndexConfig {
    IndexConfig {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: Ecosystem::new("pypi"),
        anonymous_read: None,
        tokens: vec![TokenConfig {
            name: "uploader".to_owned(),
            secret: SecretSource::Literal("s3cret".to_owned()),
            resources: vec!["*".to_owned()],
            actions: [Action::Write, Action::Delete].into_iter().collect(),
            expires_at: None,
        }],
        kind: IndexKind::Hosted { volatile: true },
    }
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
