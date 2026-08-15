use std::num::NonZeroUsize;
use std::time::Duration;

use std::path::PathBuf;

use rstest::rstest;

use super::toml_config;
use crate::config::{
    self, AvailabilityConfig, AvailabilityListenerTls, AvailabilityMode, Config, ReplicationConfig, SecretSource,
};

const DC_PRIMARY: &str =
    "[availability]\nmode = \"dc\"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"b\"\n";

#[test]
fn test_omitted_table_and_explicit_none_resolve_alike() {
    let omitted = Config::default().availability;
    let explicit = toml_config("[availability]\nmode = \"none\"\n").availability;

    assert_eq!(omitted, AvailabilityConfig::None);
    assert_eq!(explicit, AvailabilityConfig::None);
}

#[test]
fn test_empty_table_selects_none() {
    assert_eq!(toml_config("[availability]\n").availability, AvailabilityConfig::None);
}

#[rstest]
#[case::none(AvailabilityConfig::None, AvailabilityMode::None, false)]
#[case::dc(AvailabilityConfig::Dc(primary()), AvailabilityMode::Dc, true)]
#[case::ha(AvailabilityConfig::Ha(primary()), AvailabilityMode::Ha, true)]
fn test_availability_accessors_report_mode_and_topology(
    #[case] availability: AvailabilityConfig,
    #[case] mode: AvailabilityMode,
    #[case] carries_role: bool,
) {
    assert_eq!(availability.mode(), mode);
    assert_eq!(availability.replication().is_some(), carries_role);
}

#[rstest]
#[case::none_with_role(
    "[availability]\nmode = \"none\"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"b\"\n",
    "`none` mode configures no replication"
)]
#[case::role_without_mode(
    "[availability]\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"b\"\n",
    "`none` mode configures no replication"
)]
#[case::dc_without_role("[availability]\nmode = \"dc\"\n", "`dc` and `ha` modes need")]
#[case::ha_without_role("[availability]\nmode = \"ha\"\n", "`dc` and `ha` modes need")]
#[case::ha_without_listener(
    "[availability]\nmode = \"ha\"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"b\"\n",
    "`ha` mode requires `[availability.listener]`"
)]
#[case::unknown_mode("[availability]\nmode = \"quorum\"\n", "unknown variant")]
fn test_availability_rejects_impossible_combinations(#[case] text: &str, #[case] expected: &str) {
    let error = config::from_toml("x.toml".into(), text)
        .and_then(|partial| Config::default().apply(partial))
        .unwrap_err();

    assert!(error.to_string().contains(expected), "{error}");
}

fn primary() -> ReplicationConfig {
    ReplicationConfig::Primary {
        source: "primary-a".to_owned(),
        token: SecretSource::Literal("secret".to_owned()),
    }
}

#[test]
fn test_dc_and_ha_carry_distinct_topology() {
    let replica = || ReplicationConfig::Replica {
        upstream: "https://primary.example/".to_owned(),
        token: SecretSource::Literal("secret".to_owned()),
        poll_interval: Duration::from_secs(1),
        page_size: NonZeroUsize::MIN,
    };

    assert_ne!(AvailabilityConfig::Dc(replica()), AvailabilityConfig::Ha(replica()));
}

#[test]
fn test_listener_absent_by_default_and_under_dc_without_table() {
    assert!(Config::default().availability_listener.is_none());
    assert!(toml_config(DC_PRIMARY).availability_listener.is_none());
}

#[test]
fn test_listener_defaults_to_a_private_loopback_bind() {
    let listener = toml_config(&format!("{DC_PRIMARY}[availability.listener]\n"))
        .availability_listener
        .expect("dc listener");

    assert!(listener.bind.ip().is_loopback());
    assert_eq!(listener.bind.port(), 4460);
    assert!(listener.tls.is_none());
    assert!(!listener.allow_remote_plaintext);
}

#[test]
fn test_listener_accepts_an_explicit_loopback_bind() {
    let listener = toml_config(&format!(
        "{DC_PRIMARY}[availability.listener]\nbind = \"127.0.0.1:9100\"\n"
    ))
    .availability_listener
    .expect("dc listener");

    assert_eq!(listener.bind.port(), 9100);
}

#[test]
fn test_listener_remote_bind_opts_into_plaintext() {
    let listener = toml_config(&format!(
        "{DC_PRIMARY}[availability.listener]\nbind = \"0.0.0.0:9100\"\nallow-remote-plaintext = true\n"
    ))
    .availability_listener
    .expect("dc listener");

    assert!(!listener.bind.ip().is_loopback());
    assert!(listener.allow_remote_plaintext);
}

#[test]
fn test_listener_remote_bind_terminates_tls() {
    let listener = toml_config(&format!(
        "{DC_PRIMARY}[availability.listener]\nbind = \"0.0.0.0:9100\"\n[availability.listener.tls]\ncert = \"/c.pem\"\nkey = \"/k.pem\"\n"
    ))
    .availability_listener
    .expect("dc listener");

    assert_eq!(
        listener.tls,
        Some(AvailabilityListenerTls {
            cert: PathBuf::from("/c.pem"),
            key: PathBuf::from("/k.pem"),
        })
    );
}

#[rstest]
#[case::none_opens_none(
    "[availability]\nmode = \"none\"\n[availability.listener]\n",
    "`none` mode opens no availability listener"
)]
#[case::remote_plaintext_refused(
    &format!("{DC_PRIMARY}[availability.listener]\nbind = \"0.0.0.0:9100\"\n"),
    "non-loopback availability listener needs"
)]
#[case::invalid_bind(
    &format!("{DC_PRIMARY}[availability.listener]\nbind = \"not-an-address\"\n"),
    "must be a `host:port` socket address"
)]
#[case::tls_needs_cert_and_key(
    &format!("{DC_PRIMARY}[availability.listener]\nbind = \"0.0.0.0:9100\"\n[availability.listener.tls]\ncert = \"/c.pem\"\n"),
    "needs both `cert` and `key`"
)]
fn test_listener_rejects_unsafe_or_malformed_tables(#[case] text: &str, #[case] expected: &str) {
    let error = config::from_toml(PathBuf::from("x.toml"), text)
        .and_then(|partial| Config::default().apply(partial))
        .unwrap_err();

    assert!(error.to_string().contains(expected), "{error}");
}

#[test]
fn test_write_ack_defaults_to_local_durability_under_none() {
    let write_ack = Config::default().write_ack;
    assert_eq!(write_ack.policy, peryx_ha_distributed::DurabilityPolicy::Local);
    assert_eq!(write_ack.deadline, Duration::from_secs(5));

    let explicit = toml_config("[availability]\nmode = \"none\"\n").write_ack;
    assert_eq!(explicit.policy, peryx_ha_distributed::DurabilityPolicy::Local);
}

#[test]
fn test_write_ack_defaults_to_a_majority_quorum_under_dc() {
    let write_ack = toml_config(DC_PRIMARY).write_ack;
    assert_eq!(write_ack.policy, peryx_ha_distributed::DurabilityPolicy::Majority);
    assert_eq!(write_ack.deadline, Duration::from_secs(5));
}

#[rstest]
#[case::local("local", peryx_ha_distributed::DurabilityPolicy::Local)]
#[case::majority("majority", peryx_ha_distributed::DurabilityPolicy::Majority)]
#[case::everywhere("everywhere", peryx_ha_distributed::DurabilityPolicy::Everywhere)]
fn test_write_ack_resolves_the_configured_quorum(
    #[case] policy: &str,
    #[case] expected: peryx_ha_distributed::DurabilityPolicy,
) {
    let text = format!("{DC_PRIMARY}[availability.write_ack]\npolicy = \"{policy}\"\ndeadline-secs = 30\n");
    let write_ack = toml_config(&text).write_ack;
    assert_eq!(write_ack.policy, expected);
    assert_eq!(write_ack.deadline, Duration::from_secs(30));
}

#[rstest]
#[case::policy_under_none(
    "[availability]\nmode = \"none\"\n[availability.write_ack]\npolicy = \"majority\"\n",
    "acknowledges from local durability"
)]
#[case::zero_deadline(
    "[availability]\nmode = \"dc\"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"b\"\n[availability.write_ack]\ndeadline-secs = 0\n",
    "must be positive"
)]
fn test_write_ack_rejects_impossible_combinations(#[case] text: &str, #[case] expected: &str) {
    let error = config::from_toml("x.toml".into(), text)
        .and_then(|partial| Config::default().apply(partial))
        .unwrap_err();

    assert!(error.to_string().contains(expected), "{error}");
}
