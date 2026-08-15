use leptos::prelude::*;

use crate::model::{AnalyticsView, UiUsagePage};

use super::{UsageAnalytics, analytics_results, usage_page};

#[test]
fn analytics_page_renders_query_controls() {
    Owner::new().with(|| {
        let html = view! { <UsageAnalytics /> }.to_html();
        assert!(html.contains("Usage analytics"), "{html}");
        assert!(html.contains(r#"id="analytics-view""#), "{html}");
        assert!(html.contains("Enter credentials and search"), "{html}");
    });
}

#[test]
fn analytics_results_renders_native_states() {
    for (result, expected) in [
        (None, "Loading usage analytics..."),
        (None, "Enter credentials and search to load usage."),
        (Some(Err("denied".to_owned())), "denied"),
        (
            Some(Ok(usage(
                AnalyticsView::Top,
                "resources",
                &serde_json::json!([]),
                false,
            ))),
            "No usage recorded for this view in the resolved window.",
        ),
    ] {
        let html = analytics_results(expected == "Loading usage analytics...", result).to_html();
        assert!(html.contains(expected), "{html}");
    }
}

#[test]
fn usage_page_renders_each_view() {
    for (view, key, rows, expected) in [
        (
            AnalyticsView::Top,
            "resources",
            serde_json::json!([{"repository":"r","resource":"p","reads":1,"bytes":2048}]),
            ["usage-top-table", "2.0 kB"],
        ),
        (
            AnalyticsView::Groups,
            "groups",
            serde_json::json!([{"repository":"r","resource":"p","group":null,"reads":1,"bytes":2}]),
            ["usage-groups-table", "<td>-</td>"],
        ),
        (
            AnalyticsView::Sources,
            "sources",
            serde_json::json!([{"repository":"r","resource":"p","source":null,"reads":1,"bytes":2}]),
            ["usage-sources-table", "local store"],
        ),
        (
            AnalyticsView::Unused,
            "unused",
            serde_json::json!([{"repository":"r","resource":"p","lifetime_reads":9}]),
            ["usage-unused-table", "Lifetime reads"],
        ),
        (
            AnalyticsView::Timeline,
            "buckets",
            serde_json::json!([{"day":1,"start_unix":0,"end_unix":86400,"reads":4,"bytes":8}]),
            ["usage-timeline-table", "1970-01-02T00:00:00Z"],
        ),
    ] {
        let html = usage_page(usage(view, key, &rows, false)).to_html();
        for fragment in ["Loaded 1 rows.", expected[0], expected[1]] {
            assert!(html.contains(fragment), "missing {fragment:?} in {html}");
        }
    }
}

#[test]
fn usage_page_reports_empty_retention_window() {
    let html = usage_page(usage(AnalyticsView::Top, "resources", &serde_json::json!([]), true)).to_html();
    assert!(html.contains("Window clamped to retention."), "{html}");
    assert!(html.contains("No usage recorded"), "{html}");
}

#[test]
fn usage_page_renders_optional_group_and_source_labels() {
    for (view, key, rows, expected) in [
        (
            AnalyticsView::Groups,
            "groups",
            serde_json::json!([
                {"repository":"r","resource":"grouped","group":"group","reads":1,"bytes":2},
                {"repository":"r","resource":"ungrouped","group":null,"reads":1,"bytes":2}
            ]),
            ["group", "<td>-</td>"],
        ),
        (
            AnalyticsView::Sources,
            "sources",
            serde_json::json!([
                {"repository":"r","resource":"remote","source":"mirror","reads":1,"bytes":2},
                {"repository":"r","resource":"local","source":null,"reads":1,"bytes":2}
            ]),
            ["mirror", "local store"],
        ),
    ] {
        let html = usage_page(usage(view, key, &rows, false)).to_html();
        for fragment in expected {
            assert!(html.contains(fragment), "missing {fragment:?} in {html}");
        }
    }
}

#[test]
fn usage_page_names_an_unknown_retention_floor() {
    let mut page = usage(AnalyticsView::Top, "resources", &serde_json::json!([]), true);
    page.interval.retained_from_day = None;
    let html = usage_page(page).to_html();
    assert!(html.contains("Data before the retention floor has aged out"), "{html}");
}

fn usage(view: AnalyticsView, key: &str, rows: &serde_json::Value, clamped: bool) -> UiUsagePage {
    UiUsagePage::parse(
        view,
        &serde_json::json!({
            key: rows,
            "interval": {
                "from_day": 1,
                "to_day": 2,
                "from_unix": 86400,
                "to_unix": 259_200,
                "retained_from_day": 1,
                "window_clamped_to_retention": clamped,
            },
            "next_cursor": null,
        }),
    )
    .unwrap()
}
