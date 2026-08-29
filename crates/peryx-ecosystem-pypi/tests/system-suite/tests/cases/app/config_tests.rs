use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use rstest::rstest;

use super::{bounded_output, config_at};
use crate::app::config_check;
use crate::config::{
    AcmeConfig, AvailabilityConfig, BlobStorageConfig, DcMember, DcMembership, DcRole, LogSink, ReplicationConfig,
    S3StorageConfig, SecretSource, TlsConfig,
};

#[rstest]
#[case::http_plural(
    None,
    false,
    "  listen: http://127.0.0.1:4433\n",
    "  indexes: 3 configured indexes\n"
)]
#[case::https_singular(
    Some(TlsConfig::Manual { cert: PathBuf::from("/cert.pem"), key: PathBuf::from("/key.pem") }),
    true,
    "  listen: https://127.0.0.1:4433\n",
    "  indexes: 1 configured index\n",
)]
#[case::acme(
    Some(TlsConfig::Acme(AcmeConfig {
        domains: vec!["packages.example".to_owned()],
        contact: "ops@example".to_owned(),
        cache_dir: PathBuf::from("/acme"),
        staging: false,
    })),
    false,
    "  listen: https+acme://127.0.0.1:4433\n",
    "  indexes: 3 configured indexes\n",
)]
fn test_config_check_summarizes_the_listener(
    #[case] tls: Option<TlsConfig>,
    #[case] single_index: bool,
    #[case] listen: &str,
    #[case] indexes: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_at(&dir);
    config.tls = tls;
    config.indexes.truncate(if single_index { 1 } else { 3 });
    let mut out = Vec::new();

    config_check(&config, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("configuration is valid\n"), "{text}");
    assert!(text.contains(listen), "{text}");
    assert!(text.contains(indexes), "{text}");
}

fn replicated_blob() -> BlobStorageConfig {
    BlobStorageConfig::S3(S3StorageConfig {
        endpoint: "https://s3.example".to_owned(),
        bucket: "peryx".to_owned(),
        prefix: String::new(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: Duration::from_secs(30),
        max_retries: 3,
        multipart_threshold: 16 << 20,
        part_size: 16 << 20,
        upload_concurrency: 4,
        conditional_writes: true,
        checksum_writes: true,
    })
}

fn dc_primary() -> ReplicationConfig {
    ReplicationConfig::Primary {
        source: "writer-a".to_owned(),
        token: SecretSource::Literal("top-secret".to_owned()),
    }
}

fn dc_replica() -> ReplicationConfig {
    ReplicationConfig::Replica {
        upstream: "https://primary.example/".to_owned(),
        token: SecretSource::Literal("top-secret".to_owned()),
        poll_interval: Duration::from_secs(1),
        page_size: NonZeroUsize::MIN,
    }
}

#[rstest]
#[case::none(AvailabilityConfig::None, None, "  availability: none (single node)\n")]
#[case::dc_primary(
    AvailabilityConfig::Dc(dc_primary()),
    None,
    "  availability: dc (primary, source \"writer-a\")\n"
)]
#[case::dc_replica(
    AvailabilityConfig::Dc(dc_replica()),
    None,
    "  availability: dc (replica, upstream \"https://primary.example/\")\n"
)]
#[case::ha_primary(
    AvailabilityConfig::Ha(dc_primary()),
    None,
    "  availability: ha (primary, source \"writer-a\")\n"
)]
#[case::dc_group(
    AvailabilityConfig::Dc(dc_primary()),
    Some(DcMembership {
        group: "east".to_owned(),
        members: vec![
            DcMember { node: "writer-a".to_owned(), dc: "dc-1".to_owned(), address: "https://a:1".to_owned(), role: DcRole::Writer },
            DcMember { node: "replica-b".to_owned(), dc: "dc-2".to_owned(), address: "https://b:1".to_owned(), role: DcRole::Replica },
        ],
    }),
    "  availability: dc (primary, source \"writer-a\"), group \"east\" (2 members)\n"
)]
#[case::dc_singleton_group(
    AvailabilityConfig::Dc(dc_primary()),
    Some(DcMembership {
        group: "east".to_owned(),
        members: vec![DcMember {
            node: "writer-a".to_owned(),
            dc: "dc-1".to_owned(),
            address: "https://a:1".to_owned(),
            role: DcRole::Writer,
        }],
    }),
    "  availability: dc (primary, source \"writer-a\"), group \"east\" (1 member)\n"
)]
fn test_config_check_reports_effective_availability_without_secrets(
    #[case] availability: AvailabilityConfig,
    #[case] membership: Option<DcMembership>,
    #[case] expected: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_at(&dir);
    config.writer_identity = (!matches!(availability, AvailabilityConfig::None)).then(|| "writer-a".to_owned());
    config.node_identity = matches!(availability, AvailabilityConfig::Ha(_)).then(|| "writer-a".to_owned());
    config.blob = replicated_blob();
    config.availability = availability;
    config.dc_membership = membership;
    let mut out = Vec::new();

    config_check(&config, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(expected), "{text}");
    assert!(!text.contains("top-secret"), "{text}");
}

#[test]
fn test_config_check_surfaces_a_configuration_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_at(&dir);
    config.log.sink = LogSink::File;

    assert!(config_check(&config, &mut Vec::new()).is_err());
}

#[test]
fn test_config_check_propagates_a_write_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);

    assert!(config_check(&config, &mut bounded_output(0)).is_err());
}
