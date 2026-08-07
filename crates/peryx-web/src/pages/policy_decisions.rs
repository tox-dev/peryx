#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;

use crate::model::{PolicyDecisionFilters, UiPolicyDecision, UiPolicyDecisionPage};

#[component]
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
    view! {
        <section class="page policy-decisions-page">
            <div class="ops-title">
                <h1>"Policy decisions"</h1>
                <span class="badge">"read-only"</span>
            </div>
            <p class="dim">
                "Inspect recorded allow, deny, and wait outcomes. Times are UTC. Credentials remain in this browser tab and are sent only in the authorization header."
            </p>
            <form class="policy-filters" on:submit=move |event| {
                event.prevent_default();
                let filters = filters.get_untracked();
                set_active.set(filters.clone());
                set_cursor.set(None);
                set_previous.set(Vec::new());
                run_query(&filters, None, user.get_untracked(), password.get_untracked(), ui);
            }>
                <PolicyDecisionFilterFields set_user set_password set_filters loading />
            </form>
            <div class="policy-results">
                {move || {
                    if loading.get() {
                        return view! { <p class="dim" role="status" aria-live="polite">"Loading policy decisions..."</p> }.into_any();
                    }
                    match result.get() {
                        None => view! { <p class="dim">"Enter credentials and search to load decisions."</p> }.into_any(),
                        Some(Err(error)) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
                        Some(Ok(page)) => policy_decision_page(page),
                    }
                }}
            </div>
            <div class="pagination">
                <button type="button" disabled=move || previous.get().is_empty() || loading.get() on:click=move |_| {
                    let mut cursors = previous.get_untracked();
                    if let Some(cursor) = cursors.pop() {
                        set_previous.set(cursors);
                        set_cursor.set(cursor.clone());
                        run_query(&active.get_untracked(), cursor.as_deref(), user.get_untracked(), password.get_untracked(), ui);
                    }
                }>"Previous"</button>
                <button type="button" disabled=move || {
                    loading.get()
                        || result
                            .get()
                            .and_then(Result::ok)
                            .and_then(|page| page.next_cursor)
                            .is_none()
                } on:click=move |_| {
                    let next = result
                        .get_untracked()
                        .and_then(Result::ok)
                        .and_then(|page| page.next_cursor);
                    if let Some(next) = next {
                        set_previous.update(|cursors| cursors.push(cursor.get_untracked()));
                        set_cursor.set(Some(next.clone()));
                        run_query(&active.get_untracked(), Some(&next), user.get_untracked(), password.get_untracked(), ui);
                    }
                }>"Next"</button>
            </div>
        </section>
    }
}

#[component]
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
            placeholder="Local user or __token__"
            required
            on:input:target=move |event| set_user.set(event.target().value())
        />
        <label for="policy-password">"Password or upload token"</label>
        <input
            id="policy-password"
            type="password"
            autocomplete="off"
            required
            on:input:target=move |event| set_password.set(event.target().value())
        />
        <label for="policy-repository">"Repository"</label>
        <input
            id="policy-repository"
            maxlength="512"
            placeholder="All permitted repositories"
            on:input:target=move |event| set_filters.update(|value| value.repository = event.target().value())
        />
        <label for="policy-state">"State"</label>
        <select
            id="policy-state"
            on:change:target=move |event| set_filters.update(|value| value.state = event.target().value())
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
            on:input:target=move |event| set_filters.update(|value| value.rule = event.target().value())
        />
        <label for="policy-source">"Source"</label>
        <input
            id="policy-source"
            maxlength="512"
            on:input:target=move |event| set_filters.update(|value| value.source = event.target().value())
        />
        <label for="policy-from">"From (UTC)"</label>
        <input
            id="policy-from"
            type="datetime-local"
            on:input:target=move |event| set_filters.update(|value| value.from = event.target().value())
        />
        <label for="policy-to">"To (UTC)"</label>
        <input
            id="policy-to"
            type="datetime-local"
            on:input:target=move |event| set_filters.update(|value| value.to = event.target().value())
        />
        <label for="policy-limit">"Rows"</label>
        <select
            id="policy-limit"
            on:change:target=move |event| set_filters.update(|value| value.limit = event.target().value())
        >
            <option value="25">"25"</option>
            <option value="50">"50"</option>
            <option value="100">"100"</option>
        </select>
        <button type="submit" disabled=move || loading.get()>"Search"</button>
    }
}

#[derive(Clone, Copy)]
struct PolicyDecisionUi {
    result: WriteSignal<Option<Result<UiPolicyDecisionPage, String>>>,
    loading: WriteSignal<bool>,
}

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

fn policy_decision_page(page: UiPolicyDecisionPage) -> AnyView {
    if page.decisions.is_empty() {
        return view! { <p class="dim" role="status" aria-live="polite">"No policy decisions matched these filters."</p> }
            .into_any();
    }
    let count = page.decisions.len();
    view! {
        <p class="result-count" role="status" aria-live="polite">{format!("Loaded {count} policy decisions.")}</p>
        <div class="table-scroll">
            <table class="files policy-decisions-table">
                <caption>{format!("{count} policy decisions")}</caption>
                <thead>
                    <tr>
                        <th scope="col">"Outcome"</th>
                        <th scope="col">"Repository"</th>
                        <th scope="col">"Package"</th>
                        <th scope="col">"Version"</th>
                        <th scope="col">"File"</th>
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
    }
    .into_any()
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
            <td>{decision.project}</td>
            <td>{or_dash(decision.version)}</td>
            <td>{or_dash(decision.filename)}</td>
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
