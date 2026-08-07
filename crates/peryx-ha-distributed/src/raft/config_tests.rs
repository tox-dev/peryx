use std::time::Duration;

use openraft::{ConfigError, SnapshotPolicy};

use crate::raft::RaftConfig;

#[test]
fn test_defaults_map_to_a_validated_config() {
    let config = RaftConfig::default().into_openraft("ownership").unwrap();

    assert_eq!(config.cluster_name, "ownership");
    assert_eq!(config.heartbeat_interval, 100);
    assert_eq!(config.election_timeout_min, 300);
    assert_eq!(config.election_timeout_max, 600);
    assert_eq!(config.install_snapshot_timeout, 3_000);
    assert_eq!(config.snapshot_policy, SnapshotPolicy::LogsSinceLast(5_000));
    assert_eq!(config.max_in_snapshot_log_to_keep, 1_000);
}

#[test]
fn test_operator_overrides_map_through() {
    let tuned = RaftConfig {
        heartbeat_interval: Duration::from_millis(250),
        election_timeout_min: Duration::from_secs(1),
        election_timeout_max: Duration::from_secs(2),
        install_snapshot_timeout: Duration::from_secs(30),
        snapshot_logs_since_last: 20_000,
        max_in_snapshot_log_to_keep: 4_000,
    };

    let config = tuned.into_openraft("group").unwrap();

    assert_eq!(config.heartbeat_interval, 250);
    assert_eq!(config.election_timeout_min, 1_000);
    assert_eq!(config.election_timeout_max, 2_000);
    assert_eq!(config.install_snapshot_timeout, 30_000);
    assert_eq!(config.snapshot_policy, SnapshotPolicy::LogsSinceLast(20_000));
    assert_eq!(config.max_in_snapshot_log_to_keep, 4_000);
}

#[test]
fn test_an_empty_election_window_is_rejected() {
    let inverted = RaftConfig {
        election_timeout_min: Duration::from_millis(600),
        election_timeout_max: Duration::from_millis(300),
        ..RaftConfig::default()
    };

    let error = inverted.into_openraft("group").unwrap_err();

    assert!(matches!(*error, ConfigError::ElectionTimeout { min: 600, max: 300 }));
}

#[test]
fn test_an_election_window_below_the_heartbeat_is_rejected() {
    let starved = RaftConfig {
        heartbeat_interval: Duration::from_millis(500),
        election_timeout_min: Duration::from_millis(400),
        election_timeout_max: Duration::from_millis(600),
        ..RaftConfig::default()
    };

    let error = starved.into_openraft("group").unwrap_err();

    assert!(matches!(
        *error,
        ConfigError::ElectionTimeoutLTHeartBeat {
            election_timeout_min: 400,
            heartbeat_interval: 500,
        }
    ));
}

#[test]
fn test_an_absurd_duration_saturates_rather_than_wrapping() {
    let saturated = RaftConfig {
        install_snapshot_timeout: Duration::new(u64::MAX, 0),
        ..RaftConfig::default()
    };

    let config = saturated.into_openraft("group").unwrap();

    assert_eq!(config.install_snapshot_timeout, u64::MAX);
}
