use clap::Command;
use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::report::load;
use peryx_bench_core::suite::{BenchmarkRun, BenchmarkSuite as _};

use super::test_support::http_client;
use super::*;

#[tokio::test]
async fn suite_skips_selected_workloads() {
    let cases: &[(&str, &[&str])] = &[
        ("install", &["install-uv", "install-pip"]),
        ("pip", &["install-pip"]),
        ("throughput", &["throughput"]),
        ("parallel", &["parallel-install"]),
        ("metadata", &["metadata"]),
        ("load", &["load"]),
        ("endpoints", &["endpoints"]),
    ];
    let all = [
        "endpoints",
        "install-pip",
        "install-uv",
        "load",
        "metadata",
        "parallel-install",
        "throughput",
    ];
    for (skip, absent) in cases {
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("report.toml");
        let context = BenchmarkContext::with_scratch("peryx".into(), report.clone(), directory.path().join("scratch"));
        let matches = SUITE.configure(Command::new("bench")).get_matches_from(["bench"]);
        SUITE
            .run(BenchmarkRun {
                context: &context,
                rounds: 0,
                skip: &[(*skip).to_owned()],
                only: "peryx",
                http: &http_client(),
                matches: &matches,
            })
            .await
            .unwrap();
        let tables = load(&report).unwrap().tables;
        let expected = all
            .iter()
            .copied()
            .filter(|name| !absent.contains(name))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            tables
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
    }
}

#[tokio::test]
async fn suite_rejects_unknown_server_selectors() {
    let directory = tempfile::tempdir().unwrap();
    let context = BenchmarkContext::with_scratch(
        "peryx".into(),
        directory.path().join("report.toml"),
        directory.path().join("scratch"),
    );
    let matches = SUITE.configure(Command::new("bench")).get_matches_from(["bench"]);
    let error = SUITE
        .run(BenchmarkRun {
            context: &context,
            rounds: 0,
            skip: &[],
            only: "missing,absent",
            http: &http_client(),
            matches: &matches,
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("unknown server selectors: missing, absent; valid selectors: "));
    for server in servers::all() {
        assert!(error.contains(server.name));
    }
}
