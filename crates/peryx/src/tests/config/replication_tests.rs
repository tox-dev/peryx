use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use rstest::rstest;

use super::toml_config;
use crate::config::{self, Config, ReplicationConfig, SecretSource};

#[test]
fn test_primary_replication_from_toml() {
    let config = toml_config(
        "[replication]\nrole = \"primary\"\nsource = \"primary-a\"\ntoken_file = \"/run/secrets/replica\"\n",
    );

    assert_eq!(
        config.replication,
        Some(ReplicationConfig::Primary {
            source: "primary-a".to_owned(),
            token: SecretSource::File(PathBuf::from("/run/secrets/replica")),
        })
    );
}

#[test]
fn test_replica_replication_from_toml_uses_defaults() {
    let config =
        toml_config("[replication]\nrole = \"replica\"\nupstream = \"https://primary.example/\"\ntoken = \"secret\"\n");

    assert_eq!(
        config.replication,
        Some(ReplicationConfig::Replica {
            upstream: "https://primary.example/".to_owned(),
            token: SecretSource::Literal("secret".to_owned()),
            poll_interval: Duration::from_secs(1),
            page_size: NonZeroUsize::new(100).unwrap(),
        })
    );
}

#[test]
fn test_replica_replication_from_toml_accepts_runtime_bounds() {
    let config = toml_config(
        "[replication]\nrole = \"replica\"\nupstream = \"https://primary.example/\"\ntoken = \"secret\"\n\
         poll_interval_secs = 30\npage_size = 250\n",
    );

    let Some(ReplicationConfig::Replica {
        poll_interval,
        page_size,
        ..
    }) = config.replication
    else {
        panic!("expected replica configuration");
    };
    assert_eq!(poll_interval, Duration::from_secs(30));
    assert_eq!(page_size, NonZeroUsize::new(250).unwrap());
}

#[rstest]
#[case::empty_source("role = \"primary\"\nsource = \"\"\ntoken = \"secret\"", "primary `source`")]
#[case::empty_upstream("role = \"replica\"\nupstream = \"\"\ntoken = \"secret\"", "replica `upstream`")]
#[case::missing_token("role = \"primary\"\nsource = \"primary-a\"", "role needs")]
#[case::empty_token(
    "role = \"primary\"\nsource = \"primary-a\"\ntoken = \"\"",
    "`token` must not be empty"
)]
#[case::duplicate_token(
    "role = \"primary\"\nsource = \"primary-a\"\ntoken = \"secret\"\ntoken_file = \"secret.txt\"",
    "at most one"
)]
#[case::zero_poll(
    "role = \"replica\"\nupstream = \"https://primary.example\"\ntoken = \"secret\"\npoll_interval_secs = 0",
    "`poll_interval_secs` must be positive"
)]
#[case::zero_page(
    "role = \"replica\"\nupstream = \"https://primary.example\"\ntoken = \"secret\"\npage_size = 0",
    "`page_size` must be positive"
)]
#[case::large_page(
    "role = \"replica\"\nupstream = \"https://primary.example\"\ntoken = \"secret\"\npage_size = 1001",
    "exceeds the primary limit"
)]
fn test_replication_rejects_invalid_configuration(#[case] body: &str, #[case] expected: &str) {
    let partial = config::from_toml(PathBuf::from("x.toml"), &format!("[replication]\n{body}\n")).unwrap();

    let error = Config::default().apply(partial).unwrap_err();

    assert!(error.to_string().contains(expected), "{error}");
}
