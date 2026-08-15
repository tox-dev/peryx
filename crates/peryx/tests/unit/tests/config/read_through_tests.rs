use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use peryx_ha_distributed::ReconnectPolicy;
use peryx_ha_distributed::read_through::DEFAULT_READ_THROUGH_LIMITS;

use super::toml_config;
use crate::config::{self, Config, ConfigError};

fn dc_toml(read_through: &str) -> String {
    format!(
        "[availability]\nmode = \"dc\"\n\
         [availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"t\"\n{read_through}"
    )
}

fn apply(text: &str) -> Result<Config, ConfigError> {
    config::from_toml(PathBuf::from("x.toml"), text).and_then(|partial| Config::default().apply(partial))
}

#[test]
fn test_absent_table_leaves_read_through_unset() {
    assert_eq!(toml_config(&dc_toml("")).read_through, None);
}

#[test]
fn test_empty_table_resolves_to_the_defaults() {
    let limits = toml_config(&dc_toml("[availability.read-through]\n"))
        .read_through
        .unwrap();
    assert_eq!(limits, DEFAULT_READ_THROUGH_LIMITS);
}

#[test]
fn test_selected_bounds_override_only_their_own_defaults() {
    let fragment = "[availability.read-through]\n\
         concurrency = 3\nchunk-bytes = 1024\nmax-fanout = 2\ntrip-after = 5\ncooldown-secs = 90\n";
    let limits = toml_config(&dc_toml(fragment)).read_through.unwrap();

    assert_eq!(limits.concurrency, NonZeroUsize::new(3).unwrap());
    assert_eq!(limits.chunk_bytes, NonZeroUsize::new(1024).unwrap());
    assert_eq!(limits.max_fanout, NonZeroUsize::new(2).unwrap());
    assert_eq!(limits.circuit.trip_after, 5);
    assert_eq!(limits.circuit.cooldown, Duration::from_secs(90));
    assert_eq!(limits.per_fetch_bytes, DEFAULT_READ_THROUGH_LIMITS.per_fetch_bytes);
    assert_eq!(limits.policy, DEFAULT_READ_THROUGH_LIMITS.policy);
}

#[test]
fn test_retry_sub_table_sets_the_whole_schedule() {
    let fragment = "[availability.read-through.retry]\n\
         base-ms = 50\nmultiplier = 3\nmax-delay-secs = 10\nmax-attempts = 4\n";
    let limits = toml_config(&dc_toml(fragment)).read_through.unwrap();

    assert_eq!(
        limits.policy,
        ReconnectPolicy::new(
            Duration::from_millis(50),
            NonZeroU32::new(3).unwrap(),
            Duration::from_secs(10),
            NonZeroU32::new(4).unwrap(),
        )
    );
}

#[test]
fn test_none_mode_rejects_a_read_through_table() {
    let text = "[availability]\nmode = \"none\"\n[availability.read-through]\nconcurrency = 2\n";
    let err = apply(text).unwrap_err();
    assert!(matches!(err, ConfigError::Availability { .. }));
}

#[test]
fn test_a_zero_bound_is_rejected_at_parse_time() {
    let err = apply(&dc_toml("[availability.read-through]\nconcurrency = 0\n")).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }));
}
