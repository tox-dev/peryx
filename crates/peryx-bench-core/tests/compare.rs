use std::collections::BTreeMap;

use peryx_bench_core::compare::compare;
use peryx_bench_core::report::{Cell, Party, Report, Row, Table};

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

#[test]
fn compare_detects_slower_latency() {
    assert!(compare(
        &report(1.0, false, false, false),
        &report(1.1, false, false, false)
    ));
}

#[test]
fn compare_detects_lower_throughput() {
    assert!(compare(
        &report(100.0, true, false, false),
        &report(90.0, true, false, false)
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
