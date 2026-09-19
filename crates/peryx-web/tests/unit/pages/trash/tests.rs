use leptos::prelude::*;
use rstest::rstest;

use crate::model::{TrashFilters, UiTrashPage};

use super::{
    Trash, TrashFilterField, TrashState, TrashUi, next_cursor, next_disabled, next_page, previous_disabled,
    previous_page, run_query, set_text, submit_query, trash_page, trash_results, update_filter,
};

#[test]
fn trash_page_renders_query_controls() {
    let owner = Owner::new();
    owner.set();
    let html = view! { <Trash /> }.to_html();
    for expected in ["Trash", r#"id="trash-state""#, "Enter credentials and search"] {
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }
}

#[test]
fn trash_page_renders_result_states() {
    let page: UiTrashPage = serde_json::from_value(serde_json::json!({
        "trash": [{
            "ecosystem": "example", "repository": "root/hosted", "resource": "artifact",
            "artifact": "artifact.bin", "digest": null, "reason": null, "actor": null,
            "deleted_at_unix": 0, "deadline_unix": 86400, "state": "restorable", "restorable": true
        }],
        "next_cursor": "next"
    }))
    .expect("trash page is valid");
    let html = trash_page(page).to_html();
    for expected in [
        "Loaded 1 trash records.",
        r#"class="badge trash-restorable">Restorable</span>"#,
        "<td>artifact.bin</td>",
        "<td>-</td>",
    ] {
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }

    for (loading, result, expected) in [
        (true, None, "Loading trash..."),
        (false, None, "Enter credentials and search to load trash records."),
        (false, Some(Err("denied".to_owned())), "denied"),
        (
            false,
            Some(Ok(UiTrashPage {
                trash: Vec::new(),
                next_cursor: None,
            })),
            "No trash records matched",
        ),
    ] {
        let html = trash_results(loading, result).to_html();
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }
}

#[rstest]
#[case::first_page_idle(false, Vec::new(), true)]
#[case::first_page_loading(true, Vec::new(), true)]
#[case::later_page_idle(false, vec![None], false)]
#[case::later_page_loading(true, vec![None], true)]
fn trash_previous_is_disabled_without_history_or_while_loading(
    #[case] loading: bool,
    #[case] previous: Vec<Option<String>>,
    #[case] expected: bool,
) {
    let owner = Owner::new();
    owner.set();
    assert_eq!(previous_disabled(state(loading, previous, None)), expected);
}

#[rstest]
#[case::more_pages_idle(false, Some("page-2"), false)]
#[case::more_pages_loading(true, Some("page-2"), true)]
#[case::last_page_idle(false, None, true)]
#[case::last_page_loading(true, None, true)]
fn trash_next_is_disabled_without_a_cursor_or_while_loading(
    #[case] loading: bool,
    #[case] cursor: Option<&str>,
    #[case] expected: bool,
) {
    let owner = Owner::new();
    owner.set();
    assert_eq!(
        next_disabled(state(loading, Vec::new(), Some(Ok(page(cursor))))),
        expected
    );
}

#[rstest]
#[case::not_loaded(None, None)]
#[case::failed(Some(Err("denied".to_owned())), None)]
#[case::last_page(Some(Ok(page(None))), None)]
#[case::more_pages(Some(Ok(page(Some("page-2")))), Some("page-2"))]
fn trash_next_cursor_comes_from_a_loaded_page(
    #[case] result: Option<Result<UiTrashPage, String>>,
    #[case] expected: Option<&str>,
) {
    assert_eq!(next_cursor(result).as_deref(), expected);
}

#[test]
fn trash_set_text_stores_the_typed_value() {
    let owner = Owner::new();
    owner.set();
    let (user, set_user) = signal(String::new());
    set_text(set_user, "alice".to_owned());
    assert_eq!(user.get_untracked(), "alice");
}

#[rstest]
#[case::repository(TrashFilterField::Repository, TrashFilters { repository: "changed".to_owned(), ..TrashFilters::default() })]
#[case::ecosystem(TrashFilterField::Ecosystem, TrashFilters { ecosystem: "changed".to_owned(), ..TrashFilters::default() })]
#[case::state(TrashFilterField::State, TrashFilters { state: "changed".to_owned(), ..TrashFilters::default() })]
#[case::limit(TrashFilterField::Limit, TrashFilters { limit: "changed".to_owned(), ..TrashFilters::default() })]
fn trash_update_filter_changes_only_the_named_field(#[case] field: TrashFilterField, #[case] expected: TrashFilters) {
    let owner = Owner::new();
    owner.set();
    let (filters, set_filters) = signal(TrashFilters::default());
    update_filter(set_filters, field, "changed".to_owned());
    assert_eq!(filters.get_untracked(), expected);
}

#[test]
fn trash_submit_query_restarts_paging_with_the_typed_filters() {
    let owner = Owner::new();
    owner.set();
    let typed = TrashFilters {
        repository: "fresh".to_owned(),
        ..TrashFilters::default()
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
}

#[rstest]
#[case::more_pages(Some("page-2"), Some("page-2"), vec![None, Some("page-1".to_owned())])]
#[case::last_page(None, Some("page-1"), vec![None])]
fn trash_next_page_advances_only_onto_a_known_cursor(
    #[case] next: Option<&str>,
    #[case] cursor: Option<&str>,
    #[case] previous: Vec<Option<String>>,
) {
    let owner = Owner::new();
    owner.set();
    let state = paging(Some("page-1"), vec![None], next);

    next_page(state);

    assert_eq!(
        (state.cursor.get_untracked(), state.previous.get_untracked()),
        (cursor.map(str::to_owned), previous)
    );
}

#[rstest]
#[case::back_to_a_later_page(vec![None, Some("page-1".to_owned())], Some("page-1"), vec![None])]
#[case::back_to_the_first_page(vec![None], None, Vec::new())]
#[case::no_history(Vec::new(), Some("page-2"), Vec::new())]
fn trash_previous_page_pops_the_last_visited_cursor(
    #[case] history: Vec<Option<String>>,
    #[case] cursor: Option<&str>,
    #[case] previous: Vec<Option<String>>,
) {
    let owner = Owner::new();
    owner.set();
    let state = paging(Some("page-2"), history, None);

    previous_page(state);

    assert_eq!(
        (state.cursor.get_untracked(), state.previous.get_untracked()),
        (cursor.map(str::to_owned), previous)
    );
}

/// The host build has nothing to fetch with, so a query that started must not leave the page stuck
/// in its loading state.
#[test]
fn trash_run_query_settles_loading_on_the_host() {
    let owner = Owner::new();
    owner.set();
    let state = seeded(TrashFilters::default(), None, Vec::new(), true, None);

    run_query(
        &TrashFilters::default(),
        None,
        "alice".to_owned(),
        "secret".to_owned(),
        state.ui,
    );

    assert_eq!(
        (state.loading.get_untracked(), state.result.get_untracked()),
        (false, None)
    );
}

fn paging(cursor: Option<&str>, previous: Vec<Option<String>>, next: Option<&str>) -> TrashState {
    seeded(TrashFilters::default(), cursor, previous, false, Some(Ok(page(next))))
}

fn state(loading: bool, previous: Vec<Option<String>>, result: Option<Result<UiTrashPage, String>>) -> TrashState {
    seeded(TrashFilters::default(), None, previous, loading, result)
}

fn seeded(
    filters: TrashFilters,
    cursor: Option<&str>,
    previous: Vec<Option<String>>,
    loading: bool,
    result: Option<Result<UiTrashPage, String>>,
) -> TrashState {
    let (active, set_active) = signal(stale());
    let (cursor, set_cursor) = signal(cursor.map(str::to_owned));
    let (previous, set_previous) = signal(previous);
    let (result, set_result) = signal(result);
    let (loading, set_loading) = signal(loading);
    TrashState {
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
        ui: TrashUi {
            result: set_result,
            loading: set_loading,
        },
    }
}

fn stale() -> TrashFilters {
    TrashFilters {
        repository: "stale".to_owned(),
        ..TrashFilters::default()
    }
}

fn page(next_cursor: Option<&str>) -> UiTrashPage {
    UiTrashPage {
        trash: Vec::new(),
        next_cursor: next_cursor.map(str::to_owned),
    }
}
