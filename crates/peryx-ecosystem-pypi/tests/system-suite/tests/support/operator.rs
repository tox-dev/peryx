use std::collections::BTreeMap;

use peryx::config::{BlobStorageConfig, S3StorageConfig, UpstreamConfig, UpstreamRoutingConfig, UpstreamTlsConfig};

pub fn single_route(url: &str) -> UpstreamRoutingConfig {
    UpstreamRoutingConfig {
        upstreams: vec![UpstreamConfig {
            name: "primary".to_owned(),
            url: url.to_owned(),
            artifact_url: None,
            trusted_hosts: Vec::new(),
            username: None,
            password: None,
            token: None,
            credential_exec: None,
            credential_refresh: None,
            tls: UpstreamTlsConfig::default(),
        }],
        fallback: true,
        protected: Vec::new(),
        pins: BTreeMap::new(),
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
