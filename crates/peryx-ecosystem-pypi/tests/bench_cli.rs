#![cfg(feature = "bench")]

#[test]
fn benchmark_cli_exposes_pypi_suite() {
    let output = benchmark_command().arg("--help").output().unwrap();

    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Benchmark PyPI package serving"));
    assert!(!help.contains("--suite"));
}

#[test]
fn benchmark_cli_rejects_unknown_options() {
    let output = benchmark_command().arg("--unknown").output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("unexpected argument '--unknown'")
    );
}

fn benchmark_command() -> std::process::Command {
    std::process::Command::new(peryx_test_support::cargo_binary("peryx-bench-pypi"))
}
