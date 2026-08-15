use std::path::PathBuf;

use peryx::config::{self, Config};
use peryx_driver::jobs::ScheduledJob;
use peryx_ecosystem_pypi::{default_job_concurrency, default_job_project_limit, default_job_timeout_secs};
use rstest::rstest;

#[test]
fn catalog_schedule_resolves_default_and_explicit_limits() {
    let config = toml_config(
        "[[jobs.schedule]]\njob = \"catalog_sync\"\ninterval_secs = 3600\nrepository = \"pypi\"\n\
         [[jobs.schedule]]\njob = \"catalog_sync\"\ninterval_secs = 60\nrepository = \"pypi\"\nsource = \"public\"\nmax_projects = 9\nconcurrency = 2\ntimeout_secs = 30\n\
         [[index]]\nname = \"pypi\"\n[[index.upstream]]\nname = \"public\"\nurl = \"https://pypi.org/simple/\"\n",
    );
    assert!(matches!(
        &config.jobs.schedules[0].job,
        ScheduledJob::Plugin(job)
            if job.settings()["repository"].as_str() == Some("pypi")
                && job.settings()["max_projects"].as_integer()
                    == Some(i64::try_from(default_job_project_limit()).unwrap())
                && job.settings()["concurrency"].as_integer()
                    == Some(i64::try_from(default_job_concurrency()).unwrap())
                && job.settings()["timeout_secs"].as_integer()
                    == Some(i64::try_from(default_job_timeout_secs()).unwrap())
    ));
    assert!(matches!(
        &config.jobs.schedules[1].job,
        ScheduledJob::Plugin(job)
            if job.settings()["source"].as_str() == Some("public")
                && job.settings()["max_projects"].as_integer() == Some(9)
                && job.settings()["concurrency"].as_integer() == Some(2)
                && job.settings()["timeout_secs"].as_integer() == Some(30)
    ));
}

#[rstest]
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
#[case::unknown_field(
    "job = \"catalog_sync\"\nrepository = \"pypi\"\nunexpected = true",
    "unknown field `unexpected`"
)]
fn catalog_schedule_rejects_invalid_parameters(#[case] fields: &str, #[case] expected: &str) {
    let partial = config::from_toml(
        PathBuf::from("x.toml"),
        &format!("[[jobs.schedule]]\n{fields}\ninterval_secs = 60\n"),
    )
    .unwrap();
    let error = Config::default().apply(partial).unwrap_err();

    assert!(error.to_string().contains(expected), "{error}");
}

#[rstest]
#[case::unknown("missing", "", "must name a configured index")]
#[case::hosted("corp", "[[index]]\nname = \"corp\"\nhosted = true\n", "must name a cached index")]
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
fn catalog_schedule_rejects_incompatible_repository(
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

fn toml_config(source: &str) -> Config {
    Config::default()
        .apply(config::from_toml(PathBuf::from("test.toml"), source).unwrap())
        .unwrap()
}
