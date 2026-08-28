use leptos::either::Either;
use leptos::prelude::*;

use super::reactive_value;
use crate::model::PolicyDecisionFilters;
use crate::model::{UiPolicyDecision, UiPolicyDecisionPage};

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
pub fn PolicyDecisions() -> impl IntoView {
    let (_, set_user) = signal(String::new());
    let (_, set_password) = signal(String::new());
    let (_, set_filters) = signal(PolicyDecisionFilters::default());
    let (result, _) = signal(None::<Result<UiPolicyDecisionPage, String>>);
    let (loading, _) = signal(false);
    view! {
        <section class="page policy-decisions-page">
            <div class="ops-title">
                <h1>"Policy decisions"</h1>
                <span class="badge">"read-only"</span>
            </div>
            <p class="dim">
                "Inspect recorded allow, deny, and wait outcomes. Times are UTC. Credentials remain in this browser tab and are sent only in the authorization header."
            </p>
            <form class="policy-filters">
                <PolicyDecisionFilterFields set_user set_password set_filters loading />
            </form>
            <div class="policy-results">
                {move || policy_decision_results(reactive_value(&loading), reactive_value(&result))}
            </div>
            <div class="pagination">
                <button type="button" disabled=move || true>"Previous"</button>
                <button type="button" disabled=move || true>"Next"</button>
            </div>
        </section>
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
pub fn PolicyDecisions() -> impl IntoView {
    let (user, set_user) = signal(String::new());
    let (password, set_password) = signal(String::new());
    let (filters, set_filters) = signal(PolicyDecisionFilters::default());
    let (active, set_active) = signal(PolicyDecisionFilters::default());
    let (cursor, set_cursor) = signal(None::<String>);
    let (previous, set_previous) = signal(Vec::<Option<String>>::new());
    let (result, set_result) = signal(None::<Result<UiPolicyDecisionPage, String>>);
    let (loading, set_loading) = signal(false);
    let ui = PolicyDecisionUi {
        result: set_result,
        loading: set_loading,
    };
    let state = PolicyDecisionState {
        user,
        password,
        filters,
        active,
        set_active,
        cursor,
        set_cursor,
        previous,
        set_previous,
        result,
        loading,
        ui,
    };
    view! {
        <section class="page policy-decisions-page">
            <div class="ops-title">
                <h1>"Policy decisions"</h1>
                <span class="badge">"read-only"</span>
            </div>
            <p class="dim">
                "Inspect recorded allow, deny, and wait outcomes. Times are UTC. Credentials remain in this browser tab and are sent only in the authorization header."
            </p>
            <form class="policy-filters" on:submit=move |event| submit(&event, state)>
                <PolicyDecisionFilterFields set_user set_password set_filters loading />
            </form>
            <div class="policy-results">
                {move || policy_decision_results(reactive_value(&state.loading), reactive_value(&state.result))}
            </div>
            <div class="pagination">
                <button type="button" disabled=previous_disabled_view(state) on:click=previous_page_action(state)>"Previous"</button>
                <button type="button" disabled=next_disabled_view(state) on:click=next_page_action(state)>"Next"</button>
            </div>
        </section>
    }
}

fn policy_decision_results(loading: bool, result: Option<Result<UiPolicyDecisionPage, String>>) -> AnyView {
    if loading {
        return view! { <p class="dim" role="status" aria-live="polite">"Loading policy decisions..."</p> }.into_any();
    }
    match result {
        None => view! { <p class="dim">"Enter credentials and search to load decisions."</p> }.into_any(),
        Some(Err(error)) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
        Some(Ok(page)) => policy_decision_page(page).into_any(),
    }
}

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
fn PolicyDecisionFilterFields(
    set_user: WriteSignal<String>,
    set_password: WriteSignal<String>,
    set_filters: WriteSignal<PolicyDecisionFilters>,
    loading: ReadSignal<bool>,
) -> AnyView {
    let _ = (set_user, set_password, set_filters);
    view! {
        <label for="policy-user">"Username"</label>
        <input id="policy-user" autocomplete="username" placeholder="Username" required />
        <label for="policy-password">"Password or write token"</label>
        <input id="policy-password" type="password" autocomplete="off" required />
        <label for="policy-repository">"Repository"</label>
        <input id="policy-repository" maxlength="512" placeholder="All permitted repositories" />
        <label for="policy-state">"State"</label>
        <select id="policy-state">
            <option value="">"Any state"</option>
            <option value="allow">"Allowed"</option>
            <option value="deny">"Denied"</option>
            <option value="wait">"Waiting"</option>
        </select>
        <label for="policy-rule">"Rule"</label><input id="policy-rule" maxlength="512" />
        <label for="policy-source">"Source"</label><input id="policy-source" maxlength="512" />
        <label for="policy-from">"From (UTC)"</label><input id="policy-from" type="datetime-local" />
        <label for="policy-to">"To (UTC)"</label><input id="policy-to" type="datetime-local" />
        <label for="policy-limit">"Rows"</label>
        <select id="policy-limit">
            <option value="25">"25"</option>
            <option value="50">"50"</option>
            <option value="100">"100"</option>
        </select>
        <button type="submit" disabled=move || reactive_value(&loading)>"Search"</button>
    }
    .into_any()
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn PolicyDecisionFilterFields(
    set_user: WriteSignal<String>,
    set_password: WriteSignal<String>,
    set_filters: WriteSignal<PolicyDecisionFilters>,
    loading: ReadSignal<bool>,
) -> impl IntoView {
    view! {
        <label for="policy-user">"Username"</label>
        <input
            id="policy-user"
            autocomplete="username"
            placeholder="Username"
            required
            on:input:target=move |event| set_text(set_user, event.target().value())
        />
        <label for="policy-password">"Password or write token"</label>
        <input
            id="policy-password"
            type="password"
            autocomplete="off"
            required
            on:input:target=move |event| set_text(set_password, event.target().value())
        />
        <label for="policy-repository">"Repository"</label>
        <input
            id="policy-repository"
            maxlength="512"
            placeholder="All permitted repositories"
            on:input:target=move |event| update_filter(set_filters, PolicyDecisionFilterField::Repository, event.target().value())
        />
        <label for="policy-state">"State"</label>
        <select
            id="policy-state"
            on:change:target=move |event| update_filter(set_filters, PolicyDecisionFilterField::State, event.target().value())
        >
            <option value="">"Any state"</option>
            <option value="allow">"Allowed"</option>
            <option value="deny">"Denied"</option>
            <option value="wait">"Waiting"</option>
        </select>
        <label for="policy-rule">"Rule"</label>
        <input
            id="policy-rule"
            maxlength="512"
            on:input:target=move |event| update_filter(set_filters, PolicyDecisionFilterField::Rule, event.target().value())
        />
        <label for="policy-source">"Source"</label>
        <input
            id="policy-source"
            maxlength="512"
            on:input:target=move |event| update_filter(set_filters, PolicyDecisionFilterField::Source, event.target().value())
        />
        <label for="policy-from">"From (UTC)"</label>
        <input
            id="policy-from"
            type="datetime-local"
            on:input:target=move |event| update_filter(set_filters, PolicyDecisionFilterField::From, event.target().value())
        />
        <label for="policy-to">"To (UTC)"</label>
        <input
            id="policy-to"
            type="datetime-local"
            on:input:target=move |event| update_filter(set_filters, PolicyDecisionFilterField::To, event.target().value())
        />
        <label for="policy-limit">"Rows"</label>
        <select
            id="policy-limit"
            on:change:target=move |event| update_filter(set_filters, PolicyDecisionFilterField::Limit, event.target().value())
        >
            <option value="25">"25"</option>
            <option value="50">"50"</option>
            <option value="100">"100"</option>
        </select>
        <button type="submit" disabled=move || reactive_value(&loading)>"Search"</button>
    }
}

#[derive(Clone, Copy)]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
struct PolicyDecisionUi {
    result: WriteSignal<Option<Result<UiPolicyDecisionPage, String>>>,
    loading: WriteSignal<bool>,
}

#[derive(Clone, Copy)]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
struct PolicyDecisionState {
    user: ReadSignal<String>,
    password: ReadSignal<String>,
    filters: ReadSignal<PolicyDecisionFilters>,
    active: ReadSignal<PolicyDecisionFilters>,
    set_active: WriteSignal<PolicyDecisionFilters>,
    cursor: ReadSignal<Option<String>>,
    set_cursor: WriteSignal<Option<String>>,
    previous: ReadSignal<Vec<Option<String>>>,
    set_previous: WriteSignal<Vec<Option<String>>>,
    result: ReadSignal<Option<Result<UiPolicyDecisionPage, String>>>,
    loading: ReadSignal<bool>,
    ui: PolicyDecisionUi,
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn submit(event: &leptos::ev::SubmitEvent, state: PolicyDecisionState) {
    event.prevent_default();
    submit_query(state);
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn submit_query(state: PolicyDecisionState) {
    let filters = state.filters.get_untracked();
    state.set_active.set(filters.clone());
    state.set_cursor.set(None);
    state.set_previous.set(Vec::new());
    run_query(
        &filters,
        None,
        state.user.get_untracked(),
        state.password.get_untracked(),
        state.ui,
    );
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn previous_disabled(state: PolicyDecisionState) -> bool {
    reactive_value(&state.previous).is_empty() || reactive_value(&state.loading)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn previous_disabled_view(state: PolicyDecisionState) -> impl Fn() -> bool {
    move || previous_disabled(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn previous_page(state: PolicyDecisionState) {
    let mut cursors = state.previous.get_untracked();
    if let Some(cursor) = cursors.pop() {
        state.set_previous.set(cursors);
        state.set_cursor.set(cursor.clone());
        run_query(
            &state.active.get_untracked(),
            cursor.as_deref(),
            state.user.get_untracked(),
            state.password.get_untracked(),
            state.ui,
        );
    }
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn previous_page_action<Event>(state: PolicyDecisionState) -> impl FnMut(Event) {
    move |_| previous_page(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_disabled(state: PolicyDecisionState) -> bool {
    reactive_value(&state.loading) || next_cursor(reactive_value(&state.result)).is_none()
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_disabled_view(state: PolicyDecisionState) -> impl Fn() -> bool {
    move || next_disabled(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_page(state: PolicyDecisionState) {
    if let Some(next) = next_cursor(state.result.get_untracked()) {
        state
            .set_previous
            .update(|cursors| cursors.push(state.cursor.get_untracked()));
        state.set_cursor.set(Some(next.clone()));
        run_query(
            &state.active.get_untracked(),
            Some(&next),
            state.user.get_untracked(),
            state.password.get_untracked(),
            state.ui,
        );
    }
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_page_action<Event>(state: PolicyDecisionState) -> impl FnMut(Event) {
    move |_| next_page(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_cursor(result: Option<Result<UiPolicyDecisionPage, String>>) -> Option<String> {
    result.and_then(Result::ok).and_then(|page| page.next_cursor)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn set_text(signal: WriteSignal<String>, value: String) {
    signal.set(value);
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn update_filter(signal: WriteSignal<PolicyDecisionFilters>, field: PolicyDecisionFilterField, value: String) {
    signal.update(|filters| match field {
        PolicyDecisionFilterField::Repository => filters.repository = value,
        PolicyDecisionFilterField::State => filters.state = value,
        PolicyDecisionFilterField::Rule => filters.rule = value,
        PolicyDecisionFilterField::Source => filters.source = value,
        PolicyDecisionFilterField::From => filters.from = value,
        PolicyDecisionFilterField::To => filters.to = value,
        PolicyDecisionFilterField::Limit => filters.limit = value,
    });
}

#[derive(Clone, Copy)]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
enum PolicyDecisionFilterField {
    Repository,
    State,
    Rule,
    Source,
    From,
    To,
    Limit,
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn run_query(
    filters: &PolicyDecisionFilters,
    cursor: Option<&str>,
    user: String,
    password: String,
    ui: PolicyDecisionUi,
) {
    let url = match filters.url(cursor) {
        Ok(url) => url,
        Err(error) => {
            ui.result.set(Some(Err(error)));
            return;
        }
    };
    ui.loading.set(true);
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    leptos::task::spawn_local(async move {
        ui.result
            .set(Some(crate::data::load_policy_decisions(&url, &user, &password).await));
        ui.loading.set(false);
    });
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    {
        let _ = (url, user, password);
        ui.loading.set(false);
    }
}

fn policy_decision_page(page: UiPolicyDecisionPage) -> impl IntoView {
    if page.decisions.is_empty() {
        return Either::Left(
            view! { <p class="dim" role="status" aria-live="polite">"No policy decisions matched these filters."</p> },
        );
    }
    let count = page.decisions.len();
    Either::Right(view! {
        <p class="result-count" role="status" aria-live="polite">{format!("Loaded {count} policy decisions.")}</p>
        <div class="table-scroll">
            <table class="files policy-decisions-table">
                <caption>{format!("{count} policy decisions")}</caption>
                <thead>
                    <tr>
                        <th scope="col">"Outcome"</th>
                        <th scope="col">"Repository"</th>
                        <th scope="col">"Resource"</th>
                        <th scope="col">"Group"</th>
                        <th scope="col">"Artifact"</th>
                        <th scope="col">"Source"</th>
                        <th scope="col">"Action"</th>
                        <th scope="col">"Rule"</th>
                        <th scope="col">"Reason"</th>
                        <th scope="col">"Evaluated (UTC)"</th>
                        <th scope="col">"Next eligible (UTC)"</th>
                    </tr>
                </thead>
                <tbody>{page.decisions.into_iter().map(policy_decision_row).collect_view()}</tbody>
            </table>
        </div>
    })
}

fn policy_decision_row(decision: UiPolicyDecision) -> impl IntoView {
    let outcome = decision.status();
    let class = format!("badge decision-{}", decision.state);
    let evaluated = decision.evaluated_at();
    let next = decision.next_eligible_at();
    view! {
        <tr>
            <td><span class=class>{outcome}</span></td>
            <td><code>{decision.repository}</code></td>
            <td>{decision.resource}</td>
            <td>{or_dash(decision.group)}</td>
            <td>{or_dash(decision.artifact)}</td>
            <td>{or_dash(decision.source)}</td>
            <td>{decision.action}</td>
            <td>{or_dash(decision.rule)}</td>
            <td>{or_dash(decision.reason)}</td>
            <td>{evaluated}</td>
            <td>{next}</td>
        </tr>
    }
}

fn or_dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_owned())
}

#[cfg(test)]
#[cfg(feature = "ssr")]
#[path = "../../tests/unit/pages/policy_decisions/tests.rs"]
mod tests;
