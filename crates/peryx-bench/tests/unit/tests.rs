use std::path::{Path, PathBuf};

use clap::Parser as _;
use mockall::{Sequence, mock, predicate};
use rstest::rstest;

use super::*;

mock! {
    Process {}

    impl Process for Process {
        fn run(&self, command: &CommandSpec) -> std::io::Result<ProcessResult>;
    }
}

mock! {
    Comparator {}

    impl Comparator for Comparator {
        fn compare(&self, base: &Path, head: &Path) -> anyhow::Result<bool>;
    }
}

fn result(success: bool, stdout: impl Into<Vec<u8>>) -> ProcessResult {
    ProcessResult {
        success,
        stdout: stdout.into(),
    }
}

fn machine(directory: &Path) -> MachineProfile {
    MachineProfile {
        path: directory.join("machine.toml"),
        scratch: directory.to_owned(),
        settings: machine::ProfileSettings {
            payload_bytes: 1,
            memory_bytes: 1,
            clients: 1,
            chunk_bytes: 1,
            rounds: 1,
        },
    }
}

fn runner<P: Process, C: Comparator>(process: P, comparator: C, directory: &Path) -> Core<P, C> {
    Core::new(process, comparator, directory.to_owned(), machine(directory))
}

fn cli(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("peryx-bench").chain(args.iter().copied())).unwrap()
}

fn skip_pypi(cli: &mut Cli) {
    cli.skip = [
        "machine",
        "install",
        "throughput",
        "parallel",
        "metadata",
        "load",
        "endpoints",
    ]
    .map(str::to_owned)
    .into();
}

fn skip_oci(cli: &mut Cli) {
    cli.skip = ["machine", "pull", "throughput", "parallel", "endpoints"]
        .map(str::to_owned)
        .into();
}

fn expect_build(process: &mut MockProcess, target: &Path, sequence: &mut Sequence) {
    process
        .expect_run()
        .withf(|command| command.program == "cargo" && !command.capture && command.args[0] == "build")
        .times(1)
        .in_sequence(sequence)
        .return_once(|_| Ok(result(true, Vec::new())));
    let metadata = serde_json::json!({"target_directory": target}).to_string();
    process
        .expect_run()
        .withf(|command| command.program == "cargo" && command.capture && command.args[0] == "metadata")
        .times(1)
        .in_sequence(sequence)
        .return_once(move |_| Ok(result(true, metadata.into_bytes())));
}

fn expect_base_build(process: &mut MockProcess, sequence: &mut Sequence) {
    process
        .expect_run()
        .withf(|command| command.program == "git" && command.args.starts_with(&["worktree".into(), "add".into()]))
        .times(1)
        .in_sequence(sequence)
        .return_once(|_| Ok(result(true, Vec::new())));
    process
        .expect_run()
        .withf(|command| {
            command.program == "cargo" && command.args[0] == "build" && command.cwd.ends_with("bench-base")
        })
        .times(1)
        .in_sequence(sequence)
        .return_once(|_| Ok(result(true, Vec::new())));
}

#[test]
fn cli_defaults_to_the_pypi_comparison() {
    let cli = cli(&[]);
    assert_eq!(cli.ecosystem, Ecosystem::Pypi);
    assert_eq!(cli.rounds, 3);
    assert!(cli.skip.is_empty());
    assert!(cli.only.is_empty());
    assert!(cli.report.is_none());
    assert!(cli.mode.is_none());
}

#[test]
fn cli_parses_the_oci_ab_mode() {
    let cli = cli(&[
        "--ecosystem",
        "oci",
        "--rounds",
        "5",
        "--skip",
        "pull",
        "--only",
        "peryx",
        "--report",
        "result.toml",
        "ab",
        "main",
        "--head-first",
    ]);
    assert_eq!(cli.ecosystem, Ecosystem::Oci);
    assert_eq!(cli.rounds, 5);
    assert_eq!(cli.skip, ["pull"]);
    assert_eq!(cli.only, "peryx");
    assert_eq!(cli.report, Some(PathBuf::from("result.toml")));
    assert_eq!(
        cli.mode,
        Some(Mode::Ab {
            base: "main".into(),
            head_first: true,
        })
    );
}

#[test]
fn system_runner_uses_the_repository_paths() {
    let runner = Runner::system();
    assert_eq!(runner.core.root, report::repo_root());
    assert_eq!(
        runner.core.machine.path,
        report::repo_root().join("site/data/bench/machine.toml")
    );
}

#[test]
fn command_process_captures_output() {
    let output = CommandProcess
        .run(&CommandSpec::output(
            std::env::current_exe().unwrap(),
            std::env::current_dir().unwrap(),
            ["--list"],
        ))
        .unwrap();
    assert!(output.success);
    assert!(!output.stdout.is_empty());
}

#[test]
fn command_process_reports_status() {
    let output = CommandProcess
        .run(&CommandSpec::status(
            std::env::current_exe().unwrap(),
            std::env::current_dir().unwrap(),
            ["--list"],
        ))
        .unwrap();
    assert!(output.success);
    assert!(output.stdout.is_empty());
}

#[test]
fn command_process_preserves_start_errors() {
    let error = CommandProcess
        .run(&CommandSpec::status(
            "peryx-command-that-does-not-exist",
            std::env::current_dir().unwrap(),
            std::iter::empty::<&str>(),
        ))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn report_comparator_accepts_equal_reports() {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().join("base.toml");
    let head = directory.path().join("head.toml");
    std::fs::write(&base, "tables = {}\n").unwrap();
    std::fs::write(&head, "tables = {}\n").unwrap();
    assert!(!ReportComparator.compare(&base, &head).unwrap());
}

#[tokio::test]
async fn pypi_suite_can_disable_every_workload() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(MockProcess::new(), MockComparator::new(), directory.path());
    let mut cli = cli(&[]);
    skip_pypi(&mut cli);
    let context = BenchmarkContext::new(PathBuf::from("peryx"), directory.path().join("report.toml"));
    runner.run_suite(&context, &cli).await.unwrap();
}

#[tokio::test]
async fn oci_suite_can_disable_every_workload() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(MockProcess::new(), MockComparator::new(), directory.path());
    let mut cli = cli(&["--ecosystem", "oci"]);
    skip_oci(&mut cli);
    let context = BenchmarkContext::new(PathBuf::from("peryx"), directory.path().join("report.toml"));
    runner.run_suite(&context, &cli).await.unwrap();
}

#[tokio::test]
async fn machine_profile_supports_smoke_settings() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(MockProcess::new(), MockComparator::new(), directory.path());
    let mut cli = cli(&[]);
    skip_pypi(&mut cli);
    cli.skip.retain(|part| part != "machine");
    let context = BenchmarkContext::new(PathBuf::from("peryx"), directory.path().join("report.toml"));
    runner.run_suite(&context, &cli).await.unwrap();
    assert!(directory.path().join("machine.toml").is_file());
}

#[rstest]
#[case::default(Vec::new())]
#[case::explicit(vec!["vs-rest"])]
#[tokio::test]
async fn comparison_builds_peryx_and_runs_the_suite(#[case] args: Vec<&str>) {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    let mut sequence = Sequence::new();
    expect_build(&mut process, directory.path(), &mut sequence);
    let runner = runner(process, MockComparator::new(), directory.path());
    let mut cli = cli(&args);
    skip_pypi(&mut cli);
    runner.run(cli).await.unwrap();
}

#[test]
fn build_failure_stops_binary_discovery() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    process
        .expect_run()
        .once()
        .return_once(|_| Ok(result(false, Vec::new())));
    let runner = runner(process, MockComparator::new(), directory.path());
    assert_eq!(
        runner.ensure_peryx_built().unwrap_err().to_string(),
        "cargo build failed"
    );
}

#[test]
fn metadata_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    process
        .expect_run()
        .times(2)
        .returning(|command| Ok(result(!command.capture, Vec::new())));
    let runner = runner(process, MockComparator::new(), directory.path());
    assert_eq!(
        runner.ensure_peryx_built().unwrap_err().to_string(),
        "cargo metadata failed"
    );
}

#[rstest]
#[case(b"not json".to_vec(), "expected ident")]
#[case(b"{}".to_vec(), "cargo metadata omitted target_directory")]
fn invalid_metadata_is_rejected(#[case] stdout: Vec<u8>, #[case] message: &str) {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    let mut sequence = Sequence::new();
    process
        .expect_run()
        .once()
        .in_sequence(&mut sequence)
        .return_once(|_| Ok(result(true, Vec::new())));
    process
        .expect_run()
        .once()
        .in_sequence(&mut sequence)
        .return_once(move |_| Ok(result(true, stdout)));
    let runner = runner(process, MockComparator::new(), directory.path());
    assert!(runner.ensure_peryx_built().unwrap_err().to_string().contains(message));
}

#[test]
fn process_start_failure_names_the_program() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    process
        .expect_run()
        .once()
        .return_once(|_| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing")));
    let runner = runner(process, MockComparator::new(), directory.path());
    assert_eq!(
        runner.ensure_peryx_built().unwrap_err().to_string(),
        "cargo did not start"
    );
}

#[test]
fn git_failure_includes_the_command() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    process
        .expect_run()
        .once()
        .return_once(|_| Ok(result(false, Vec::new())));
    let runner = runner(process, MockComparator::new(), directory.path());
    assert_eq!(
        runner.run_git([OsStr::new("status")]).unwrap_err().to_string(),
        "git status failed"
    );
}

#[test]
fn base_build_returns_the_release_binary() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    let mut sequence = Sequence::new();
    expect_base_build(&mut process, &mut sequence);
    let runner = runner(process, MockComparator::new(), directory.path());
    assert_eq!(
        runner.build_base("main").unwrap(),
        directory
            .path()
            .join("target/bench-base/target/release")
            .join(format!("peryx{}", std::env::consts::EXE_SUFFIX))
    );
}

#[test]
fn base_build_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    process
        .expect_run()
        .with(predicate::function(|command: &CommandSpec| command.program == "git"))
        .once()
        .return_once(|_| Ok(result(true, Vec::new())));
    process
        .expect_run()
        .with(predicate::function(|command: &CommandSpec| command.program == "cargo"))
        .once()
        .return_once(|_| Ok(result(false, Vec::new())));
    let runner = runner(process, MockComparator::new(), directory.path());
    assert_eq!(
        runner.build_base("main").unwrap_err().to_string(),
        "base build of main failed"
    );
}

#[test]
fn base_build_start_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    process
        .expect_run()
        .with(predicate::function(|command: &CommandSpec| command.program == "git"))
        .once()
        .return_once(|_| Ok(result(true, Vec::new())));
    process
        .expect_run()
        .with(predicate::function(|command: &CommandSpec| command.program == "cargo"))
        .once()
        .return_once(|_| Err(std::io::Error::other("failed")));
    let runner = runner(process, MockComparator::new(), directory.path());
    assert_eq!(
        runner.build_base("main").unwrap_err().to_string(),
        "cargo did not start"
    );
}

#[test]
fn existing_base_worktree_is_removed() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("target/bench-base")).unwrap();
    let mut process = MockProcess::new();
    process
        .expect_run()
        .withf(|command| command.program == "git" && command.args[1] == "remove")
        .once()
        .return_once(|_| Ok(result(true, Vec::new())));
    let runner = runner(process, MockComparator::new(), directory.path());
    runner.remove_worktree().unwrap();
}

#[test]
fn absent_base_worktree_needs_no_git_command() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(MockProcess::new(), MockComparator::new(), directory.path());
    runner.remove_worktree().unwrap();
}

#[rstest]
#[case(false, false, false)]
#[case(true, false, false)]
#[case(false, true, true)]
#[tokio::test]
async fn ab_comparison_handles_order_and_regressions(
    #[case] head_first: bool,
    #[case] regressed: bool,
    #[case] expect_error: bool,
) {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    let mut sequence = Sequence::new();
    expect_build(&mut process, directory.path(), &mut sequence);
    expect_base_build(&mut process, &mut sequence);
    let mut comparator = MockComparator::new();
    comparator
        .expect_compare()
        .once()
        .return_once(move |_, _| Ok(regressed));
    let runner = runner(process, comparator, directory.path());
    let mut cli = cli(&[]);
    skip_pypi(&mut cli);
    cli.only.clear();
    cli.skip.retain(|part| part != "machine");
    cli.mode = Some(Mode::Ab {
        base: "main".into(),
        head_first,
    });
    let result = runner.run(cli).await;
    assert_eq!(result.is_err(), expect_error);
    if expect_error {
        assert_eq!(result.unwrap_err().to_string(), "peryx regressed against main");
    }
}

#[tokio::test]
async fn ab_preserves_explicit_selection() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    let mut sequence = Sequence::new();
    expect_build(&mut process, directory.path(), &mut sequence);
    expect_base_build(&mut process, &mut sequence);
    let mut comparator = MockComparator::new();
    comparator.expect_compare().once().return_once(|_, _| Ok(false));
    let runner = runner(process, comparator, directory.path());
    let mut cli = cli(&["--only", "other"]);
    skip_pypi(&mut cli);
    runner.ab("main", false, &cli).await.unwrap();
}

#[test]
fn metadata_start_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = MockProcess::new();
    let mut sequence = Sequence::new();
    process
        .expect_run()
        .once()
        .in_sequence(&mut sequence)
        .return_once(|_| Ok(result(true, Vec::new())));
    process
        .expect_run()
        .once()
        .in_sequence(&mut sequence)
        .return_once(|_| Err(std::io::Error::other("failed")));
    let runner = runner(process, MockComparator::new(), directory.path());
    assert_eq!(
        runner.ensure_peryx_built().unwrap_err().to_string(),
        "cargo did not start"
    );
}
