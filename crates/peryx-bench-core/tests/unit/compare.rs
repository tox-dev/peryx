use std::collections::BTreeMap;

use rstest::rstest;

use super::{Change, against_paths, compare, describe};
use crate::report::{Cell, Party, Report, Row, Table, publish_to};

fn report(value: f64, higher_is_better: bool, network_bound: bool, noisy: bool) -> Report {
    Report {
        tables: BTreeMap::from([(
            "workload".to_owned(),
            Table {
                label: "workload".to_owned(),
                baseline: "peryx".to_owned(),
                parties: vec![Party {
                    name: "peryx".to_owned(),
                    url: String::new(),
                }],
                rows: vec![Row {
                    name: "metric".to_owned(),
                    cells: vec![Cell {
                        text: value.to_string(),
                        ratio: String::new(),
                        tint: String::new(),
                        spread: String::new(),
                        range: String::new(),
                        noisy,
                        outliers: 0,
                        value: Some(value),
                    }],
                    network_bound,
                    higher_is_better,
                }],
            },
        )]),
    }
}

#[rstest]
#[case::slower_latency(1.0, 1.1, false)]
#[case::lower_throughput(100.0, 90.0, true)]
fn compare_detects_regression(#[case] baseline: f64, #[case] head: f64, #[case] higher_is_better: bool) {
    assert!(compare(
        &report(baseline, higher_is_better, false, false),
        &report(head, higher_is_better, false, false)
    ));
}

#[test]
fn compare_ignores_unstable_metrics() {
    let base = report(1.0, false, false, false);
    assert_eq!(
        (
            compare(&base, &report(2.0, false, true, false)),
            compare(&base, &report(2.0, false, false, true)),
            compare(
                &Report {
                    tables: BTreeMap::new()
                },
                &Report {
                    tables: BTreeMap::new()
                }
            ),
        ),
        (false, false, false)
    );
}

#[test]
fn compare_skips_unmatched_report_parts() {
    let complete = report(1.0, false, false, false);
    let mut no_party = report(1.0, false, false, false);
    no_party.tables.get_mut("workload").expect("table exists").parties[0].name = "other".to_owned();
    let mut no_row = report(1.0, false, false, false);
    no_row.tables.get_mut("workload").expect("table exists").rows[0].name = "other".to_owned();
    let mut no_value = report(1.0, false, false, false);
    no_value.tables.get_mut("workload").expect("table exists").rows[0].cells[0].value = None;
    assert_eq!(
        (
            compare(
                &Report {
                    tables: BTreeMap::new()
                },
                &complete
            ),
            compare(&no_party, &complete),
            compare(&no_row, &complete),
            compare(&no_value, &complete),
        ),
        (false, false, false, false)
    );
}

#[test]
fn against_paths_loads_both_reports() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let baseline = directory.path().join("baseline.toml");
    let head = directory.path().join("head.toml");
    for (path, value) in [(&baseline, 1.0), (&head, 1.1)] {
        let table = report(value, false, false, false)
            .tables
            .remove("workload")
            .expect("fixture table");
        publish_to(path, "workload", table).unwrap();
    }
    assert!(against_paths(&baseline, &head).unwrap());
}

#[rstest]
#[case::a_faster_rate_is_not_a_regression(100.0, 200.0, true)]
#[case::an_unchanged_metric_is_not_a_regression(2.0, 2.0, false)]
#[case::exactly_at_the_threshold_is_not_a_regression(1.0, 1.03, false)]
fn compare_holds_metrics_at_or_better_than_the_gate(
    #[case] baseline: f64,
    #[case] head: f64,
    #[case] higher_is_better: bool,
) {
    assert!(!compare(
        &report(baseline, higher_is_better, false, false),
        &report(head, higher_is_better, false, false)
    ));
}

#[test]
fn against_paths_reports_a_clean_comparison() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("report.toml");
    let table = report(1.0, false, false, false)
        .tables
        .remove("workload")
        .expect("fixture table");
    publish_to(&path, "workload", table).expect("the report is published");
    assert!(!against_paths(&path, &path).expect("both reports load"));
}

#[test]
fn describe_renders_the_change_as_a_signed_percentage() {
    assert_eq!(
        describe(&Change {
            table: "workload".to_owned(),
            row: "metric".to_owned(),
            base: 1.0,
            head: 1.1,
            worse: 1.1,
            gated: false,
            reason: "noisy",
        }),
        "workload           metric                                    1.000        1.100    +10.0%  noisy"
    );
}
