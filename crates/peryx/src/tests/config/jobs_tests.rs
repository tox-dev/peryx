use rstest::rstest;

use super::toml_config;
use crate::config::{Config, JobsMode};

#[test]
fn test_jobs_default_to_local() {
    assert_eq!(Config::default().jobs.mode, JobsMode::Local);
}

#[rstest]
#[case::none("none", JobsMode::None)]
#[case::local("local", JobsMode::Local)]
fn test_jobs_mode_from_toml(#[case] value: &str, #[case] expected: JobsMode) {
    assert_eq!(
        toml_config(&format!("[jobs]\nmode = \"{value}\"\n")).jobs.mode,
        expected
    );
}

#[test]
fn test_an_absent_jobs_table_keeps_the_default() {
    assert_eq!(toml_config("host = \"127.0.0.1\"\n").jobs.mode, JobsMode::Local);
}
