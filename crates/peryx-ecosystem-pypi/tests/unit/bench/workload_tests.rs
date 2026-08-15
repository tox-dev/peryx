use clap::Command;
use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::suite::BenchmarkRun;

use super::test_support::http_client;
use super::*;

#[tokio::test]
async fn suite_dispatches_all_workloads_without_rounds() {
    assert_eq!(BENCHMARK_SUITE.name(), "pypi");
    let directory = tempfile::tempdir().unwrap();
    let context = BenchmarkContext::new("peryx".into(), directory.path().join("report.toml"));
    let matches = BENCHMARK_SUITE
        .configure(Command::new("bench"))
        .get_matches_from(["bench"]);
    BENCHMARK_SUITE
        .run(BenchmarkRun {
            context: &context,
            rounds: 0,
            skip: &[],
            only: "direct",
            http: &http_client(),
            matches: &matches,
        })
        .await
        .unwrap();

    let report = std::fs::read_to_string(context.report_path()).unwrap();
    for workload in [
        "install-uv",
        "install-pip",
        "throughput",
        "parallel-install",
        "metadata",
        "load",
    ] {
        assert!(report.contains(workload), "missing report for {workload}");
    }
}

#[tokio::test]
async fn suite_rejects_an_unknown_server() {
    let directory = tempfile::tempdir().unwrap();
    let context = BenchmarkContext::new("peryx".into(), directory.path().join("report.toml"));
    let matches = BENCHMARK_SUITE
        .configure(Command::new("bench"))
        .get_matches_from(["bench"]);

    let error = BENCHMARK_SUITE
        .run(BenchmarkRun {
            context: &context,
            rounds: 0,
            skip: &[],
            only: "missing",
            http: &http_client(),
            matches: &matches,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().starts_with("unknown server selectors: missing;"));
}
