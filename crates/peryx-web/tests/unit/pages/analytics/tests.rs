use leptos::prelude::*;
use rstest::rstest;

use crate::model::{AnalyticsFilters, AnalyticsView, UiUsagePage};

use super::{
    AnalyticsFilterField, AnalyticsState, AnalyticsUi, UsageAnalytics, analytics_results, next_cursor, next_disabled,
    next_page, previous_disabled, previous_page, run_query, set_text, submit_query, update_filter, usage_page,
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

#[test]
fn analytics_submit_query_restarts_paging_with_the_typed_filters() {
    Owner::new().with(|| {
        let typed = AnalyticsFilters {
            repository: "fresh".to_owned(),
            ..AnalyticsFilters::default()
        };
        let state = seeded(
            typed.clone(),
            Some("page-3"),
            vec![None, Some("page-2".to_owned())],
            false,
            None,
        );

        submit_query(state);

        assert_eq!(
            (
                state.active.get_untracked(),
                state.cursor.get_untracked(),
                state.previous.get_untracked()
            ),
            (typed, None, Vec::new())
        );
    });
}

#[rstest]
#[case::more_pages(Some("page-2"), Some("page-2"), vec![None, Some("page-1".to_owned())])]
#[case::last_page(None, Some("page-1"), vec![None])]
fn analytics_next_page_advances_only_onto_a_known_cursor(
    #[case] next: Option<&str>,
    #[case] cursor: Option<&str>,
    #[case] previous: Vec<Option<String>>,
) {
    Owner::new().with(|| {
        let state = paging(Some("page-1"), vec![None], next);

        next_page(state);

        assert_eq!(
            (state.cursor.get_untracked(), state.previous.get_untracked()),
            (cursor.map(str::to_owned), previous)
        );
    });
}

#[rstest]
#[case::back_to_a_later_page(vec![None, Some("page-1".to_owned())], Some("page-1"), vec![None])]
#[case::back_to_the_first_page(vec![None], None, Vec::new())]
#[case::no_history(Vec::new(), Some("page-2"), Vec::new())]
fn analytics_previous_page_pops_the_last_visited_cursor(
    #[case] history: Vec<Option<String>>,
    #[case] cursor: Option<&str>,
    #[case] previous: Vec<Option<String>>,
) {
    Owner::new().with(|| {
        let state = paging(Some("page-2"), history, None);

        previous_page(state);

        assert_eq!(
            (state.cursor.get_untracked(), state.previous.get_untracked()),
            (cursor.map(str::to_owned), previous)
        );
    });
}

#[test]
fn analytics_run_query_reports_an_unparseable_date_without_loading() {
    Owner::new().with(|| {
        let state = seeded(AnalyticsFilters::default(), None, Vec::new(), false, None);
        let filters = AnalyticsFilters {
            from: "soon".to_owned(),
            ..AnalyticsFilters::default()
        };

        run_query(&filters, None, "alice".to_owned(), "secret".to_owned(), state.ui);

        assert_eq!(
            (state.loading.get_untracked(), state.result.get_untracked()),
            (false, Some(Err("Invalid UTC date: soon".to_owned())))
        );
    });
}

fn paging(cursor: Option<&str>, previous: Vec<Option<String>>, next: Option<&str>) -> AnalyticsState {
    seeded(
        AnalyticsFilters::default(),
        cursor,
        previous,
        false,
        Some(Ok(page(next))),
    )
}

fn state(loading: bool, previous: Vec<Option<String>>, result: Option<Result<UiUsagePage, String>>) -> AnalyticsState {
    seeded(AnalyticsFilters::default(), None, previous, loading, result)
}

fn seeded(
    filters: AnalyticsFilters,
    cursor: Option<&str>,
    previous: Vec<Option<String>>,
    loading: bool,
    result: Option<Result<UiUsagePage, String>>,
) -> AnalyticsState {
    let (active, set_active) = signal(stale());
    let (cursor, set_cursor) = signal(cursor.map(str::to_owned));
    let (previous, set_previous) = signal(previous);
    let (result, set_result) = signal(result);
    let (loading, set_loading) = signal(loading);
    AnalyticsState {
        user: signal("alice".to_owned()).0,
        password: signal("secret".to_owned()).0,
        filters: signal(filters).0,
        active,
        set_active,
        cursor,
        set_cursor,
        previous,
        set_previous,
        result,
        loading,
        ui: AnalyticsUi {
            result: set_result,
            loading: set_loading,
        },
    }
}

fn stale() -> AnalyticsFilters {
    AnalyticsFilters {
        repository: "stale".to_owned(),
        ..AnalyticsFilters::default()
    }
}

fn page(next_cursor: Option<&str>) -> UiUsagePage {
    UiUsagePage {
        next_cursor: next_cursor.map(str::to_owned),
        ..usage(AnalyticsView::Top, "resources", &serde_json::json!([]), false)
    }
}
