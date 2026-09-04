use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rstest::rstest;

use super::*;

#[test]
fn runner_finish_preserves_success_and_runtime_errors() {
    assert!(finish(Ok(()), &|_| unreachable!()).is_ok());
    assert_eq!(
        finish(Err(anyhow::anyhow!("benchmark failed")), &|_| unreachable!())
            .unwrap_err()
            .to_string(),
        "benchmark failed"
    );
}

#[test]
fn runner_finish_delegates_argument_errors() {
    let exited = std::cell::Cell::new(false);
    finish(
        Err(clap::Error::new(clap::error::ErrorKind::UnknownArgument).into()),
        &|_| {
            exited.set(true);
            Ok(())
        },
    )
    .unwrap();

    assert!(exited.get());
}

#[derive(Clone, Copy, Debug)]
enum ExpectedCommand {
    HeadBuild,
    Metadata,
    WorktreeAdd,
    BaseBuild,
    WorktreeRemove,
    GitStatus,
}

impl ExpectedCommand {
    fn spec(self, root: &Path) -> CommandSpec {
        let worktree = root.join("target/bench-base");
        match self {
            Self::HeadBuild => CommandSpec::status("cargo", root, ["build", "--release", "-p", "peryx"]),
            Self::Metadata => CommandSpec::output("cargo", root, ["metadata", "--no-deps", "--format-version", "1"]),
            Self::WorktreeAdd => CommandSpec::status(
                "git",
                root,
                [
                    OsString::from("worktree"),
                    OsString::from("add"),
                    OsString::from("--detach"),
                    OsString::from("--force"),
                    worktree.into_os_string(),
                    OsString::from("main"),
                ],
            ),
            Self::BaseBuild => CommandSpec::status(
                "cargo",
                &worktree,
                [
                    OsString::from("build"),
                    OsString::from("--release"),
                    OsString::from("-p"),
                    OsString::from("peryx"),
                    OsString::from("--target-dir"),
                    worktree.join("target").into_os_string(),
                ],
            ),
            Self::WorktreeRemove => CommandSpec::status(
                "git",
                root,
                [
                    OsString::from("worktree"),
                    OsString::from("remove"),
                    OsString::from("--force"),
                    worktree.into_os_string(),
                ],
            ),
            Self::GitStatus => CommandSpec::status("git", root, ["status"]),
        }
    }
}

#[derive(Debug)]
enum CommandOutcome {
    Success(Vec<u8>),
    Failure,
    StartError(std::io::ErrorKind),
}

impl CommandOutcome {
    fn respond(self) -> std::io::Result<ProcessResult> {
        match self {
            Self::Success(stdout) => Ok(result(true, stdout)),
            Self::Failure => Ok(result(false, Vec::new())),
            Self::StartError(kind) => Err(std::io::Error::from(kind)),
        }
    }
}

#[derive(Debug)]
struct ExpectedRun {
    command: ExpectedCommand,
    outcome: CommandOutcome,
}

struct ExpectedRuns(Mutex<VecDeque<ExpectedRun>>);

impl Drop for ExpectedRuns {
    fn drop(&mut self) {
        assert!(self.0.get_mut().unwrap().is_empty(), "expected commands were not run");
    }
}

struct ExpectedComparison(Mutex<Option<(PathBuf, PathBuf)>>);

impl Drop for ExpectedComparison {
    fn drop(&mut self) {
        assert!(
            self.0.get_mut().unwrap().is_none(),
            "expected report comparison was not run"
        );
    }
}

struct FailsBase;

struct NoopSuite(&'static str);

struct RecordingSuite;

#[async_trait::async_trait]
impl BenchmarkSuite for NoopSuite {
    fn name(&self) -> &'static str {
        self.0
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
    }

    async fn run(&self, _: BenchmarkRun<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl BenchmarkSuite for RecordingSuite {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command.arg(clap::Arg::new("suite-value").long("suite-value").required(true))
    }

    async fn run(&self, run: BenchmarkRun<'_>) -> anyhow::Result<()> {
        std::fs::write(
            run.context.report_path(),
            format!(
                "{:?}",
                (
                    run.context.peryx_binary(),
                    run.context.report_path(),
                    run.context.scratch(),
                    run.rounds,
                    run.skip,
                    run.only,
                    run.matches.get_one::<String>("suite-value").map(String::as_str),
                )
            ),
        )?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl BenchmarkSuite for FailsBase {
    fn name(&self) -> &'static str {
        "failure"
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
    }

    async fn run(&self, run: BenchmarkRun<'_>) -> anyhow::Result<()> {
        if run.context.report_path().ends_with("bench-base-report.toml") {
            anyhow::bail!("suite failed");
        }
        Ok(())
    }
}

static FAILS_BASE: FailsBase = FailsBase;
static FIRST_SUITE: NoopSuite = NoopSuite("first");
static RECORDING_SUITE: RecordingSuite = RecordingSuite;

#[test]
fn suites_report_their_names() {
    assert_eq!(
        (FIRST_SUITE.name(), RECORDING_SUITE.name(), FAILS_BASE.name()),
        ("first", "recording", "failure")
    );
}

fn process(root: &Path, runs: Vec<ExpectedRun>) -> impl Process + use<> {
    let root = root.to_owned();
    let runs = ExpectedRuns(Mutex::new(runs.into()));
    move |command: &CommandSpec| {
        let expected = runs.0.lock().unwrap().pop_front().expect("unexpected process command");
        assert_eq!(command, &expected.command.spec(&root));
        let result = expected.outcome.respond();
        if matches!(expected.command, ExpectedCommand::WorktreeAdd)
            && result.as_ref().is_ok_and(|result| result.success)
        {
            std::fs::create_dir_all(PathBuf::from(&command.args[4])).unwrap();
        }
        if matches!(expected.command, ExpectedCommand::WorktreeRemove)
            && result.as_ref().is_ok_and(|result| result.success)
        {
            std::fs::remove_dir_all(PathBuf::from(&command.args[3])).unwrap();
        }
        result
    }
}

fn succeeds(command: ExpectedCommand) -> ExpectedRun {
    ExpectedRun {
        command,
        outcome: CommandOutcome::Success(Vec::new()),
    }
}

fn outputs(command: ExpectedCommand, stdout: impl Into<Vec<u8>>) -> ExpectedRun {
    ExpectedRun {
        command,
        outcome: CommandOutcome::Success(stdout.into()),
    }
}

fn fails(command: ExpectedCommand) -> ExpectedRun {
    ExpectedRun {
        command,
        outcome: CommandOutcome::Failure,
    }
}

fn start_fails(command: ExpectedCommand, kind: std::io::ErrorKind) -> ExpectedRun {
    ExpectedRun {
        command,
        outcome: CommandOutcome::StartError(kind),
    }
}

fn comparator(directory: &Path, regressed: bool) -> impl Comparator + use<> {
    let expected = ExpectedComparison(Mutex::new(Some((
        directory.join("target/bench-base-report.toml"),
        directory.join("target/bench-head-report.toml"),
    ))));
    move |base: &Path, head: &Path| {
        let expected = expected.0.lock().unwrap().take().expect("unexpected report comparison");
        assert_eq!((base, head), (expected.0.as_path(), expected.1.as_path()));
        Ok(regressed)
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
        scratch: directory.join(".tox/bench/scratch"),
        settings: machine::ProfileSettings {
            payload_bytes: 1,
            memory_bytes: 1,
            clients: 1,
            chunk_bytes: 1,
            rounds: 1,
        },
    }
}

fn runner(process: impl Process + 'static, comparator: impl Comparator + 'static, directory: &Path) -> Core {
    Core::new(process, comparator, directory.to_owned(), machine(directory))
}

fn cli(args: &[&str]) -> Cli {
    parse(&FIRST_SUITE, std::iter::once("peryx-bench").chain(args.iter().copied())).unwrap()
}

fn failing_cli() -> Cli {
    parse(&FAILS_BASE, ["peryx-bench"]).unwrap()
}

fn skip_machine(cli: &mut Cli) {
    cli.skip = ["machine"].map(str::to_owned).into();
}

fn expect_build(runs: &mut Vec<ExpectedRun>, target: &Path) {
    runs.push(succeeds(ExpectedCommand::HeadBuild));
    runs.push(outputs(
        ExpectedCommand::Metadata,
        serde_json::json!({"target_directory": target}).to_string(),
    ));
}

fn expect_base_build(runs: &mut Vec<ExpectedRun>) {
    runs.push(succeeds(ExpectedCommand::WorktreeAdd));
    runs.push(succeeds(ExpectedCommand::BaseBuild));
}

fn expect_cleanup(runs: &mut Vec<ExpectedRun>) {
    runs.push(succeeds(ExpectedCommand::WorktreeRemove));
}

#[test]
fn cli_uses_the_owner_suite() {
    let cli = cli(&[]);
    assert_eq!(
        (
            cli.suite.name(),
            cli.rounds,
            cli.skip,
            cli.only,
            cli.report,
            cli.scratch,
            cli.mode,
        ),
        ("first", 3, Vec::new(), String::new(), None, None, None)
    );
}

#[test]
fn cli_parses_ab_mode() {
    let cli = cli(&[
        "--rounds",
        "5",
        "--skip",
        "transfer",
        "--only",
        "peryx",
        "--report",
        "result.toml",
        "--scratch",
        "/benchmark-volume",
        "ab",
        "main",
        "--head-first",
    ]);
    assert_eq!(
        (
            cli.suite.name(),
            cli.rounds,
            cli.skip,
            cli.only,
            cli.report,
            cli.scratch,
            cli.mode,
        ),
        (
            "first",
            5,
            vec!["transfer".to_owned()],
            "peryx".to_owned(),
            Some(PathBuf::from("result.toml")),
            Some(PathBuf::from("/benchmark-volume")),
            Some(Mode::Ab {
                base: "main".into(),
                head_first: true,
            }),
        )
    );
}

#[test]
fn system_runner_uses_the_repository_paths() {
    let runner = Runner::system();
    assert_eq!(
        (runner.core.root, runner.core.machine.path, runner.core.machine.scratch,),
        (
            report::repo_root(),
            report::repo_root().join("site/data/benchmark-machine.toml"),
            report::repo_root().join(".tox/bench/scratch"),
        )
    );
}

#[test]
fn command_process_captures_output() {
    let output = CommandProcess
        .run(&CommandSpec::output(
            "rustc",
            std::env::current_dir().unwrap(),
            ["--version"],
        ))
        .unwrap();
    assert!(output.success);
    assert!(String::from_utf8(output.stdout).unwrap().starts_with("rustc "));
}

#[test]
fn command_process_reports_status() {
    let output = CommandProcess
        .run(&CommandSpec::status(
            "rustc",
            std::env::current_dir().unwrap(),
            ["--version"],
        ))
        .unwrap();
    assert_eq!(output, result(true, Vec::new()));
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
async fn configured_suite_runs_without_owner_dependencies() {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report.toml");
    let runner = runner(
        process(directory.path(), Vec::new()),
        ReportComparator,
        directory.path(),
    );
    let cli = parse(
        &RECORDING_SUITE,
        [
            OsString::from("peryx-bench"),
            OsString::from("--rounds"),
            OsString::from("7"),
            OsString::from("--skip"),
            OsString::from("machine"),
            OsString::from("--skip"),
            OsString::from("transfer"),
            OsString::from("--only"),
            OsString::from("peryx"),
            OsString::from("--suite-value"),
            OsString::from("configured"),
        ],
    )
    .unwrap();
    let scratch = directory.path().join("scratch");
    let context = BenchmarkContext::with_scratch(PathBuf::from("peryx"), report.clone(), scratch.clone());
    runner.run_suite(&context, &cli).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&report).unwrap(),
        format!(
            "{:?}",
            (
                Path::new("peryx"),
                report.as_path(),
                scratch.as_path(),
                7,
                ["machine".to_owned(), "transfer".to_owned()].as_slice(),
                "peryx",
                Some("configured"),
            )
        )
    );
}

#[tokio::test]
async fn runner_parses_and_runs_owner_suite() {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report.toml");
    let scratch = directory.path().join("alternate-scratch");
    let mut runs = Vec::new();
    expect_build(&mut runs, directory.path());
    let runner = Runner {
        core: runner(process(directory.path(), runs), ReportComparator, directory.path()),
    };

    runner
        .run_from(
            &RECORDING_SUITE,
            [
                OsString::from("peryx-bench"),
                OsString::from("--report"),
                report.clone().into_os_string(),
                OsString::from("--scratch"),
                scratch.clone().into_os_string(),
                OsString::from("--suite-value"),
                OsString::from("configured"),
            ],
        )
        .await
        .unwrap();

    assert_eq!(
        (
            report.is_file(),
            scratch.is_dir(),
            std::fs::read_dir(&scratch).unwrap().next().is_none(),
            directory.path().join(".tox/bench/scratch").exists(),
            std::fs::read_to_string(&report)
                .unwrap()
                .contains(&format!("{:?}", scratch.as_path())),
        ),
        (true, true, true, false, true)
    );
}

#[tokio::test]
async fn machine_profile_uses_repository_scratch_and_removes_samples() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(
        process(directory.path(), Vec::new()),
        ReportComparator,
        directory.path(),
    );
    let cli = cli(&[]);
    let scratch = directory.path().join(".tox/bench/scratch");
    let context = BenchmarkContext::with_scratch(
        PathBuf::from("peryx"),
        directory.path().join("report.toml"),
        scratch.clone(),
    );
    runner.run_suite(&context, &cli).await.unwrap();
    assert_eq!(
        (
            directory.path().join("machine.toml").is_file(),
            scratch.is_dir(),
            std::fs::read_dir(scratch).unwrap().next().is_none(),
        ),
        (true, true, true)
    );
}

#[rstest]
#[case::default(Vec::new())]
#[case::explicit(vec!["vs-rest"])]
#[tokio::test]
async fn comparison_builds_peryx_and_runs_the_suite(#[case] args: Vec<&str>) {
    let directory = tempfile::tempdir().unwrap();
    let mut runs = Vec::new();
    expect_build(&mut runs, directory.path());
    let runner = Runner {
        core: runner(process(directory.path(), runs), ReportComparator, directory.path()),
    };
    runner
        .run_from(
            &FIRST_SUITE,
            std::iter::once("peryx-bench").chain(["--skip", "machine"]).chain(args),
        )
        .await
        .unwrap();
}

#[test]
fn build_failure_stops_binary_discovery() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(
        process(directory.path(), vec![fails(ExpectedCommand::HeadBuild)]),
        ReportComparator,
        directory.path(),
    );
    assert_eq!(
        runner.ensure_peryx_built().unwrap_err().to_string(),
        "cargo build failed"
    );
}

#[test]
fn metadata_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(
        process(
            directory.path(),
            vec![succeeds(ExpectedCommand::HeadBuild), fails(ExpectedCommand::Metadata)],
        ),
        ReportComparator,
        directory.path(),
    );
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
    let runner = runner(
        process(
            directory.path(),
            vec![
                succeeds(ExpectedCommand::HeadBuild),
                outputs(ExpectedCommand::Metadata, stdout),
            ],
        ),
        ReportComparator,
        directory.path(),
    );
    assert!(runner.ensure_peryx_built().unwrap_err().to_string().contains(message));
}

#[test]
fn process_start_failure_names_the_program() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(
        process(
            directory.path(),
            vec![start_fails(ExpectedCommand::HeadBuild, std::io::ErrorKind::NotFound)],
        ),
        ReportComparator,
        directory.path(),
    );
    assert_eq!(
        runner.ensure_peryx_built().unwrap_err().to_string(),
        "cargo did not start"
    );
}

#[test]
fn git_failure_includes_the_command() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(
        process(directory.path(), vec![fails(ExpectedCommand::GitStatus)]),
        ReportComparator,
        directory.path(),
    );
    assert_eq!(
        runner.run_git([OsStr::new("status")]).unwrap_err().to_string(),
        "git status failed"
    );
}

#[test]
fn base_build_returns_the_release_binary() {
    let directory = tempfile::tempdir().unwrap();
    let mut runs = Vec::new();
    expect_base_build(&mut runs);
    let runner = runner(process(directory.path(), runs), ReportComparator, directory.path());
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
    let runner = runner(
        process(
            directory.path(),
            vec![
                succeeds(ExpectedCommand::WorktreeAdd),
                fails(ExpectedCommand::BaseBuild),
                succeeds(ExpectedCommand::WorktreeRemove),
            ],
        ),
        ReportComparator,
        directory.path(),
    );
    assert_eq!(
        runner.build_base("main").unwrap_err().to_string(),
        "base build of main failed"
    );
}

#[test]
fn base_build_start_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(
        process(
            directory.path(),
            vec![
                succeeds(ExpectedCommand::WorktreeAdd),
                start_fails(ExpectedCommand::BaseBuild, std::io::ErrorKind::Other),
                succeeds(ExpectedCommand::WorktreeRemove),
            ],
        ),
        ReportComparator,
        directory.path(),
    );
    assert_eq!(
        runner.build_base("main").unwrap_err().to_string(),
        "cargo did not start"
    );
}

#[rstest]
#[case::build(fails(ExpectedCommand::BaseBuild), "base build failed; cleanup failed")]
#[case::start(
    start_fails(ExpectedCommand::BaseBuild, std::io::ErrorKind::Other),
    "worktree cleanup also failed"
)]
fn base_build_reports_cleanup_failure(#[case] build: ExpectedRun, #[case] message: &str) {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(
        process(
            directory.path(),
            vec![
                succeeds(ExpectedCommand::WorktreeAdd),
                build,
                fails(ExpectedCommand::WorktreeRemove),
            ],
        ),
        ReportComparator,
        directory.path(),
    );
    assert!(runner.build_base("main").unwrap_err().to_string().contains(message));
}

#[test]
fn base_worktree_add_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(
        process(directory.path(), vec![fails(ExpectedCommand::WorktreeAdd)]),
        ReportComparator,
        directory.path(),
    );
    assert!(
        runner
            .build_base("main")
            .unwrap_err()
            .to_string()
            .starts_with("git worktree add")
    );
}

#[test]
fn existing_base_worktree_is_removed() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("target/bench-base")).unwrap();
    let runner = runner(
        process(directory.path(), vec![succeeds(ExpectedCommand::WorktreeRemove)]),
        ReportComparator,
        directory.path(),
    );
    runner.remove_worktree().unwrap();
}

#[test]
fn base_worktree_remove_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("target/bench-base")).unwrap();
    let runner = runner(
        process(directory.path(), vec![fails(ExpectedCommand::WorktreeRemove)]),
        ReportComparator,
        directory.path(),
    );
    assert!(
        runner
            .remove_worktree()
            .unwrap_err()
            .to_string()
            .starts_with("git worktree remove")
    );
}

#[test]
fn absent_base_worktree_needs_no_git_command() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(
        process(directory.path(), Vec::new()),
        ReportComparator,
        directory.path(),
    );
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
    let mut runs = Vec::new();
    expect_build(&mut runs, directory.path());
    expect_base_build(&mut runs);
    expect_cleanup(&mut runs);
    let runner = runner(
        process(directory.path(), runs),
        comparator(directory.path(), regressed),
        directory.path(),
    );
    let mut cli = cli(&[]);
    skip_machine(&mut cli);
    cli.only.clear();
    cli.skip.retain(|part| part != "machine");
    cli.mode = Some(Mode::Ab {
        base: "main".into(),
        head_first,
    });
    assert_eq!(
        runner.run(cli).await.map_err(|error| error.to_string()),
        if expect_error {
            Err("peryx regressed against main".to_owned())
        } else {
            Ok(())
        }
    );
}

#[tokio::test]
async fn ab_preserves_explicit_selection() {
    let directory = tempfile::tempdir().unwrap();
    let mut runs = Vec::new();
    expect_build(&mut runs, directory.path());
    expect_base_build(&mut runs);
    expect_cleanup(&mut runs);
    let runner = runner(
        process(directory.path(), runs),
        comparator(directory.path(), false),
        directory.path(),
    );
    let mut cli = cli(&["--only", "other"]);
    skip_machine(&mut cli);
    runner.ab("main", false, &cli).await.unwrap();
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn ab_propagates_suite_failures(#[case] head_first: bool) {
    let directory = tempfile::tempdir().unwrap();
    let mut runs = Vec::new();
    expect_build(&mut runs, directory.path());
    expect_base_build(&mut runs);
    expect_cleanup(&mut runs);
    let runner = runner(process(directory.path(), runs), ReportComparator, directory.path());
    let error = runner
        .ab("main", head_first, &failing_cli())
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "suite failed");
    assert!(!directory.path().join("target/bench-base").exists());
}

#[test]
fn comparison_cleanup_removes_reports() {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report");
    std::fs::write(&report, "report").unwrap();
    let runner = runner(
        process(directory.path(), Vec::new()),
        ReportComparator,
        directory.path(),
    );
    runner.cleanup_comparison(std::slice::from_ref(&report)).unwrap();
    assert!(!report.exists());
}

#[test]
fn comparison_cleanup_reports_file_errors() {
    let directory = tempfile::tempdir().unwrap();
    let report = directory.path().join("report");
    std::fs::create_dir(&report).unwrap();
    let runner = runner(
        process(directory.path(), Vec::new()),
        ReportComparator,
        directory.path(),
    );
    assert!(
        runner
            .cleanup_comparison(&[report])
            .unwrap_err()
            .to_string()
            .contains("cannot remove")
    );
}

#[test]
fn metadata_start_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let runner = runner(
        process(
            directory.path(),
            vec![
                succeeds(ExpectedCommand::HeadBuild),
                start_fails(ExpectedCommand::Metadata, std::io::ErrorKind::Other),
            ],
        ),
        ReportComparator,
        directory.path(),
    );
    assert_eq!(
        runner.ensure_peryx_built().unwrap_err().to_string(),
        "cargo did not start"
    );
}

/// A scratch directory that cannot be created has to name the path it failed on. The profile write
/// that follows depends on it, and a bare io error would leave whoever reads the failure guessing
/// which directory the benchmark wanted.
#[tokio::test]
async fn machine_profile_publish_names_the_scratch_it_cannot_create() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("occupied");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let scratch = blocked.join("scratch");

    let error = MachineProfile::system(dir.path())
        .publish(Some(&scratch))
        .await
        .unwrap_err();

    assert_eq!(
        format!("{error}"),
        format!("cannot create benchmark scratch at {}", scratch.display())
    );
}
