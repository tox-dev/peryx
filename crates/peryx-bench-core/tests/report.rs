use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use peryx_bench_core::report::{
    Absent, Cell, Metric, Party, Report, Row, Table, anchor, baseline, cost_rows, cost_rows_per_request, load,
    network_row, peryx_binary, publish_to, repo_root, report_path, row, set_peryx_binary, summarize, table,
};
use peryx_bench_core::servers::Server;
use peryx_bench_core::stats::Summary;
use peryx_bench_core::usage::Cost;

fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

fn probe(url: &str) -> String {
    url.to_owned()
}

fn command(_: u16, _: &Path) -> Command {
    Command::new("true")
}

fn server(name: &'static str) -> Server {
    Server {
        name,
        homepage: "https://example.invalid",
        base_url,
        probe,
        command: Some(command),
        setup: None,
        teardown: None,
    }
}

#[test]
fn report_store_merges_tables() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("nested/report.toml");
    for name in ["first", "second"] {
        publish_to(
            &path,
            name,
            Table {
                label: name.to_owned(),
                baseline: "peryx".to_owned(),
                parties: Vec::new(),
                rows: Vec::new(),
            },
        )
        .expect("table is published");
    }
    assert_eq!(
        load(&path).expect("report loads"),
        Report {
            tables: BTreeMap::from([
                (
                    "first".to_owned(),
                    Table {
                        label: "first".to_owned(),
                        baseline: "peryx".to_owned(),
                        parties: Vec::new(),
                        rows: Vec::new(),
                    },
                ),
                (
                    "second".to_owned(),
                    Table {
                        label: "second".to_owned(),
                        baseline: "peryx".to_owned(),
                        parties: Vec::new(),
                        rows: Vec::new(),
                    },
                ),
            ]),
        }
    );
}

#[test]
fn report_store_rejects_invalid_documents() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("report.toml");
    std::fs::write(&path, "tables = 1").expect("fixture writes");
    let error = publish_to(
        &path,
        "new",
        Table {
            label: String::new(),
            baseline: String::new(),
            parties: Vec::new(),
            rows: Vec::new(),
        },
    )
    .expect_err("invalid tables value fails");
    assert!(error.to_string().contains("`tables` is not a TOML table"));
}

#[test]
fn report_load_describes_missing_and_malformed_files() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let missing = directory.path().join("missing.toml");
    assert!(
        load(&missing)
            .expect_err("missing report fails")
            .to_string()
            .contains("cannot read")
    );
    let malformed = directory.path().join("malformed.toml");
    std::fs::write(&malformed, "=").expect("fixture writes");
    assert!(
        load(&malformed)
            .expect_err("malformed report fails")
            .to_string()
            .contains("not a valid report")
    );
}

#[test]
fn rows_format_measurements_and_absence() {
    let values = summarize(&[vec![0.000_5], vec![0.5], Vec::new()]);
    let seconds = row("latency", &values, 1, Metric::Seconds, Absent::Failed);
    let rate = network_row("rate", &values, 1, Metric::Rate("req/s"), Absent::NoServer);
    assert_eq!(
        (
            seconds.cells.iter().map(|cell| cell.text.as_str()).collect::<Vec<_>>(),
            seconds.cells.iter().map(|cell| cell.ratio.as_str()).collect::<Vec<_>>(),
            seconds.higher_is_better,
            rate.network_bound,
            rate.higher_is_better,
        ),
        (
            vec!["500 µs", "500 ms", "error"],
            vec!["0.00x", "≈1.00x", "n/a"],
            false,
            true,
            true,
        )
    );
}

#[test]
fn rows_format_ranges_units_and_missing_baselines() {
    let values = summarize(&[
        vec![61.0, 62.0],
        vec![1.0, 1.5],
        vec![0.001, 0.002],
        vec![1_234.0, 1_235.0],
    ]);
    let minutes = row("minutes", &values[..1], 9, Metric::Seconds, Absent::Failed);
    let seconds = row("seconds", &values[1..2], 0, Metric::Seconds, Absent::Failed);
    let millis = row("millis", &values[2..3], 0, Metric::Seconds, Absent::Failed);
    let amount = row("amount", &values[3..], 0, Metric::Amount("items"), Absent::Failed);
    assert_eq!(
        (
            minutes.cells[0].text.as_str(),
            minutes.cells[0].ratio.as_str(),
            minutes.cells[0].spread.as_str(),
            minutes.cells[0].range.as_str(),
            seconds.cells[0].text.as_str(),
            millis.cells[0].text.as_str(),
            amount.cells[0].text.as_str(),
        ),
        (
            "1m 01.5s",
            "-",
            "±1%",
            "1m 01.0s..1m 02.0s",
            "1.2 s",
            "1.5 ms",
            "1,235 items"
        )
    );
}

#[test]
fn server_selection_and_table_follow_named_parties() {
    let servers = [server("other"), server("direct"), server("peryx")];
    assert_eq!((baseline(&servers), anchor(&servers)), (1, 2));
    assert_eq!(
        table("label", &servers, 1, Vec::new()),
        Table {
            label: "label".to_owned(),
            baseline: "direct".to_owned(),
            parties: vec![
                Party {
                    name: "other".to_owned(),
                    url: "https://example.invalid".to_owned(),
                },
                Party {
                    name: "direct".to_owned(),
                    url: "https://example.invalid".to_owned(),
                },
                Party {
                    name: "peryx".to_owned(),
                    url: "https://example.invalid".to_owned(),
                },
            ],
            rows: Vec::new(),
        }
    );
}

#[test]
fn server_selection_falls_back_to_first_party() {
    let servers = [server("first"), server("second")];
    assert_eq!((baseline(&servers), anchor(&servers)), (0, 0));
}

#[test]
fn resource_rows_price_server_and_request_costs() {
    let servers = [server("direct"), server("peryx")];
    let costs = [
        None,
        Some(vec![Cost {
            cpu_seconds: 2.0,
            peak_rss_bytes: 4_000_000,
        }]),
    ];
    let rows = cost_rows(&servers, &costs);
    let request_rows = cost_rows_per_request(&servers, &costs, &[None, Some(vec![2_000])]);
    assert_eq!(
        (
            rows.iter().map(|row| row.cells[1].text.as_str()).collect::<Vec<_>>(),
            request_rows
                .iter()
                .map(|row| row.cells[1].text.as_str())
                .collect::<Vec<_>>(),
        ),
        (vec!["2.0 s", "4 MB"], vec!["1.0 s", "4 MB"])
    );
}

#[test]
fn public_report_types_preserve_serialized_fields() {
    let row = Row {
        name: "metric".to_owned(),
        cells: vec![Cell {
            text: "1".to_owned(),
            ratio: "1x".to_owned(),
            tint: "par".to_owned(),
            spread: String::new(),
            range: String::new(),
            noisy: false,
            outliers: 0,
            value: Some(1.0),
        }],
        network_bound: false,
        higher_is_better: false,
    };
    assert_eq!(row.cells[0].value, Some(1.0));
}

#[test]
fn summaries_keep_each_partys_distribution() {
    assert_eq!(
        summarize(&[vec![1.0], Vec::new()]),
        vec![
            Some(Summary {
                median: 1.0,
                min: 1.0,
                max: 1.0,
                cv: 0.0,
                outliers: 0,
                n: 1,
            }),
            None,
        ]
    );
}

#[test]
fn report_paths_and_binary_override_follow_checkout() {
    let root = repo_root();
    assert_eq!(report_path(), root.join("site/data/bench/report.toml"));
    assert_eq!(peryx_binary(), root.join("target/release/peryx"));
    let custom = root.join("target/custom-peryx");
    set_peryx_binary(Some(custom.clone()));
    assert_eq!(peryx_binary(), custom);
    set_peryx_binary(None);
}
