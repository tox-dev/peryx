use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use rstest::rstest;

use super::*;
use crate::config::{AcmeConfig, DcMember, DcMembership, DcRole, SecretSource};

#[rstest]
#[case::http(None, "http")]
#[case::manual(
    Some(TlsConfig::Manual {
        cert: PathBuf::from("/cert.pem"),
        key: PathBuf::from("/key.pem"),
    }),
    "https"
)]
#[case::acme(
    Some(TlsConfig::Acme(AcmeConfig {
        domains: vec!["packages.example".to_owned()],
        contact: "ops@example.test".to_owned(),
        cache_dir: PathBuf::from("/acme"),
        staging: true,
    })),
    "https+acme"
)]
fn test_config_check_reports_the_listener_scheme(#[case] tls: Option<TlsConfig>, #[case] expected: &str) {
    let config = Config {
        tls,
        ..Config::default()
    };
    let mut output = Vec::new();

    config_check(&config, &mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains(&format!("  listen: {expected}://127.0.0.1:4433\n")),
        "{output}"
    );
}

#[test]
fn test_config_check_reports_a_bare_ipv6_listener() {
    let config = Config {
        host: "::1".to_owned(),
        ..Config::default()
    };
    let mut output = Vec::new();

    config_check(&config, &mut output).unwrap();

    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("  listen: http://[::1]:4433\n")
    );
}

#[test]
fn test_config_check_rejects_an_invalid_host() {
    let config = Config {
        host: "not a host".to_owned(),
        ..Config::default()
    };

    assert!(
        config_check(&config, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .starts_with("`host` \"not a host\" with `port` 4433 cannot resolve to a listen address:")
    );
}

#[rstest]
#[case::none(AvailabilityConfig::None, None, "none (single node)")]
#[case::dc_primary(AvailabilityConfig::Dc(primary()), None, "dc (primary, source \"writer-a\")")]
#[case::dc_replica(
    AvailabilityConfig::Dc(replica()),
    None,
    "dc (replica, upstream \"https://primary.example/\")"
)]
#[case::ha_primary(AvailabilityConfig::Ha(primary()), None, "ha (primary, source \"writer-a\")")]
#[case::single_member(
    AvailabilityConfig::Dc(primary()),
    Some(membership(1)),
    "dc (primary, source \"writer-a\"), group \"east\" (1 member)"
)]
#[case::multiple_members(
    AvailabilityConfig::Dc(primary()),
    Some(membership(2)),
    "dc (primary, source \"writer-a\"), group \"east\" (2 members)"
)]
fn test_availability_summary_reports_topology_without_the_token(
    #[case] availability: AvailabilityConfig,
    #[case] dc_membership: Option<DcMembership>,
    #[case] expected: &str,
) {
    let config = Config {
        availability,
        dc_membership,
        ..Config::default()
    };

    let summary = availability_summary(&config);

    assert_eq!(summary, expected);
    assert!(!summary.contains("top-secret"));
}

fn membership(count: usize) -> DcMembership {
    DcMembership {
        group: "east".to_owned(),
        members: (0..count)
            .map(|index| DcMember {
                node: format!("node-{index}"),
                dc: format!("dc-{index}"),
                address: format!("http://127.0.0.1:90{index:02}"),
                role: if index == 0 { DcRole::Writer } else { DcRole::Replica },
            })
            .collect(),
    }
}

fn primary() -> ReplicationConfig {
    ReplicationConfig::Primary {
        source: "writer-a".to_owned(),
        token: SecretSource::Literal("top-secret".to_owned()),
    }
}

fn replica() -> ReplicationConfig {
    ReplicationConfig::Replica {
        upstream: "https://primary.example/".to_owned(),
        token: SecretSource::Literal("top-secret".to_owned()),
        poll_interval: Duration::from_secs(1),
        page_size: NonZeroUsize::MIN,
    }
}
