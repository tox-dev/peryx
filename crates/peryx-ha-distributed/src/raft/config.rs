//! [`RaftNode`](super::RaftNode) validates timing and snapshot settings before starting `OpenRaft`.

use std::time::Duration;

use openraft::{Config, ConfigError, SnapshotPolicy};

/// Prevents an unreachable learner with a short log from passing `OpenRaft`'s blocking readiness check.
const PROMOTION_LAG_THRESHOLD: u64 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaftConfig {
    /// Must stay below [`election_timeout_min`](Self::election_timeout_min).
    pub heartbeat_interval: Duration,
    pub election_timeout_min: Duration,
    /// Upper election-timeout bound; `OpenRaft` randomizes within the window to reduce split votes.
    pub election_timeout_max: Duration,
    pub install_snapshot_timeout: Duration,
    pub snapshot_logs_since_last: u64,
    /// Retains snapshotted entries so lagging followers can avoid a full snapshot transfer.
    pub max_in_snapshot_log_to_keep: u64,
}

impl Default for RaftConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_millis(100),
            election_timeout_min: Duration::from_millis(300),
            election_timeout_max: Duration::from_millis(600),
            install_snapshot_timeout: Duration::from_secs(3),
            snapshot_logs_since_last: 5_000,
            max_in_snapshot_log_to_keep: 1_000,
        }
    }
}

impl RaftConfig {
    /// Converts durations to milliseconds, saturating at [`u64::MAX`].
    ///
    /// # Errors
    /// Returns a boxed [`ConfigError`] when the election window is empty
    /// ([`election_timeout_min`](Self::election_timeout_min) at or above
    /// [`election_timeout_max`](Self::election_timeout_max)) or does not clear the heartbeat
    /// ([`election_timeout_min`](Self::election_timeout_min) at or below
    /// [`heartbeat_interval`](Self::heartbeat_interval)).
    pub fn into_openraft(self, cluster_name: impl Into<String>) -> Result<Config, Box<ConfigError>> {
        #[allow(deprecated)] // OpenRaft still requires its deprecated snapshot field in struct literals.
        Config {
            cluster_name: cluster_name.into(),
            heartbeat_interval: millis(self.heartbeat_interval),
            election_timeout_min: millis(self.election_timeout_min),
            election_timeout_max: millis(self.election_timeout_max),
            install_snapshot_timeout: millis(self.install_snapshot_timeout),
            send_snapshot_timeout: 0,
            max_payload_entries: 300,
            replication_lag_threshold: PROMOTION_LAG_THRESHOLD,
            snapshot_policy: SnapshotPolicy::LogsSinceLast(self.snapshot_logs_since_last),
            snapshot_max_chunk_size: 3 * 1024 * 1024,
            max_in_snapshot_log_to_keep: self.max_in_snapshot_log_to_keep,
            purge_batch_size: 1,
            enable_tick: true,
            enable_heartbeat: true,
            enable_elect: true,
        }
        .validate()
        .map_err(Box::new)
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
