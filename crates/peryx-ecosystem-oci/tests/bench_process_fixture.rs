#[cfg(all(feature = "bench", unix))]
#[test]
fn benchmark_process_fixture_preserves_credentials() {
    assert!(
        std::process::Command::new(peryx_test_support::cargo_binary("peryx-oci-bench-process-fixture"))
            .status()
            .unwrap()
            .success()
    );
}
