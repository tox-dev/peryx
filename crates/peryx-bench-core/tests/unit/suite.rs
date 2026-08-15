use std::sync::atomic::{AtomicUsize, Ordering};

use clap::{Arg, ArgAction, Command};

use super::*;

struct TestSuite;

#[async_trait::async_trait]
impl BenchmarkSuite for TestSuite {
    fn name(&self) -> &'static str {
        "test"
    }

    fn configure(&self, command: Command) -> Command {
        command.arg(Arg::new("suite-option").long("suite-option").action(ArgAction::SetTrue))
    }

    async fn run(&self, _: BenchmarkRun<'_>) -> anyhow::Result<()> {
        RUNS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

static SUITE: TestSuite = TestSuite;
static RUNS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn suite_configures_the_parser() {
    assert_eq!(
        (
            SUITE.name(),
            SUITE
                .configure(Command::new("bench"))
                .try_get_matches_from(["bench", "--suite-option"])
                .unwrap()
                .get_flag("suite-option"),
        ),
        ("test", true)
    );
}

#[tokio::test]
async fn suite_runs_through_the_contract() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let context =
        crate::context::BenchmarkContext::new(directory.path().join("peryx"), directory.path().join("report.toml"));
    let matches = Command::new("bench").get_matches_from(["bench"]);
    let http = crate::servers::http_client().expect("HTTP client builds");
    let before = RUNS.load(Ordering::Relaxed);
    SUITE
        .run(BenchmarkRun {
            context: &context,
            rounds: 1,
            skip: &[],
            only: "test",
            http: &http,
            matches: &matches,
        })
        .await
        .expect("suite runs");
    assert_eq!(RUNS.load(Ordering::Relaxed), before + 1);
}
