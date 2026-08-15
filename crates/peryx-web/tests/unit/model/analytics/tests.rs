use serde_json::json;

use super::{
    AnalyticsFilters, AnalyticsView, UiGroupRow, UiInterval, UiResourceRow, UiSourceRow, UiTimelineRow, UiUnusedRow,
    UiUsagePage, UiUsageRows, format_instant,
};

#[test]
fn analytics_views_round_trip() {
    for (view, query, path) in [
        (AnalyticsView::Top, "top", "/+analytics/top-resources"),
        (AnalyticsView::Groups, "groups", "/+analytics/groups"),
        (AnalyticsView::Sources, "sources", "/+analytics/sources"),
        (AnalyticsView::Unused, "unused", "/+analytics/unused"),
        (AnalyticsView::Timeline, "timeline", "/+analytics/timeline"),
    ] {
        assert_eq!(
            (view.as_query(), AnalyticsView::from_query(query), view.path()),
            (query, view, path)
        );
    }
}

#[test]
fn analytics_view_defaults_unknown_queries_to_top() {
    assert_eq!(AnalyticsView::from_query("unknown"), AnalyticsView::Top);
}

#[test]
fn intervals_format_dates_and_retention_floor() {
    let interval = UiInterval {
        from_day: 0,
        to_day: 1,
        retained_from_day: Some(2),
        window_clamped_to_retention: true,
    };
    assert_eq!(
        (interval.window(), interval.retained_from()),
        ("1970-01-01 to 1970-01-02".to_owned(), Some("1970-01-03".to_owned()))
    );
}

#[test]
fn intervals_preserve_out_of_range_days() {
    let interval = UiInterval {
        from_day: 100_000_000,
        to_day: 100_000_001,
        retained_from_day: None,
        window_clamped_to_retention: false,
    };
    assert_eq!(
        (interval.window(), interval.retained_from()),
        ("100000000 to 100000001".to_owned(), None)
    );
}

#[test]
fn intervals_preserve_days_that_overflow_seconds() {
    let interval = UiInterval {
        from_day: i64::MAX,
        to_day: i64::MIN,
        retained_from_day: None,
        window_clamped_to_retention: false,
    };
    assert_eq!(interval.window(), format!("{} to {}", i64::MAX, i64::MIN));
}

#[test]
fn usage_rows_report_lengths() {
    let rows = [
        UiUsageRows::Top(vec![UiResourceRow {
            repository: "repo".to_owned(),
            resource: "resource".to_owned(),
            reads: 1,
            bytes: 2,
        }]),
        UiUsageRows::Groups(vec![UiGroupRow {
            repository: "repo".to_owned(),
            resource: "resource".to_owned(),
            group: None,
            reads: 1,
            bytes: 2,
        }]),
        UiUsageRows::Sources(vec![UiSourceRow {
            repository: "repo".to_owned(),
            resource: "resource".to_owned(),
            source: None,
            reads: 1,
            bytes: 2,
        }]),
        UiUsageRows::Unused(vec![UiUnusedRow {
            repository: "repo".to_owned(),
            resource: "resource".to_owned(),
            lifetime_reads: 1,
        }]),
        UiUsageRows::Timeline(vec![UiTimelineRow {
            day: 0,
            start_unix: 0,
            end_unix: 86_400,
            reads: 1,
            bytes: 2,
        }]),
    ];
    assert_eq!(rows.map(|rows| (rows.len(), rows.is_empty())), [(1, false); 5]);
}

#[test]
fn empty_usage_rows_report_empty() {
    let rows = [
        UiUsageRows::Top(Vec::new()),
        UiUsageRows::Groups(Vec::new()),
        UiUsageRows::Sources(Vec::new()),
        UiUsageRows::Unused(Vec::new()),
        UiUsageRows::Timeline(Vec::new()),
    ];
    assert_eq!(rows.map(|rows| (rows.len(), rows.is_empty())), [(0, true); 5]);
}

#[test]
fn usage_pages_parse_each_view() {
    let cases = [
        (
            AnalyticsView::Top,
            "resources",
            json!({"repository":"repo","resource":"resource","reads":1,"bytes":2}),
            UiUsageRows::Top(vec![UiResourceRow {
                repository: "repo".to_owned(),
                resource: "resource".to_owned(),
                reads: 1,
                bytes: 2,
            }]),
        ),
        (
            AnalyticsView::Groups,
            "groups",
            json!({"repository":"repo","resource":"resource","group":"group","reads":1,"bytes":2}),
            UiUsageRows::Groups(vec![UiGroupRow {
                repository: "repo".to_owned(),
                resource: "resource".to_owned(),
                group: Some("group".to_owned()),
                reads: 1,
                bytes: 2,
            }]),
        ),
        (
            AnalyticsView::Sources,
            "sources",
            json!({"repository":"repo","resource":"resource","source":"remote","reads":1,"bytes":2}),
            UiUsageRows::Sources(vec![UiSourceRow {
                repository: "repo".to_owned(),
                resource: "resource".to_owned(),
                source: Some("remote".to_owned()),
                reads: 1,
                bytes: 2,
            }]),
        ),
        (
            AnalyticsView::Unused,
            "unused",
            json!({"repository":"repo","resource":"resource","lifetime_reads":1}),
            UiUsageRows::Unused(vec![UiUnusedRow {
                repository: "repo".to_owned(),
                resource: "resource".to_owned(),
                lifetime_reads: 1,
            }]),
        ),
        (
            AnalyticsView::Timeline,
            "buckets",
            json!({"day":0,"start_unix":0,"end_unix":86400,"reads":1,"bytes":2}),
            UiUsageRows::Timeline(vec![UiTimelineRow {
                day: 0,
                start_unix: 0,
                end_unix: 86_400,
                reads: 1,
                bytes: 2,
            }]),
        ),
    ];
    for (view, rows_key, row, rows) in cases {
        let mut value = json!({"interval": interval(), "next_cursor": "next"});
        value[rows_key] = json!([row]);
        assert_eq!(
            UiUsagePage::parse(view, &value),
            Some(UiUsagePage {
                rows,
                interval: UiInterval {
                    from_day: 0,
                    to_day: 1,
                    retained_from_day: None,
                    window_clamped_to_retention: false,
                },
                next_cursor: Some("next".to_owned()),
            })
        );
    }
}

#[test]
fn usage_pages_reject_malformed_envelopes() {
    for (view, value) in [
        (AnalyticsView::Top, json!({"next_cursor": null, "resources": []})),
        (
            AnalyticsView::Top,
            json!({"interval": {}, "next_cursor": null, "resources": []}),
        ),
        (AnalyticsView::Top, json!({"interval": interval(), "resources": []})),
        (
            AnalyticsView::Top,
            json!({"interval": interval(), "next_cursor": 1, "resources": []}),
        ),
        (AnalyticsView::Top, json!({"interval": interval(), "next_cursor": null})),
        (
            AnalyticsView::Top,
            json!({"interval": interval(), "next_cursor": null, "resources": [{}]}),
        ),
        (
            AnalyticsView::Groups,
            json!({"interval": interval(), "next_cursor": null, "groups": [{}]}),
        ),
        (
            AnalyticsView::Sources,
            json!({"interval": interval(), "next_cursor": null, "sources": [{}]}),
        ),
        (
            AnalyticsView::Unused,
            json!({"interval": interval(), "next_cursor": null, "unused": [{}]}),
        ),
        (
            AnalyticsView::Timeline,
            json!({"interval": interval(), "next_cursor": null, "buckets": [{}]}),
        ),
    ] {
        assert!(UiUsagePage::parse(view, &value).is_none());
    }
}

#[test]
fn analytics_filters_have_stable_defaults() {
    assert_eq!(
        AnalyticsFilters::default(),
        AnalyticsFilters {
            view: "top".to_owned(),
            repository: String::new(),
            from: String::new(),
            to: String::new(),
            limit: "25".to_owned(),
        }
    );
}

#[test]
fn analytics_filters_keep_only_the_required_limit() {
    assert_eq!(
        AnalyticsFilters::default().url(None),
        Ok("/+analytics/top-resources?limit=25".to_owned())
    );
}

#[test]
fn analytics_filters_encode_values_and_cursor() {
    let filters = AnalyticsFilters {
        view: "timeline".to_owned(),
        repository: " root/private ".to_owned(),
        from: " 1970-01-02 ".to_owned(),
        to: "1970-01-03".to_owned(),
        limit: " 50 ".to_owned(),
    };
    assert_eq!(
        (filters.view(), filters.url(Some("next value"))),
        (
            AnalyticsView::Timeline,
            Ok(
                "/+analytics/timeline?repository=root%2Fprivate&from=86400&to=172800&limit=50&cursor=next+value"
                    .to_owned()
            )
        )
    );
}

#[test]
fn analytics_filters_reject_invalid_dates() {
    for (from, to) in [("invalid", ""), ("", "invalid")] {
        assert_eq!(
            AnalyticsFilters {
                from: from.to_owned(),
                to: to.to_owned(),
                ..AnalyticsFilters::default()
            }
            .url(None),
            Err("Invalid UTC date: invalid".to_owned())
        );
    }
}

#[test]
fn instants_format_valid_values_and_preserve_invalid_ones() {
    for (value, expected) in [
        (0, "1970-01-01T00:00:00Z"),
        (-62_167_219_201, "-62167219201"),
        (i64::MAX, "9223372036854775807"),
    ] {
        assert_eq!(format_instant(value), expected);
    }
}

fn interval() -> serde_json::Value {
    json!({
        "from_day": 0,
        "to_day": 1,
        "retained_from_day": null,
        "window_clamped_to_retention": false
    })
}
