#[cfg(all(feature = "bench", unix))]
#[test]
fn benchmark_process_fixture_preserves_credentials() {
    assert!(
        std::process::Command::new(env!("CARGO_BIN_EXE_peryx-oci-bench-process-fixture"))
            .status()
            .unwrap()
            .success()
    );
}
