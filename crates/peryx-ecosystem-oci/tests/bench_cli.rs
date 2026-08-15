#![cfg(feature = "bench")]

#[test]
fn benchmark_cli_exposes_oci_options() {
    let output = benchmark_command().arg("--help").output().unwrap();

    let help = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        (
            output.status.success(),
            help.contains("--mirror"),
            help.contains("--suite")
        ),
        (true, true, false)
    );
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
    std::process::Command::new(env!("CARGO_BIN_EXE_peryx-bench-oci"))
}
