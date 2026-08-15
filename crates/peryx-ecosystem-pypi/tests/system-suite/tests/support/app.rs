use std::collections::BTreeSet;

use peryx::config::{BlobStorageConfig, S3StorageConfig, SecretSource, TokenConfig};
use peryx_identity::Action;

pub fn writer_token(secret: SecretSource) -> TokenConfig {
    TokenConfig {
        name: "uploader".to_owned(),
        secret,
        resources: vec!["*".to_owned()],
        actions: BTreeSet::from([Action::Write, Action::Delete]),
        expires_at: None,
    }
}

pub fn s3_blob_backend() -> BlobStorageConfig {
    BlobStorageConfig::S3(S3StorageConfig {
        endpoint: "https://s3.example.com".to_owned(),
        bucket: "cache".to_owned(),
        prefix: "peryx".to_owned(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: std::time::Duration::from_secs(20),
        max_retries: 4,
        multipart_threshold: 1024,
        part_size: 8 << 20,
        upload_concurrency: 6,
        conditional_writes: true,
        checksum_writes: true,
    })
}
