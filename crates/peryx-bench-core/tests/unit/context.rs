use super::BenchmarkContext;
use crate::report::{Table, load};

#[test]
fn benchmark_context_owns_artifact_paths() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let binary = directory.path().join("peryx");
    let report = directory.path().join("report.toml");
    let scratch = directory.path().join("scratch");
    let context = BenchmarkContext::with_scratch(binary.clone(), report.clone(), scratch.clone());
    assert_eq!(context.peryx_binary(), binary);
    assert_eq!(context.report_path(), report);
    assert_eq!(context.scratch(), scratch);
}

#[test]
fn benchmark_context_defaults_to_checkout_scratch() {
    let context = BenchmarkContext::new("peryx".into(), "report.toml".into());
    assert_eq!(context.scratch(), std::path::Path::new(".tox/bench/scratch"));
}

#[test]
fn benchmark_context_publishes_to_its_report() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("report.toml");
    let context = BenchmarkContext::new(directory.path().join("peryx"), path.clone());
    context
        .publish(
            "throughput",
            Table {
                label: "Throughput".to_owned(),
                baseline: "peryx".to_owned(),
                parties: Vec::new(),
                rows: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(load(&path).unwrap().tables["throughput"].label, "Throughput");
}
