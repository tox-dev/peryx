use std::path::PathBuf;
use std::time::Duration;

use peryx_driver::jobs::{
    DEFAULT_CATALOG_CONCURRENCY, DEFAULT_CATALOG_PROJECTS, DEFAULT_CATALOG_TIMEOUT, DEFAULT_DC_COPY_CONCURRENCY,
    MAINTENANCE_INTERVAL, Schedule, ScheduledJob,
};
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

#[test]
fn test_catalog_schedule_resolves_default_and_explicit_limits() {
    let config = toml_config(
        "[[jobs.schedule]]\njob = \"catalog_sync\"\ninterval_secs = 3600\nrepository = \"pypi\"\n\
         [[jobs.schedule]]\njob = \"catalog_sync\"\ninterval_secs = 60\nrepository = \"pypi\"\nsource = \"public\"\nmax_projects = 9\nconcurrency = 2\ntimeout_secs = 30\n\
         [[index]]\nname = \"pypi\"\n[[index.upstream]]\nname = \"public\"\nurl = \"https://pypi.org/simple/\"\n",
    );
    let ScheduledJob::CatalogSync(defaults) = &config.jobs.schedules[0].job else {
        panic!("expected catalog sync");
    };
    assert_eq!(defaults.repository, "pypi");
    assert_eq!(defaults.max_projects.get(), DEFAULT_CATALOG_PROJECTS);
    assert_eq!(defaults.concurrency.get(), DEFAULT_CATALOG_CONCURRENCY);
    assert_eq!(defaults.timeout, DEFAULT_CATALOG_TIMEOUT);
    let ScheduledJob::CatalogSync(explicit) = &config.jobs.schedules[1].job else {
        panic!("expected catalog sync");
    };
    assert_eq!(explicit.source.as_deref(), Some("public"));
    assert_eq!(explicit.max_projects.get(), 9);
    assert_eq!(explicit.concurrency.get(), 2);
    assert_eq!(explicit.timeout, Duration::from_secs(30));
}

#[rstest]
#[case::cache_fields(
    "job = \"cache_maintenance\"\nrepository = \"pypi\"",
    "accepts no catalog-sync fields"
)]
#[case::missing_repository("job = \"catalog_sync\"", "needs a non-empty `repository`")]
#[case::empty_repository("job = \"catalog_sync\"\nrepository = \" \"", "needs a non-empty `repository`")]
#[case::empty_source(
    "job = \"catalog_sync\"\nrepository = \"pypi\"\nsource = \" \"",
    "`source` must not be empty"
)]
#[case::zero_projects(
    "job = \"catalog_sync\"\nrepository = \"pypi\"\nmax_projects = 0",
    "`max_projects` must be positive"
)]
#[case::too_many_projects(
    "job = \"catalog_sync\"\nrepository = \"pypi\"\nmax_projects = 100001",
    "`max_projects` exceeds the per-run limit"
)]
#[case::zero_concurrency(
    "job = \"catalog_sync\"\nrepository = \"pypi\"\nconcurrency = 0",
    "`concurrency` must be positive"
)]
#[case::too_much_concurrency(
    "job = \"catalog_sync\"\nrepository = \"pypi\"\nconcurrency = 33",
    "`concurrency` exceeds the per-run limit"
)]
#[case::zero_timeout(
    "job = \"catalog_sync\"\nrepository = \"pypi\"\ntimeout_secs = 0",
    "`timeout_secs` must be between 1 and 86400"
)]
#[case::long_timeout(
    "job = \"catalog_sync\"\nrepository = \"pypi\"\ntimeout_secs = 86401",
    "`timeout_secs` must be between 1 and 86400"
)]
#[case::dc_copy_fields(
    "job = \"dc_copy\"\nrepository = \"pypi\"",
    "cross-datacenter copy accepts only `concurrency`"
)]
#[case::dc_copy_zero_concurrency(
    "job = \"dc_copy\"\nconcurrency = 0",
    "cross-datacenter copy `concurrency` must be positive"
)]
#[case::dc_copy_too_much_concurrency(
    "job = \"dc_copy\"\nconcurrency = 65",
    "cross-datacenter copy `concurrency` exceeds the per-pass limit"
)]
#[case::placement_reconcile_fields(
    "job = \"placement_reconcile\"\nrepository = \"pypi\"",
    "placement reconcile accepts no job-specific fields"
)]
#[case::reclamation_fields(
    "job = \"reclamation\"\nconcurrency = 4",
    "reclamation accepts no job-specific fields"
)]
fn test_schedule_rejects_invalid_kind_parameters(#[case] fields: &str, #[case] expected: &str) {
    let partial = config::from_toml(
        PathBuf::from("x.toml"),
        &format!("[[jobs.schedule]]\n{fields}\ninterval_secs = 60\n"),
    )
    .unwrap();

    let error = Config::default().apply(partial).unwrap_err();

    assert!(error.to_string().contains(expected), "{error}");
}

#[test]
fn test_dc_copy_schedule_resolves_default_and_explicit_concurrency() {
    let config = toml_config(
        "[[jobs.schedule]]\njob = \"dc_copy\"\ninterval_secs = 300\n\n\
         [[jobs.schedule]]\njob = \"dc_copy\"\ninterval_secs = 60\nconcurrency = 4\n",
    );
    let ScheduledJob::DcCopy(defaults) = &config.jobs.schedules[0].job else {
        panic!("expected dc copy");
    };
    assert_eq!(defaults.concurrency.get(), DEFAULT_DC_COPY_CONCURRENCY);
    assert_eq!(config.jobs.schedules[0].interval, Duration::from_mins(5));
    let ScheduledJob::DcCopy(explicit) = &config.jobs.schedules[1].job else {
        panic!("expected dc copy");
    };
    assert_eq!(explicit.concurrency.get(), 4);
}

#[test]
fn test_placement_reconcile_schedule_resolves_without_job_fields() {
    let config = toml_config("[[jobs.schedule]]\njob = \"placement_reconcile\"\ninterval_secs = 120\n");

    assert!(matches!(
        config.jobs.schedules[0].job,
        ScheduledJob::PlacementReconcile(_)
    ));
    assert_eq!(config.jobs.schedules[0].interval, Duration::from_mins(2));
}

#[test]
fn test_reclamation_schedule_resolves_without_job_fields() {
    let config = toml_config("[[jobs.schedule]]\njob = \"reclamation\"\ninterval_secs = 300\n");

    assert!(matches!(config.jobs.schedules[0].job, ScheduledJob::Reclamation(_)));
    assert_eq!(config.jobs.schedules[0].interval, Duration::from_mins(5));
}

#[rstest]
#[case::unknown("missing", "", "must name a configured index")]
#[case::hosted("corp", "[[index]]\nname = \"corp\"\nhosted = true\n", "must name a cached index")]
#[case::oci(
    "corp",
    "[[index]]\nname = \"corp\"\necosystem = \"oci\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://registry.example/\"\n",
    "needs an online repository with catalog support"
)]
#[case::offline(
    "corp",
    "[[index]]\nname = \"corp\"\noffline = true\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\n",
    "needs an online repository with catalog support"
)]
#[case::unknown_source(
    "corp",
    "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\n",
    "`source` must name a repository upstream"
)]
fn test_catalog_schedule_rejects_incompatible_repository(
    #[case] repository: &str,
    #[case] indexes: &str,
    #[case] expected: &str,
) {
    let partial = config::from_toml(
        PathBuf::from("x.toml"),
        &format!(
            "{indexes}[[jobs.schedule]]\njob = \"catalog_sync\"\ninterval_secs = 60\nrepository = \"{repository}\"\nsource = \"missing\"\n"
        ),
    )
    .unwrap();

    let error = Config::default().apply(partial).unwrap_err();

    assert!(error.to_string().contains(expected), "{error}");
}
