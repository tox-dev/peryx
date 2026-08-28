use std::path::PathBuf;
use std::time::Duration;

use peryx_driver::jobs::{MAINTENANCE_INTERVAL, Schedule, ScheduledJob};
use rstest::rstest;

use super::toml_config;
use crate::config::{self, Config, JobsConfig, JobsMode};

fn distributed_config(mode: &str, schedules: &str) -> Config {
    toml_config(&format!(
        "[availability]\nmode = \"{mode}\"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"b\"\n[availability.listener]\n\n{schedules}"
    ))
}

fn dc_config(job: &str, replica_dc: &str, blob: &str) -> Config {
    toml_config(&format!(
        "writer_identity = \"writer\"\n\
         [availability]\nmode = \"dc\"\ngroup = \"group\"\n\
         [availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"b\"\n\
         [[availability.member]]\nnode = \"writer\"\ndc = \"east\"\naddress = \"https://writer:4460\"\nrole = \"writer\"\n\
         [[availability.member]]\nnode = \"replica\"\ndc = \"{replica_dc}\"\naddress = \"https://replica:4460\"\nrole = \"replica\"\n\
         {blob}\n\
         [[jobs.schedule]]\njob = \"{job}\"\ninterval_secs = 60\n"
    ))
}

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

#[test]
fn test_none_availability_rejects_a_distributed_schedule() {
    let config = Config {
        jobs: JobsConfig {
            mode: JobsMode::Local,
            schedules: vec![Schedule {
                job: peryx_ha_distributed::compile_scheduled_job("dc_copy", &toml::Table::new())
                    .unwrap()
                    .unwrap(),
                interval: Duration::from_secs(1),
            }],
        },
        ..Config::default()
    };

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "jobs schedule [0]: `none` availability cannot schedule distributed jobs"
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
fn test_an_unknown_job_kind_is_rejected_during_classification() {
    let partial = config::from_toml(
        PathBuf::from("x.toml"),
        "[[jobs.schedule]]\njob = \"vacuum\"\ninterval_secs = 60\n",
    )
    .unwrap();
    let error = Config::default().apply(partial).unwrap_err();

    assert!(error.to_string().contains("job"), "{error}");
}

#[rstest]
#[case::cache_fields(
    "job = \"cache_maintenance\"\nrepository = \"example\"",
    "accepts no job-specific fields",
    false
)]
#[case::dc_copy_fields(
    "job = \"dc_copy\"\nrepository = \"example\"",
    "cross-datacenter copy accepts only `concurrency`",
    true
)]
#[case::dc_copy_zero_concurrency(
    "job = \"dc_copy\"\nconcurrency = 0",
    "cross-datacenter copy `concurrency` must be positive",
    true
)]
#[case::dc_copy_negative_concurrency(
    "job = \"dc_copy\"\nconcurrency = -1",
    "cross-datacenter copy `concurrency` must be positive",
    true
)]
#[case::dc_copy_too_much_concurrency(
    "job = \"dc_copy\"\nconcurrency = 65",
    "cross-datacenter copy `concurrency` exceeds the per-pass limit",
    true
)]
#[case::placement_reconcile_fields(
    "job = \"placement_reconcile\"\nrepository = \"example\"",
    "placement reconcile accepts no job-specific fields",
    true
)]
#[case::reclamation_fields(
    "job = \"reclamation\"\nconcurrency = 4",
    "reclamation accepts no job-specific fields",
    true
)]
fn test_schedule_rejects_invalid_kind_parameters(
    #[case] fields: &str,
    #[case] expected: &str,
    #[case] distributed: bool,
) {
    let partial = config::from_toml(
        PathBuf::from("x.toml"),
        &format!(
            "{}[[jobs.schedule]]\n{fields}\ninterval_secs = 60\n",
            if distributed {
                "[availability]\nmode = \"dc\"\n[availability.replication]\nrole = \"primary\"\nsource = \"a\"\ntoken = \"b\"\n\n"
            } else {
                ""
            }
        ),
    )
    .unwrap();

    let error = Config::default().apply(partial).unwrap_err();

    assert!(error.to_string().contains(expected), "{error}");
}

#[rstest]
#[case::dc("dc")]
#[case::ha("ha")]
fn test_dc_copy_schedule_resolves_default_and_explicit_concurrency(#[case] mode: &str) {
    let config = distributed_config(
        mode,
        "[[jobs.schedule]]\njob = \"dc_copy\"\ninterval_secs = 300\n\n\
         [[jobs.schedule]]\njob = \"dc_copy\"\ninterval_secs = 60\nconcurrency = 4\n",
    );
    assert_eq!(config.jobs.schedules[0].job.as_str(), "dc_copy");
    assert_eq!(
        config.jobs.schedules[0].job.settings()["concurrency"].as_integer(),
        Some(8)
    );
    assert_eq!(config.jobs.schedules[0].interval, Duration::from_mins(5));
    assert_eq!(
        config.jobs.schedules[1].job.settings()["concurrency"].as_integer(),
        Some(4)
    );
}

#[rstest]
#[case::dc("dc")]
#[case::ha("ha")]
fn test_placement_reconcile_schedule_resolves_without_job_fields(#[case] mode: &str) {
    let config = distributed_config(
        mode,
        "[[jobs.schedule]]\njob = \"placement_reconcile\"\ninterval_secs = 120\n",
    );

    assert_eq!(config.jobs.schedules[0].job.as_str(), "placement_reconcile");
    assert_eq!(config.jobs.schedules[0].interval, Duration::from_mins(2));
}

#[rstest]
#[case::dc("dc")]
#[case::ha("ha")]
fn test_reclamation_schedule_resolves_without_job_fields(#[case] mode: &str) {
    let config = distributed_config(mode, "[[jobs.schedule]]\njob = \"reclamation\"\ninterval_secs = 300\n");

    assert_eq!(config.jobs.schedules[0].job.as_str(), "reclamation");
    assert_eq!(config.jobs.schedules[0].interval, Duration::from_mins(5));
}

#[rstest]
#[case::dc_copy("dc_copy")]
#[case::placement_reconcile("placement_reconcile")]
#[case::reclamation("reclamation")]
#[case::authority_drain("authority_drain")]
fn test_none_availability_rejects_distributed_schedules(#[case] job: &str) {
    let partial = config::from_toml(
        PathBuf::from("x.toml"),
        &format!("[availability]\nmode = \"none\"\n\n[[jobs.schedule]]\njob = \"{job}\"\ninterval_secs = 60\n"),
    )
    .unwrap();

    assert_eq!(
        Config::default().apply(partial).unwrap_err().to_string(),
        "jobs schedule [0]: `none` availability cannot schedule distributed jobs"
    );
}

#[test]
fn test_dc_copy_schedule_requires_a_remote_datacenter() {
    assert_eq!(
        dc_config("dc_copy", "east", "").validate().unwrap_err().to_string(),
        "jobs schedule [0]: `dc_copy` requires a remote datacenter"
    );
}

#[test]
fn test_distributed_schedule_requires_a_primary() {
    let mut config = dc_config("reclamation", "east", "");
    config.availability = crate::config::AvailabilityConfig::Dc(crate::config::ReplicationConfig::Replica {
        upstream: "https://writer:4460".to_owned(),
        token: crate::config::SecretSource::Literal("token".to_owned()),
        poll_interval: Duration::from_secs(1),
        page_size: std::num::NonZeroUsize::MIN,
    });

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "jobs schedule [0]: distributed jobs require a primary availability node"
    );
}

#[test]
fn test_distributed_schedule_requires_a_local_member_roster() {
    let config = distributed_config("dc", "[[jobs.schedule]]\njob = \"reclamation\"\ninterval_secs = 60\n");

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "jobs schedule [0]: distributed jobs require a local member roster"
    );
}

#[rstest]
#[case::dc_copy("dc_copy", "west")]
#[case::placement_reconcile("placement_reconcile", "east")]
fn test_filesystem_schedule_accepts_an_installed_capability(#[case] job: &str, #[case] replica_dc: &str) {
    dc_config(job, replica_dc, "").validate().unwrap();
}

#[rstest]
#[case::dc_copy("dc_copy")]
#[case::placement_reconcile("placement_reconcile")]
fn test_filesystem_jobs_reject_an_object_store(#[case] job: &str) {
    let config = dc_config(
        job,
        "west",
        "[blob]\nbackend = \"s3\"\nendpoint = \"https://s3.example.com\"\nbucket = \"blobs\"\nregion = \"us-east-1\"",
    );

    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "jobs schedule [0]: copy and placement jobs require filesystem blob storage"
    );
}

#[test]
fn test_reclamation_schedule_accepts_an_object_store() {
    let config = dc_config(
        "reclamation",
        "west",
        "[blob]\nbackend = \"s3\"\nendpoint = \"https://s3.example.com\"\nbucket = \"blobs\"\nregion = \"us-east-1\"",
    );

    config.validate().unwrap();
}
