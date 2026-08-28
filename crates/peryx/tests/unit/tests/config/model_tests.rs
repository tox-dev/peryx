use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;

use peryx_driver::rate_limit::RateLimitConfig;

use peryx_storage::blob::DurabilityRequirement;

use crate::config::{
    AvailabilityConfig, AvailabilityMode, BlobStorageConfig, Config, LogConfig, ReplicationConfig, S3StorageConfig,
    SecretSource,
};

#[test]
fn test_secret_source_file_returns_trimmed_contents() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(b"  s3cr3t\n").unwrap();
    assert_eq!(SecretSource::File(file.path().to_owned()).read().unwrap(), "s3cr3t");
}

#[test]
fn test_secret_source_file_missing_reports_path_without_value() {
    let err = SecretSource::File(PathBuf::from("/nonexistent/peryx/secret"))
        .read()
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("failed to read config file /nonexistent/peryx/secret:"),
        "{err}"
    );
}

#[test]
fn test_secret_source_empty_file_is_rejected() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(b"   \n").unwrap();
    assert_eq!(
        SecretSource::File(file.path().to_owned())
            .read()
            .unwrap_err()
            .to_string(),
        format!("secret file {} holds no secret", file.path().display())
    );
}

#[test]
fn test_secret_source_oversize_file_is_rejected_without_value() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&vec![b'a'; (1 << 20) + 1]).unwrap();
    assert_eq!(
        SecretSource::File(file.path().to_owned())
            .read()
            .unwrap_err()
            .to_string(),
        format!("secret file {} exceeds the 1048576-byte limit", file.path().display())
    );
}

#[test]
fn test_secret_source_env_reads_a_present_variable() {
    let path = std::env::var("PATH").expect("PATH is set for the test process");
    assert_eq!(SecretSource::Env("PATH".to_owned()).read().unwrap(), path.trim());
}

#[test]
fn test_secret_source_env_missing_reports_variable_without_value() {
    assert_eq!(
        SecretSource::Env("PERYX_TEST_ABSENT_CREDENTIAL".to_owned())
            .read()
            .unwrap_err()
            .to_string(),
        "credential environment variable PERYX_TEST_ABSENT_CREDENTIAL is unset, empty, or not valid UTF-8"
    );
}

#[test]
fn test_default_config() {
    let c = Config::default();
    assert_eq!(c.host, "127.0.0.1");
    assert_eq!(c.port, 4433);
    assert_eq!(c.data_dir, PathBuf::from("peryx-data"));
    assert_eq!(c.writer_identity, None);
    assert_eq!(c.node_identity, None);
    assert!(!c.offline);
    assert!(!c.read_only);
    assert_eq!(c.cache_ttl_secs, 300);
    assert_eq!(c.log, LogConfig::default());
    assert_eq!(c.rate_limit, RateLimitConfig::default());
}

#[rstest::rstest]
#[case::ipv4("127.0.0.1", "127.0.0.1:4433")]
#[case::ipv6_wildcard("::", "[::]:4433")]
#[case::ipv6_loopback("::1", "[::1]:4433")]
fn test_listen_address_accepts_ip_literals(#[case] host: &str, #[case] expected: &str) {
    let config = Config {
        host: host.to_owned(),
        ..Config::default()
    };

    assert_eq!(
        config.listen_address().unwrap(),
        expected.parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn test_listen_address_resolves_localhost() {
    let config = Config {
        host: "localhost".to_owned(),
        ..Config::default()
    };

    let address = config.listen_address().unwrap();
    assert_eq!((address.ip().is_loopback(), address.port()), (true, 4433));
}

#[test]
fn test_config_rejects_a_blank_writer_identity() {
    let config = Config {
        writer_identity: Some(" \t".to_owned()),
        ..Config::default()
    };

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "writer identity: must not be blank"
    );
}

#[test]
fn test_config_requires_a_writer_identity_in_replica_mode() {
    let config = Config {
        availability: AvailabilityConfig::Dc(ReplicationConfig::Replica {
            upstream: "https://writer.example/".to_owned(),
            token: SecretSource::Literal("secret".to_owned()),
            poll_interval: std::time::Duration::from_secs(1),
            page_size: std::num::NonZeroUsize::MIN,
        }),
        ..Config::default()
    };

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "writer identity: required in read replica mode"
    );
}

#[test]
fn test_config_accepts_standalone_read_only_without_a_writer_identity() {
    let config = Config {
        read_only: true,
        ..Config::default()
    };

    config.validate().unwrap();
}

#[test]
fn test_config_rejects_a_writer_identity_without_replication() {
    let config = Config {
        writer_identity: Some("writer-a".to_owned()),
        ..Config::default()
    };

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "writer identity: requires `dc` or `ha` availability"
    );
}

#[rstest::rstest]
#[case::none(AvailabilityConfig::None)]
#[case::dc(dc_primary())]
fn test_config_rejects_node_identity_without_ha(#[case] availability: AvailabilityConfig) {
    let config = Config {
        availability,
        node_identity: Some("node-a".to_owned()),
        ..Config::default()
    };

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "availability: `node_identity` requires `ha` mode"
    );
}

fn s3_blob(conditional_writes: bool, checksum_writes: bool) -> BlobStorageConfig {
    BlobStorageConfig::S3(s3_config(conditional_writes, checksum_writes))
}

fn s3_config(conditional_writes: bool, checksum_writes: bool) -> S3StorageConfig {
    S3StorageConfig {
        endpoint: "https://s3.example.com".to_owned(),
        bucket: "cache".to_owned(),
        prefix: String::new(),
        region: "us-east-1".to_owned(),
        path_style: false,
        request_timeout: std::time::Duration::from_secs(30),
        max_retries: 3,
        multipart_threshold: 16 << 20,
        part_size: 16 << 20,
        upload_concurrency: 4,
        conditional_writes,
        checksum_writes,
    }
}

#[test]
fn test_s3_storage_debug_redacts_the_endpoint() {
    assert_eq!(
        format!("{:?}", s3_config(true, false)),
        "S3StorageConfig { endpoint: \"<redacted>\", bucket: \"cache\", prefix: \"\", region: \"us-east-1\", \
         path_style: false, request_timeout: 30s, max_retries: 3, multipart_threshold: 16777216, part_size: 16777216, \
         upload_concurrency: 4, conditional_writes: true, checksum_writes: false }"
    );
}

fn dc_primary() -> AvailabilityConfig {
    AvailabilityConfig::Dc(ReplicationConfig::Primary {
        source: "primary-a".to_owned(),
        token: SecretSource::Literal("secret".to_owned()),
    })
}

#[rstest::rstest]
#[case::filesystem_dc(dc_primary(), BlobStorageConfig::Filesystem)]
#[case::filesystem_ha(AvailabilityConfig::Ha(ReplicationConfig::Primary {
    source: "primary-a".to_owned(),
    token: SecretSource::Literal("secret".to_owned()),
}), BlobStorageConfig::Filesystem)]
#[case::object_store_dc(dc_primary(), s3_blob(true, true))]
#[case::basic_object_store_none(AvailabilityConfig::None, s3_blob(false, false))]
fn test_config_accepts_a_backend_that_proves_the_mode_durability(
    #[case] availability: AvailabilityConfig,
    #[case] blob: BlobStorageConfig,
) {
    let config = Config {
        availability,
        blob,
        ..Config::default()
    };

    config.validate().unwrap();
}

#[rstest::rstest]
#[case::dc_without_conditional_writes(
    dc_primary(),
    s3_blob(false, true),
    "blob storage durability: dc availability requires conditional create-if-absent writes, \
     which the configured backend cannot prove"
)]
#[case::ha_without_checksum_writes(
    AvailabilityConfig::Ha(ReplicationConfig::Primary {
        source: "primary-a".to_owned(),
        token: SecretSource::Literal("secret".to_owned()),
    }),
    s3_blob(true, false),
    "blob storage durability: ha availability requires checksum-validated writes, \
     which the configured backend cannot prove"
)]
fn test_config_rejects_a_backend_that_cannot_prove_the_mode_durability(
    #[case] availability: AvailabilityConfig,
    #[case] blob: BlobStorageConfig,
    #[case] expected: &str,
) {
    let config = Config {
        availability,
        blob,
        ..Config::default()
    };

    assert_eq!(config.validate().unwrap_err().to_string(), expected);
}

#[test]
fn test_availability_mode_maps_to_its_durability_requirement() {
    assert_eq!(AvailabilityMode::None.as_str(), "none");
    assert_eq!(AvailabilityMode::Dc.as_str(), "dc");
    assert_eq!(AvailabilityMode::Ha.as_str(), "ha");
    assert_eq!(
        AvailabilityMode::None.durability_requirement(),
        DurabilityRequirement::LOCAL
    );
    assert_eq!(
        AvailabilityMode::Dc.durability_requirement(),
        DurabilityRequirement::REPLICATED
    );
    assert_eq!(
        AvailabilityMode::Ha.durability_requirement(),
        DurabilityRequirement::REPLICATED
    );
}
