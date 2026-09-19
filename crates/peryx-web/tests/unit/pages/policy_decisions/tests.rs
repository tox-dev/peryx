use leptos::prelude::*;
use rstest::rstest;

use crate::model::{PolicyDecisionFilters, UiPolicyDecisionPage};

use super::{
    PolicyDecisionFilterField, PolicyDecisionState, PolicyDecisionUi, PolicyDecisions, next_cursor, next_disabled,
    next_page, or_dash, policy_decision_page, policy_decision_results, previous_disabled, previous_page, run_query,
    set_text, submit_query, update_filter,
};

#[test]
fn policy_page_renders_query_controls() {
    let owner = Owner::new();
    owner.set();
    let html = view! { <PolicyDecisions /> }.to_html();
    for expected in [
        "Policy decisions",
        r#"id="policy-state""#,
        "Enter credentials and search",
    ] {
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }
}

#[test]
fn policy_page_renders_result_states() {
    let page: UiPolicyDecisionPage = serde_json::from_value(serde_json::json!({
        "decisions": [{
            "id": "decision-1", "repository": "private", "resource": "example", "group": null,
            "artifact": null, "source": null, "action": "serve", "state": "allow", "rule": null,
            "reason": null, "evaluated_at_unix": 0, "input_generation": {"repository": 0},
            "next_eligible_at_unix": null, "fresh": true
        }],
        "next_cursor": "next"
    }))
    .expect("policy decision page is valid");
    let html = policy_decision_page(page).to_html();
    for expected in [
        "Loaded 1 policy decisions.",
        r#"class="badge decision-allow">Allowed</span>"#,
        "<code>private</code>",
        "<td>-</td>",
    ] {
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }

    for (loading, result, expected) in [
        (true, None, "Loading policy decisions..."),
        (false, None, "Enter credentials and search to load decisions."),
        (false, Some(Err("denied".to_owned())), "denied"),
        (
            false,
            Some(Ok(UiPolicyDecisionPage {
                decisions: Vec::new(),
                next_cursor: None,
            })),
            "No policy decisions matched",
        ),
    ] {
        let html = policy_decision_results(loading, result).to_html();
        assert!(html.contains(expected), "missing {expected:?} in {html}");
    }
}

#[rstest]
#[case::first_page_idle(false, Vec::new(), true)]
#[case::first_page_loading(true, Vec::new(), true)]
#[case::later_page_idle(false, vec![None], false)]
#[case::later_page_loading(true, vec![None], true)]
fn policy_previous_is_disabled_without_history_or_while_loading(
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
fn policy_next_is_disabled_without_a_cursor_or_while_loading(
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
fn policy_next_cursor_comes_from_a_loaded_page(
    #[case] result: Option<Result<UiPolicyDecisionPage, String>>,
    #[case] expected: Option<&str>,
) {
    assert_eq!(next_cursor(result).as_deref(), expected);
}

#[test]
fn policy_set_text_stores_the_typed_value() {
    let owner = Owner::new();
    owner.set();
    let (user, set_user) = signal(String::new());
    set_text(set_user, "alice".to_owned());
    assert_eq!(user.get_untracked(), "alice");
}

#[rstest]
#[case::repository(PolicyDecisionFilterField::Repository, PolicyDecisionFilters { repository: "changed".to_owned(), ..PolicyDecisionFilters::default() })]
#[case::state(PolicyDecisionFilterField::State, PolicyDecisionFilters { state: "changed".to_owned(), ..PolicyDecisionFilters::default() })]
#[case::rule(PolicyDecisionFilterField::Rule, PolicyDecisionFilters { rule: "changed".to_owned(), ..PolicyDecisionFilters::default() })]
#[case::source(PolicyDecisionFilterField::Source, PolicyDecisionFilters { source: "changed".to_owned(), ..PolicyDecisionFilters::default() })]
#[case::from(PolicyDecisionFilterField::From, PolicyDecisionFilters { from: "changed".to_owned(), ..PolicyDecisionFilters::default() })]
#[case::to(PolicyDecisionFilterField::To, PolicyDecisionFilters { to: "changed".to_owned(), ..PolicyDecisionFilters::default() })]
#[case::limit(PolicyDecisionFilterField::Limit, PolicyDecisionFilters { limit: "changed".to_owned(), ..PolicyDecisionFilters::default() })]
fn policy_update_filter_changes_only_the_named_field(
    #[case] field: PolicyDecisionFilterField,
    #[case] expected: PolicyDecisionFilters,
) {
    let owner = Owner::new();
    owner.set();
    let (filters, set_filters) = signal(PolicyDecisionFilters::default());
    update_filter(set_filters, field, "changed".to_owned());
    assert_eq!(filters.get_untracked(), expected);
}

#[rstest]
#[case::present(Some("team-a"), "team-a")]
#[case::absent(None, "-")]
fn policy_or_dash_keeps_a_value_and_dashes_a_gap(#[case] value: Option<&str>, #[case] expected: &str) {
    assert_eq!(or_dash(value.map(str::to_owned)), expected);
}

#[test]
fn policy_submit_query_restarts_paging_with_the_typed_filters() {
    let owner = Owner::new();
    owner.set();
    let typed = PolicyDecisionFilters {
        repository: "fresh".to_owned(),
        ..PolicyDecisionFilters::default()
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
fn policy_next_page_advances_only_onto_a_known_cursor(
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
fn policy_previous_page_pops_the_last_visited_cursor(
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

#[test]
fn policy_run_query_reports_an_unparseable_date_without_loading() {
    let owner = Owner::new();
    owner.set();
    let state = seeded(PolicyDecisionFilters::default(), None, Vec::new(), false, None);
    let filters = PolicyDecisionFilters {
        from: "soon".to_owned(),
        ..PolicyDecisionFilters::default()
    };

    run_query(&filters, None, "alice".to_owned(), "secret".to_owned(), state.ui);

    assert_eq!(
        (state.loading.get_untracked(), state.result.get_untracked()),
        (false, Some(Err("Invalid UTC date and time: soon".to_owned())))
    );
}

/// The host build has nothing to fetch with, so a query that started must not leave the page stuck
/// in its loading state.
#[test]
fn policy_run_query_settles_loading_on_the_host() {
    let owner = Owner::new();
    owner.set();
    let state = seeded(PolicyDecisionFilters::default(), None, Vec::new(), true, None);

    run_query(
        &PolicyDecisionFilters::default(),
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

fn paging(cursor: Option<&str>, previous: Vec<Option<String>>, next: Option<&str>) -> PolicyDecisionState {
    seeded(
        PolicyDecisionFilters::default(),
        cursor,
        previous,
        false,
        Some(Ok(page(next))),
    )
}

fn state(
    loading: bool,
    previous: Vec<Option<String>>,
    result: Option<Result<UiPolicyDecisionPage, String>>,
) -> PolicyDecisionState {
    seeded(PolicyDecisionFilters::default(), None, previous, loading, result)
}

fn seeded(
    filters: PolicyDecisionFilters,
    cursor: Option<&str>,
    previous: Vec<Option<String>>,
    loading: bool,
    result: Option<Result<UiPolicyDecisionPage, String>>,
) -> PolicyDecisionState {
    let (active, set_active) = signal(stale());
    let (cursor, set_cursor) = signal(cursor.map(str::to_owned));
    let (previous, set_previous) = signal(previous);
    let (result, set_result) = signal(result);
    let (loading, set_loading) = signal(loading);
    PolicyDecisionState {
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
        ui: PolicyDecisionUi {
            result: set_result,
            loading: set_loading,
        },
    }
}

fn stale() -> PolicyDecisionFilters {
    PolicyDecisionFilters {
        repository: "stale".to_owned(),
        ..PolicyDecisionFilters::default()
    }
}

fn page(next_cursor: Option<&str>) -> UiPolicyDecisionPage {
    UiPolicyDecisionPage {
        decisions: Vec::new(),
        next_cursor: next_cursor.map(str::to_owned),
    }
}
