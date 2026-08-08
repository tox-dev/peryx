mod api;
mod app;
mod availability_listener_tests;
mod cli;
mod config;
mod finalize_home_dc_tests;
mod identity_availability_tests;
mod jobs_availability_tests;
mod logging_tests;
mod metrics_availability_tests;
mod none_mode_tests;
mod operator;
mod package_lifecycle_availability_tests;
mod prefetch;
mod read_through_install_tests;
mod replication_liveness_tests;
mod replication_tests;
mod server_tests;
mod shadow_tests;
mod tls_support;
mod trash_tests;
mod ui_tests;
mod upstream_tls_error_tests;
mod upstream_tls_tests;

/// A `[[index.access_token]]` grant that writes and deletes across every project, for a hosted index
/// that must accept uploads with `secret`.
fn writer_token(secret: crate::config::SecretSource) -> crate::config::TokenConfig {
    crate::config::TokenConfig {
        name: "uploader".to_owned(),
        secret,
        projects: vec!["*".to_owned()],
        actions: std::collections::BTreeSet::from([peryx_identity::Action::Write, peryx_identity::Action::Delete]),
        expires_at: None,
    }
}

/// A one-element upstream route over `url`, the config-model form a cached index takes when it has a
/// single source.
fn single_route(url: &str) -> crate::config::UpstreamRoutingConfig {
    use crate::config::{UpstreamConfig, UpstreamRoutingConfig, UpstreamTlsConfig};

    UpstreamRoutingConfig {
        upstreams: vec![UpstreamConfig {
            name: "primary".to_owned(),
            url: url.to_owned(),
            artifact_url: None,
            username: None,
            password: None,
            token: None,
            credential_exec: None,
            credential_refresh: None,
            tls: UpstreamTlsConfig::default(),
        }],
        fallback: true,
        protected: Vec::new(),
        pins: std::collections::BTreeMap::new(),
    }
}

/// A valid S3 blob backend, the config an offline blob command must refuse to run against.
fn s3_blob_backend() -> crate::config::BlobStorageConfig {
    crate::config::BlobStorageConfig::S3(crate::config::S3StorageConfig {
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
