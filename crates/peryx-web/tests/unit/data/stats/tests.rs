use std::collections::BTreeMap;

use rstest::rstest;

use super::{CounterGroups, IndexStatsDocument, ResourceStatsDocument};
use crate::data::{LoaderEndpoint, LoaderError};
use crate::model::{UiCounters, UiStats};

fn counter_groups(offset: u64) -> serde_json::Value {
    serde_json::json!({
        "base": {"pages": offset + 1, "reads": offset + 2, "bytes": offset + 5, "rejected": offset + 10},
        "cached": {
            "refreshes": offset + 6,
            "changed": offset + 7,
            "stale_served": offset + 8,
            "upstream_errors": offset + 9,
        },
        "hosted": {"writes": offset + 4},
        "ecosystem": {"metadata": offset + 3, "simple": 77},
    })
}

const fn ui_counters(offset: u64) -> UiCounters {
    UiCounters {
        pages: offset + 1,
        reads: offset + 2,
        metadata: offset + 3,
        writes: offset + 4,
        bytes: offset + 5,
        refreshes: offset + 6,
        changed: offset + 7,
        stale_served: offset + 8,
        upstream_errors: offset + 9,
        rejected: offset + 10,
    }
}

fn activity_row(name: &str, reads: u64, pages: u64) -> (String, UiCounters) {
    (
        name.to_owned(),
        UiCounters {
            pages,
            reads,
            ..UiCounters::default()
        },
    )
}

#[cfg(feature = "ssr")]
#[test]
fn stats_parser_selects_requested_depth() {
    let value = serde_json::json!({
        "routes": {"route": {}},
        "resources": {"resource": {}},
        "artifacts": {"artifact": {}},
    });
    for (index, resource, expected) in [
        (None, None, "artifacts"),
        (None, Some("artifact"), "artifacts"),
        (Some("root/cache"), None, "resource"),
        (Some("root/cache"), Some("artifact"), "artifact"),
    ] {
        assert_eq!(super::parse_stats(&value, index, resource).rows[0].0, expected);
    }
}

#[test]
fn stats_routes_sums_every_counter_across_routes() {
    let routes: BTreeMap<String, CounterGroups> = serde_json::from_value(serde_json::json!({
        "root/alpha": counter_groups(100),
        "root/beta": counter_groups(2000),
    }))
    .unwrap();

    assert_eq!(
        super::stats_routes(routes),
        UiStats {
            totals: UiCounters {
                pages: 2102,
                reads: 2104,
                metadata: 2106,
                writes: 2108,
                bytes: 2110,
                refreshes: 2112,
                changed: 2114,
                stale_served: 2116,
                upstream_errors: 2118,
                rejected: 2120,
            },
            rows: vec![
                ("root/beta".to_owned(), ui_counters(2000)),
                ("root/alpha".to_owned(), ui_counters(100)),
            ],
        }
    );
}

#[test]
fn stats_index_reads_totals_and_resource_rows() {
    let document: IndexStatsDocument = serde_json::from_value(serde_json::json!({
        "totals": counter_groups(500),
        "resources": {"quiet": counter_groups(10), "busy": counter_groups(300)},
    }))
    .unwrap();

    assert_eq!(
        super::stats_index(document),
        Ok(UiStats {
            totals: ui_counters(500),
            rows: vec![
                ("busy".to_owned(), ui_counters(300)),
                ("quiet".to_owned(), ui_counters(10))
            ],
        })
    );
}

#[test]
fn stats_resource_reads_totals_and_artifact_rows() {
    let document: ResourceStatsDocument = serde_json::from_value(serde_json::json!({
        "totals": counter_groups(500),
        "artifacts": {
            "quiet.whl": {"reads": 2, "bytes": 30, "ecosystem": {"metadata": 4}},
            "busy.whl": {"reads": 9, "bytes": 80, "ecosystem": {"metadata": 7}},
        },
    }))
    .unwrap();

    assert_eq!(
        super::stats_resource(document),
        Ok(UiStats {
            totals: ui_counters(500),
            rows: vec![
                (
                    "busy.whl".to_owned(),
                    UiCounters {
                        reads: 9,
                        bytes: 80,
                        metadata: 7,
                        ..UiCounters::default()
                    }
                ),
                (
                    "quiet.whl".to_owned(),
                    UiCounters {
                        reads: 2,
                        bytes: 30,
                        metadata: 4,
                        ..UiCounters::default()
                    }
                ),
            ],
        })
    );
}

#[test]
fn stats_counters_without_metadata_family_report_zero_metadata() {
    let document: ResourceStatsDocument = serde_json::from_value(serde_json::json!({
        "totals": {
            "base": {"pages": 1, "reads": 2, "bytes": 3, "rejected": 4},
            "cached": {"refreshes": 0, "changed": 0, "stale_served": 0, "upstream_errors": 0},
            "hosted": {"writes": 0},
            "ecosystem": {"simple": 5},
        },
        "artifacts": {"one.whl": {"reads": 2, "bytes": 3, "ecosystem": {}}},
    }))
    .unwrap();

    let stats: UiStats = super::stats_resource(document).unwrap();

    assert_eq!((stats.totals.metadata, stats.rows[0].1.metadata), (0, 0));
}

#[test]
fn stats_index_without_totals_and_resources_is_empty() {
    let document: IndexStatsDocument = serde_json::from_str("{}").unwrap();

    assert_eq!(super::stats_index(document), Ok(UiStats::default()));
}

#[test]
fn stats_resource_without_totals_and_artifacts_is_empty() {
    let document: ResourceStatsDocument = serde_json::from_str("{}").unwrap();

    assert_eq!(super::stats_resource(document), Ok(UiStats::default()));
}

#[rstest]
#[case::totals_only(serde_json::json!({"totals": counter_groups(1)}))]
#[case::resources_only(serde_json::json!({"resources": {"busy": counter_groups(1)}}))]
fn stats_index_with_half_a_document_is_invalid(#[case] value: serde_json::Value) {
    let document: IndexStatsDocument = serde_json::from_value(value).unwrap();

    assert_eq!(
        super::stats_index(document),
        Err(LoaderError::Invalid(LoaderEndpoint::Stats))
    );
}

// A pair is the smallest input whose single comparison sees each row on either side, so both
// orders together reach the left and the right operand of the activity sum.
#[rstest]
#[case::already_ordered([activity_row("zeta", 0, 10), activity_row("alpha", 3, 5)])]
#[case::reversed([activity_row("alpha", 3, 5), activity_row("zeta", 0, 10)])]
fn sort_rows_orders_by_reads_plus_pages_descending(#[case] mut rows: [(String, UiCounters); 2]) {
    super::sort_rows(&mut rows);

    assert_eq!(rows, [activity_row("zeta", 0, 10), activity_row("alpha", 3, 5)]);
}

#[test]
fn sort_rows_breaks_activity_ties_by_name() {
    let mut rows: [(String, UiCounters); 2] = [activity_row("beta", 4, 1), activity_row("alpha", 1, 4)];

    super::sort_rows(&mut rows);

    assert_eq!(rows, [activity_row("alpha", 1, 4), activity_row("beta", 4, 1)]);
}
