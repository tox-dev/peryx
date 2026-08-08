use std::time::Duration;

use super::toml_config;
use crate::config::{self, BlobStorageConfig, Config, S3StorageConfig};

#[test]
fn test_blob_defaults_to_the_filesystem_backend() {
    assert_eq!(Config::default().blob, BlobStorageConfig::Filesystem);
}

#[test]
fn test_blob_filesystem_backend_from_toml() {
    let config = toml_config("[blob]\nbackend = \"filesystem\"\n");
    assert_eq!(config.blob, BlobStorageConfig::Filesystem);
}

#[test]
fn test_blob_s3_backend_from_toml_applies_defaults() {
    let config = toml_config(
        "[blob]\nbackend = \"s3\"\nendpoint = \"https://s3.example.com\"\nbucket = \"cache\"\nregion = \"us-east-1\"\n",
    );
    assert_eq!(
        config.blob,
        BlobStorageConfig::S3(S3StorageConfig {
            endpoint: "https://s3.example.com".to_owned(),
            bucket: "cache".to_owned(),
            prefix: String::new(),
            region: "us-east-1".to_owned(),
            path_style: false,
            request_timeout: Duration::from_secs(30),
            max_retries: 3,
            multipart_threshold: 16 << 20,
            part_size: 16 << 20,
            upload_concurrency: 4,
            conditional_writes: true,
            checksum_writes: true,
        })
    );
}

#[test]
fn test_blob_s3_backend_from_toml_overrides_every_field() {
    let config = toml_config(concat!(
        "[blob]\n",
        "backend = \"s3\"\n",
        "endpoint = \"http://minio:9000\"\n",
        "bucket = \"blobs\"\n",
        "region = \"eu-central-1\"\n",
        "prefix = \"/peryx/\"\n",
        "path_style = true\n",
        "timeout_secs = 15\n",
        "max_retries = 5\n",
        "multipart_threshold_bytes = 32\n",
        "part_size_bytes = 8388608\n",
        "upload_concurrency = 8\n",
    ));
    assert_eq!(
        config.blob,
        BlobStorageConfig::S3(S3StorageConfig {
            endpoint: "http://minio:9000".to_owned(),
            bucket: "blobs".to_owned(),
            prefix: "peryx".to_owned(),
            region: "eu-central-1".to_owned(),
            path_style: true,
            request_timeout: Duration::from_secs(15),
            max_retries: 5,
            multipart_threshold: 32,
            part_size: 8 << 20,
            upload_concurrency: 8,
            conditional_writes: true,
            checksum_writes: true,
        })
    );
}

#[test]
fn test_blob_s3_backend_rejects_an_empty_bucket() {
    let error = config::from_toml(
        std::path::PathBuf::from("x.toml"),
        "[blob]\nbackend = \"s3\"\nendpoint = \"https://s3.example.com\"\nbucket = \"\"\nregion = \"us-east-1\"\n",
    )
    .and_then(|partial| Config::default().apply(partial))
    .unwrap_err();
    assert!(matches!(error, config::ConfigError::Blob { .. }), "{error:?}");
    assert!(error.to_string().contains("bucket must not be empty"));
}

#[test]
fn test_blob_s3_backend_rejects_a_bad_endpoint() {
    let error = config::from_toml(
        std::path::PathBuf::from("x.toml"),
        "[blob]\nbackend = \"s3\"\nendpoint = \"ftp://s3\"\nbucket = \"b\"\nregion = \"r\"\n",
    )
    .and_then(|partial| Config::default().apply(partial))
    .unwrap_err();
    assert!(matches!(error, config::ConfigError::Blob { .. }), "{error:?}");
}

#[test]
fn test_blob_s3_backend_rejects_an_unknown_field() {
    let error = config::from_toml(
        std::path::PathBuf::from("x.toml"),
        "[blob]\nbackend = \"s3\"\nendpoint = \"https://s3\"\nbucket = \"b\"\nregion = \"r\"\nsecret = \"nope\"\n",
    )
    .unwrap_err();
    assert!(matches!(error, config::ConfigError::Parse { .. }), "{error:?}");
}
