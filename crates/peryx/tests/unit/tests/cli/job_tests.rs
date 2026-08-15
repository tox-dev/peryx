use std::path::PathBuf;

use rstest::rstest;

use super::parse;
use crate::cli::{Command, JobCommand, JobShowArgs, RuntimeArgs};

#[rstest]
#[case::list(&["peryx", "job", "list", "--data-dir", "/list"], "/list")]
#[case::show(&["peryx", "job", "show", "run-id", "--data-dir", "/show"], "/show")]
#[case::run(&["peryx", "job", "run", "--target", "target", "--data-dir", "/run"], "/run")]
#[case::reindex(&["peryx", "job", "reindex", "--data-dir", "/reindex"], "/reindex")]
#[case::drain(
    &["peryx", "job", "drain", "--authority", "authority", "--data-dir", "/drain"],
    "/drain"
)]
fn test_job_commands_expose_runtime_args(#[case] argv: &[&str], #[case] expected: &str) {
    let Command::Job(command) = parse(argv).command else {
        panic!("expected job command");
    };
    assert_eq!(command.runtime_args().data_dir, Some(PathBuf::from(expected)));
}

#[test]
fn test_parse_job_show_id() {
    assert_eq!(
        parse(&["peryx", "job", "show", "jr_0000000000000001"]).command,
        Command::Job(JobCommand::Show(JobShowArgs {
            runtime: RuntimeArgs::default(),
            id: "jr_0000000000000001".to_owned(),
        }))
    );
}

#[rstest]
#[case::default(&["peryx", "job", "reindex"], peryx_driver::jobs::DEFAULT_SEARCH_REBUILD_CHUNK)]
#[case::explicit(&["peryx", "job", "reindex", "--chunk-size", "50"], 50)]
fn test_parse_job_reindex_chunk_size(#[case] argv: &[&str], #[case] expected: usize) {
    let Command::Job(JobCommand::Reindex { chunk_size, .. }) = parse(argv).command else {
        panic!("expected job reindex command");
    };
    assert_eq!(chunk_size, expected);
}

#[test]
fn test_parse_job_run_defaults() {
    assert_eq!(
        parse(&["peryx", "job", "run", "--target", "packages"]).command,
        Command::Job(JobCommand::Run {
            runtime: RuntimeArgs::default(),
            target: "packages".to_owned(),
            source: None,
            item_limit: None,
            concurrency: None,
            timeout_secs: None,
        })
    );
}

#[test]
fn test_parse_job_run_options() {
    assert_eq!(
        parse(&[
            "peryx",
            "job",
            "run",
            "--target",
            "target",
            "--source",
            "source",
            "--item-limit",
            "10",
            "--concurrency",
            "2",
            "--timeout-secs",
            "30",
        ])
        .command,
        Command::Job(JobCommand::Run {
            runtime: RuntimeArgs::default(),
            target: "target".to_owned(),
            source: Some("source".to_owned()),
            item_limit: Some(10),
            concurrency: Some(2),
            timeout_secs: Some(30),
        })
    );
}

#[test]
fn test_parse_job_drain_authority() {
    assert_eq!(
        parse(&["peryx", "job", "drain", "--authority", "tenant/item"]).command,
        Command::Job(JobCommand::Drain {
            runtime: RuntimeArgs::default(),
            authority: "tenant/item".to_owned(),
        })
    );
}
