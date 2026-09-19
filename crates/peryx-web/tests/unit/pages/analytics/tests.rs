use leptos::prelude::*;
use rstest::rstest;

use crate::model::{AnalyticsFilters, AnalyticsView, UiUsagePage};

use super::{
    AnalyticsFilterField, AnalyticsState, UsageAnalytics, analytics_results, next_cursor, next_disabled,
    previous_disabled, set_text, update_filter, usage_page,
};

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

#[rstest]
#[case::first_page_idle(false, Vec::new(), true)]
#[case::first_page_loading(true, Vec::new(), true)]
#[case::later_page_idle(false, vec![None], false)]
#[case::later_page_loading(true, vec![None], true)]
fn analytics_previous_is_disabled_without_history_or_while_loading(
    #[case] loading: bool,
    #[case] previous: Vec<Option<String>>,
    #[case] expected: bool,
) {
    Owner::new().with(|| {
        assert_eq!(previous_disabled(state(loading, previous, None)), expected);
    });
}

#[rstest]
#[case::more_pages_idle(false, Some("page-2"), false)]
#[case::more_pages_loading(true, Some("page-2"), true)]
#[case::last_page_idle(false, None, true)]
#[case::last_page_loading(true, None, true)]
fn analytics_next_is_disabled_without_a_cursor_or_while_loading(
    #[case] loading: bool,
    #[case] cursor: Option<&str>,
    #[case] expected: bool,
) {
    Owner::new().with(|| {
        assert_eq!(
            next_disabled(state(loading, Vec::new(), Some(Ok(page(cursor))))),
            expected
        );
    });
}

#[rstest]
#[case::not_loaded(None, None)]
#[case::failed(Some(Err("denied".to_owned())), None)]
#[case::last_page(Some(Ok(page(None))), None)]
#[case::more_pages(Some(Ok(page(Some("page-2")))), Some("page-2"))]
fn analytics_next_cursor_comes_from_a_loaded_page(
    #[case] result: Option<Result<UiUsagePage, String>>,
    #[case] expected: Option<&str>,
) {
    assert_eq!(next_cursor(result).as_deref(), expected);
}

#[test]
fn analytics_set_text_stores_the_typed_value() {
    Owner::new().with(|| {
        let (user, set_user) = signal(String::new());
        set_text(set_user, "alice".to_owned());
        assert_eq!(user.get_untracked(), "alice");
    });
}

#[rstest]
#[case::view(AnalyticsFilterField::View, AnalyticsFilters { view: "changed".to_owned(), ..AnalyticsFilters::default() })]
#[case::repository(AnalyticsFilterField::Repository, AnalyticsFilters { repository: "changed".to_owned(), ..AnalyticsFilters::default() })]
#[case::from(AnalyticsFilterField::From, AnalyticsFilters { from: "changed".to_owned(), ..AnalyticsFilters::default() })]
#[case::to(AnalyticsFilterField::To, AnalyticsFilters { to: "changed".to_owned(), ..AnalyticsFilters::default() })]
#[case::limit(AnalyticsFilterField::Limit, AnalyticsFilters { limit: "changed".to_owned(), ..AnalyticsFilters::default() })]
fn analytics_update_filter_changes_only_the_named_field(
    #[case] field: AnalyticsFilterField,
    #[case] expected: AnalyticsFilters,
) {
    Owner::new().with(|| {
        let (filters, set_filters) = signal(AnalyticsFilters::default());
        update_filter(set_filters, field, "changed".to_owned());
        assert_eq!(filters.get_untracked(), expected);
    });
}

fn state(loading: bool, previous: Vec<Option<String>>, result: Option<Result<UiUsagePage, String>>) -> AnalyticsState {
    let (active, set_active) = signal(AnalyticsFilters::default());
    let (cursor, set_cursor) = signal(None);
    let (previous, set_previous) = signal(previous);
    AnalyticsState {
        user: signal(String::new()).0,
        password: signal(String::new()).0,
        filters: signal(AnalyticsFilters::default()).0,
        active,
        set_active,
        cursor,
        set_cursor,
        previous,
        set_previous,
        result: signal(result).0,
        loading: signal(loading).0,
    }
}

fn page(next_cursor: Option<&str>) -> UiUsagePage {
    UiUsagePage {
        next_cursor: next_cursor.map(str::to_owned),
        ..usage(AnalyticsView::Top, "resources", &serde_json::json!([]), false)
    }
}
