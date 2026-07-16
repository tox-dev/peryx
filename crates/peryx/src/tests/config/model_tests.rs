use std::path::PathBuf;

use peryx_driver::rate_limit::RateLimitConfig;

use crate::config::{Config, IndexKind, LogConfig, ReplicationConfig, SecretSource};

#[test]
fn test_default_config() {
    let c = Config::default();
    assert_eq!(c.host, "127.0.0.1");
    assert_eq!(c.port, 4433);
    assert_eq!(c.data_dir, PathBuf::from("peryx-data"));
    assert_eq!(c.writer_identity, None);
    assert!(!c.offline);
    assert!(!c.read_only);
    assert_eq!(c.cache_ttl_secs, 300);
    assert_eq!(c.log, LogConfig::default());
    assert_eq!(c.rate_limit, RateLimitConfig::default());
    // One trio per ecosystem: a cache and a hosted store behind a virtual index, for PyPI and OCI.
    let routes: Vec<&str> = c.indexes.iter().map(|index| index.route.as_str()).collect();
    assert_eq!(
        routes,
        ["pypi", "hosted", "root/pypi", "dockerhub", "images", "root/oci"]
    );
    assert!(matches!(&c.indexes[0].kind, IndexKind::Cached { .. }));
    assert!(matches!(&c.indexes[1].kind, IndexKind::Hosted { .. }));
    assert!(matches!(&c.indexes[2].kind, IndexKind::Virtual { upload: Some(target), .. } if target == "hosted"));
    assert_eq!(c.indexes[3].ecosystem, peryx_core::Ecosystem::Oci);
    assert!(matches!(&c.indexes[3].kind, IndexKind::Cached { .. }));
    assert!(matches!(&c.indexes[4].kind, IndexKind::Hosted { .. }));
    assert!(matches!(&c.indexes[5].kind, IndexKind::Virtual { upload: Some(target), .. } if target == "images"));
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

#[rstest::rstest]
#[case::read_only(false)]
#[case::replication(true)]
fn test_config_requires_a_writer_identity_in_replica_mode(#[case] configured_replication: bool) {
    let config = Config {
        read_only: !configured_replication,
        replication: configured_replication.then(|| ReplicationConfig::Replica {
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
fn test_config_accepts_a_replica_writer_identity() {
    let config = Config {
        writer_identity: Some("writer-a".to_owned()),
        read_only: true,
        ..Config::default()
    };

    config.validate().unwrap();
}
