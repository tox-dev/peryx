use std::path::PathBuf;
use std::time::Duration;

use peryx_driver::jobs::{MAINTENANCE_INTERVAL, Schedule, ScheduledJob};
use rstest::rstest;

use super::toml_config;
use crate::config::{self, Config, JobsMode};

#[test]
fn test_jobs_default_to_local() {
    assert_eq!(Config::default().jobs.mode, JobsMode::Local);
}

#[test]
fn test_jobs_default_to_the_built_in_cache_maintenance_schedule() {
    assert_eq!(
        Config::default().jobs.schedules,
        vec![Schedule {
            job: ScheduledJob::CacheMaintenance,
            interval: MAINTENANCE_INTERVAL,
        }]
    );
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

#[test]
fn test_a_schedule_array_replaces_the_default_set() {
    let config = toml_config(
        "[[jobs.schedule]]\njob = \"cache_maintenance\"\ninterval_secs = 300\n\n\
         [[jobs.schedule]]\njob = \"cache_maintenance\"\ninterval_secs = 30\n",
    );

    assert_eq!(
        config.jobs.schedules,
        vec![
            Schedule {
                job: ScheduledJob::CacheMaintenance,
                interval: Duration::from_mins(5),
            },
            Schedule {
                job: ScheduledJob::CacheMaintenance,
                interval: Duration::from_secs(30),
            },
        ]
    );
}

#[test]
fn test_a_schedule_keeps_the_configured_mode() {
    let config = toml_config(
        "[jobs]\nmode = \"local\"\n\n[[jobs.schedule]]\njob = \"cache_maintenance\"\ninterval_secs = 120\n",
    );

    assert_eq!(config.jobs.mode, JobsMode::Local);
    assert_eq!(config.jobs.schedules.len(), 1);
    assert_eq!(config.jobs.schedules[0].interval, Duration::from_mins(2));
}

#[test]
fn test_a_zero_interval_is_rejected_with_its_schedule_index() {
    let partial = config::from_toml(
        PathBuf::from("x.toml"),
        "[[jobs.schedule]]\njob = \"cache_maintenance\"\ninterval_secs = 300\n\n\
         [[jobs.schedule]]\njob = \"cache_maintenance\"\ninterval_secs = 0\n",
    )
    .unwrap();

    let error = Config::default().apply(partial).unwrap_err();

    assert_eq!(error.to_string(), "jobs schedule [1]: `interval_secs` must be positive");
}

#[test]
fn test_an_unknown_job_kind_is_rejected_at_parse_time() {
    let error = config::from_toml(
        PathBuf::from("x.toml"),
        "[[jobs.schedule]]\njob = \"vacuum\"\ninterval_secs = 60\n",
    )
    .unwrap_err();

    assert!(error.to_string().contains("job"), "{error}");
}
