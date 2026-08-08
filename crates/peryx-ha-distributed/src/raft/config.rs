//! Tunable timing and snapshot policy for the ownership consensus group's `OpenRaft` node.
//!
//! Election and heartbeat timing has to match the network the voters run across: too tight and a
//! healthy leader is deposed on a transient delay, too loose and a real failover drags. Snapshot
//! cadence trades log-replay cost on restart against snapshot-build cost under load. Both are an
//! operator's call, so this exposes them as [`RaftConfig`] with defaults and maps them to a validated
//! [`openraft::Config`], failing before the node starts when the timing is inconsistent.

use std::time::Duration;

use openraft::{Config, ConfigError, SnapshotPolicy};

/// The timing and snapshot knobs for the ownership Raft node.
///
/// The defaults suit a low-latency datacenter network: a heartbeat well under the election window so a
/// leader holds its term through ordinary jitter, and a snapshot every few thousand log entries. An
/// operator overrides any field before [`into_openraft`](RaftConfig::into_openraft) maps and validates
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaftConfig {
    /// How often a leader sends heartbeats. Must stay below [`election_timeout_min`](Self::election_timeout_min).
    pub heartbeat_interval: Duration,
    /// The shortest a follower waits without a heartbeat before standing for election.
    pub election_timeout_min: Duration,
    /// The longest that wait. The actual timeout is drawn between the two so voters do not all stand at
    /// once; a wider spread lowers the odds of a split vote.
    pub election_timeout_max: Duration,
    /// How long the leader waits for a follower to accept an install-snapshot RPC before giving up.
    pub install_snapshot_timeout: Duration,
    /// How many log entries accumulate past the last snapshot before the node builds a new one.
    pub snapshot_logs_since_last: u64,
    /// How many already-snapshotted log entries to retain, so a slightly lagging follower catches up
    /// from the log rather than a full snapshot transfer.
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
    /// Map into a validated [`openraft::Config`] for the group named `cluster_name`.
    ///
    /// Durations convert to the millisecond counts `OpenRaft` holds, saturating at [`u64::MAX`] rather
    /// than wrapping. Validation enforces `OpenRaft`'s timing invariants, so an inconsistent setting is
    /// caught here instead of destabilizing the running node.
    ///
    /// # Errors
    /// Returns a boxed [`ConfigError`] when the election window is empty
    /// ([`election_timeout_min`](Self::election_timeout_min) at or above
    /// [`election_timeout_max`](Self::election_timeout_max)) or does not clear the heartbeat
    /// ([`election_timeout_min`](Self::election_timeout_min) at or below
    /// [`heartbeat_interval`](Self::heartbeat_interval)).
    pub fn into_openraft(self, cluster_name: impl Into<String>) -> Result<Config, Box<ConfigError>> {
        Config {
            cluster_name: cluster_name.into(),
            heartbeat_interval: millis(self.heartbeat_interval),
            election_timeout_min: millis(self.election_timeout_min),
            election_timeout_max: millis(self.election_timeout_max),
            install_snapshot_timeout: millis(self.install_snapshot_timeout),
            snapshot_policy: SnapshotPolicy::LogsSinceLast(self.snapshot_logs_since_last),
            max_in_snapshot_log_to_keep: self.max_in_snapshot_log_to_keep,
            ..Config::default()
        }
        .validate()
        .map_err(Box::new)
    }
}

/// A duration as whole milliseconds, saturating at [`u64::MAX`] rather than wrapping on an absurd value.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
