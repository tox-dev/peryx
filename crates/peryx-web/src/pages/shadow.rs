#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;

use crate::model::{ShadowInspectionFilters, UiShadowCandidate, UiShadowDecision, UiShadowPage};

/// Inspect how one virtual repository resolves a project: the selected candidate for each filename,
/// the members it shadowed and why, and the allow, deny, or wait decision that governs the filename.
#[component]
pub fn ShadowInspection() -> impl IntoView {
    let (user, set_user) = signal(String::new());
    let (password, set_password) = signal(String::new());
    let (filters, set_filters) = signal(ShadowInspectionFilters::default());
    let (active, set_active) = signal(ShadowInspectionFilters::default());
    let (cursor, set_cursor) = signal(None::<String>);
    let (previous, set_previous) = signal(Vec::<Option<String>>::new());
    let (result, set_result) = signal(None::<Result<UiShadowPage, String>>);
    let (loading, set_loading) = signal(false);
    let ui = ShadowUi {
        result: set_result,
        loading: set_loading,
    };
    view! {
        <section class="page shadow-inspection-page">
            <div class="ops-title">
                <h1>"Shadowed candidates"</h1>
                <span class="badge">"read-only"</span>
            </div>
            <p class="dim">
                "Explain a virtual repository's resolution of one project: which member serves each file, which candidates it shadows, and whether policy allows, denies, or holds each one. A repository operator sees only repositories they may read; an administrator may inspect any. Credentials remain in this browser tab and are sent only in the authorization header."
            </p>
            <form class="policy-filters" on:submit=move |event| {
                event.prevent_default();
                let filters = filters.get_untracked();
                set_active.set(filters.clone());
                set_cursor.set(None);
                set_previous.set(Vec::new());
                run_query(&filters, None, user.get_untracked(), password.get_untracked(), ui);
            }>
                <ShadowFilterFields set_user set_password set_filters loading />
            </form>
            <div class="policy-results">
                {move || {
                    if loading.get() {
                        return view! { <p class="dim" role="status" aria-live="polite">"Loading shadowed candidates..."</p> }.into_any();
                    }
                    match result.get() {
                        None => view! { <p class="dim">"Enter credentials, a repository, and a project to inspect resolution."</p> }.into_any(),
                        Some(Err(error)) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
                        Some(Ok(page)) => shadow_candidate_page(page),
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
fn ShadowFilterFields(
    set_user: WriteSignal<String>,
    set_password: WriteSignal<String>,
    set_filters: WriteSignal<ShadowInspectionFilters>,
    loading: ReadSignal<bool>,
) -> impl IntoView {
    view! {
        <label for="shadow-user">"Username"</label>
        <input
            id="shadow-user"
            autocomplete="username"
            placeholder="Local user or __token__"
            required
            on:input:target=move |event| set_user.set(event.target().value())
        />
        <label for="shadow-password">"Password or upload token"</label>
        <input
            id="shadow-password"
            type="password"
            autocomplete="off"
            required
            on:input:target=move |event| set_password.set(event.target().value())
        />
        <label for="shadow-repository">"Repository"</label>
        <input
            id="shadow-repository"
            maxlength="512"
            placeholder="root/packages"
            required
            on:input:target=move |event| set_filters.update(|value| value.repository = event.target().value())
        />
        <label for="shadow-project">"Project"</label>
        <input
            id="shadow-project"
            maxlength="512"
            placeholder="normalized name"
            required
            on:input:target=move |event| set_filters.update(|value| value.project = event.target().value())
        />
        <label for="shadow-limit">"Rows"</label>
        <select
            id="shadow-limit"
            on:change:target=move |event| set_filters.update(|value| value.limit = event.target().value())
        >
            <option value="25">"25"</option>
            <option value="50">"50"</option>
            <option value="100">"100"</option>
        </select>
        <button type="submit" disabled=move || loading.get()>"Inspect"</button>
    }
}

#[derive(Clone, Copy)]
struct ShadowUi {
    result: WriteSignal<Option<Result<UiShadowPage, String>>>,
    loading: WriteSignal<bool>,
}

fn run_query(filters: &ShadowInspectionFilters, cursor: Option<&str>, user: String, password: String, ui: ShadowUi) {
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
            .set(Some(crate::data::load_shadow_candidates(&url, &user, &password).await));
        ui.loading.set(false);
    });
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    {
        let _ = (url, user, password);
        ui.loading.set(false);
    }
}

fn shadow_candidate_page(page: UiShadowPage) -> AnyView {
    if page.candidates.is_empty() {
        return view! { <p class="dim" role="status" aria-live="polite">"No candidates resolved for this repository and project."</p> }
            .into_any();
    }
    let count = page.candidates.len();
    view! {
        <p class="result-count" role="status" aria-live="polite">{format!("Loaded {count} candidates.")}</p>
        <div class="table-scroll">
            <table class="files shadow-inspection-table">
                <caption>{format!("{count} resolution candidates")}</caption>
                <thead>
                    <tr>
                        <th scope="col">"Outcome"</th>
                        <th scope="col">"Decision"</th>
                        <th scope="col">"Source"</th>
                        <th scope="col">"Member"</th>
                        <th scope="col">"File"</th>
                        <th scope="col">"Digest"</th>
                        <th scope="col">"Shadowed because"</th>
                        <th scope="col">"Rule"</th>
                        <th scope="col">"Reason"</th>
                        <th scope="col">"Next eligible (UTC)"</th>
                    </tr>
                </thead>
                <tbody>{page.candidates.into_iter().map(shadow_candidate_row).collect_view()}</tbody>
            </table>
        </div>
    }
    .into_any()
}

fn shadow_candidate_row(candidate: UiShadowCandidate) -> impl IntoView {
    let outcome_class = format!("badge outcome-{}", candidate.outcome_key());
    let outcome = candidate.outcome();
    let source = candidate.source_text().to_owned();
    let reason = candidate.reason_text();
    let digest = candidate.digest_text();
    let decision = candidate.decision;
    let rule = decision.as_ref().and_then(|decision| decision.rule.clone());
    let reason_text = decision.as_ref().and_then(|decision| decision.reason.clone());
    let next = decision
        .as_ref()
        .map_or_else(|| "-".to_owned(), UiShadowDecision::next_eligible_at);
    let decision_cell = decision.map_or_else(
        || view! { <span class="dim">"-"</span> }.into_any(),
        |decision| {
            let class = format!("badge decision-{}", decision.state_key());
            view! { <span class=class>{decision.status()}</span> }.into_any()
        },
    );
    view! {
        <tr>
            <td><span class=outcome_class>{outcome}</span></td>
            <td>{decision_cell}</td>
            <td>{source}</td>
            <td><code>{candidate.member}</code></td>
            <td>{candidate.filename}</td>
            <td><code>{digest}</code></td>
            <td>{reason}</td>
            <td>{or_dash(rule)}</td>
            <td>{or_dash(reason_text)}</td>
            <td>{next}</td>
        </tr>
    }
}

fn or_dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_owned())
}
