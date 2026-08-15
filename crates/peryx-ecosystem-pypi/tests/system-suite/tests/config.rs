use std::path::PathBuf;

use peryx::config::{self, Config, IndexKind};

#[path = "cases/config/jobs.rs"]
mod jobs;
#[path = "cases/config/server.rs"]
mod server;

#[test]
fn default_config_contains_the_pypi_stack() {
    let indexes = Config::default()
        .indexes
        .into_iter()
        .filter(|index| index.ecosystem == peryx_ecosystem_pypi::ECOSYSTEM)
        .map(|index| {
            (
                index.route,
                match index.kind {
                    IndexKind::Cached { .. } => "cached",
                    IndexKind::Hosted { .. } => "hosted",
                    IndexKind::Virtual { .. } => "virtual",
                },
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        indexes,
        [
            ("pypi".to_owned(), "cached"),
            ("hosted".to_owned(), "hosted"),
            ("root/pypi".to_owned(), "virtual"),
        ]
    );
}

#[test]
fn pypi_ecosystem_parses_explicitly_and_as_the_default() {
    let parse = |ecosystem: &str| {
        Config::default()
            .apply(
                config::from_toml(
                    PathBuf::from("config.toml"),
                    &format!(
                        "[[index]]\nname = \"mirror\"\n{ecosystem}[[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\n"
                    ),
                )
                .unwrap(),
            )
            .unwrap()
            .indexes[0]
            .ecosystem
            .clone()
    };

    assert_eq!(
        (parse("ecosystem = \"pypi\"\n"), parse("")),
        (peryx_ecosystem_pypi::ECOSYSTEM, peryx_ecosystem_pypi::ECOSYSTEM)
    );
}

#[test]
fn pypi_operator_job_owns_its_defaults() {
    assert_eq!(
        (
            peryx_ecosystem_pypi::default_job_project_limit(),
            peryx_ecosystem_pypi::default_job_concurrency(),
            peryx_ecosystem_pypi::default_job_timeout_secs(),
        ),
        (10_000, 4, 900)
    );
}
