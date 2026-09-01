use std::time::Duration;

use openraft::{Config, ConfigError, SnapshotPolicy};

use crate::raft::RaftConfig;

#[test]
fn test_defaults_map_to_a_validated_config() {
    let config = RaftConfig::default().into_openraft("ownership").unwrap();

    assert_eq!(
        (
            config.cluster_name,
            config.heartbeat_interval,
            config.election_timeout_min,
            config.election_timeout_max,
            config.install_snapshot_timeout,
            config.snapshot_policy,
            config.max_in_snapshot_log_to_keep,
        ),
        (
            "ownership".to_owned(),
            100,
            300,
            600,
            3_000,
            SnapshotPolicy::LogsSinceLast(5_000),
            1_000,
        )
    );
}

#[test]
#[allow(deprecated)] // OpenRaft still exposes the deprecated snapshot field in Config.
fn test_unowned_defaults_track_openraft() {
    let expected = Config::default();
    let actual = RaftConfig::default().into_openraft("ownership").unwrap();

    assert_eq!(
        (
            actual.send_snapshot_timeout,
            actual.max_payload_entries,
            actual.snapshot_max_chunk_size,
            actual.purge_batch_size,
            actual.enable_tick,
            actual.enable_heartbeat,
            actual.enable_elect,
        ),
        (
            expected.send_snapshot_timeout,
            expected.max_payload_entries,
            expected.snapshot_max_chunk_size,
            expected.purge_batch_size,
            expected.enable_tick,
            expected.enable_heartbeat,
            expected.enable_elect,
        )
    );
}

#[test]
fn test_promotion_requires_the_latest_learner_entry_to_match() {
    let config = RaftConfig::default().into_openraft("ownership").unwrap();

    assert_eq!(config.replication_lag_threshold, 0);
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

    assert_eq!(
        (
            config.heartbeat_interval,
            config.election_timeout_min,
            config.election_timeout_max,
            config.install_snapshot_timeout,
            config.snapshot_policy,
            config.max_in_snapshot_log_to_keep,
        ),
        (250, 1_000, 2_000, 30_000, SnapshotPolicy::LogsSinceLast(20_000), 4_000)
    );
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
