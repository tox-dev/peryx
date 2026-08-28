use leptos::prelude::*;

use super::reactive_value;
use crate::model::TrashFilters;
use crate::model::{UiTrashPage, UiTrashRecord};

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
pub fn Trash() -> impl IntoView {
    let (_, set_user) = signal(String::new());
    let (_, set_password) = signal(String::new());
    let (_, set_filters) = signal(TrashFilters::default());
    let (result, _) = signal(None::<Result<UiTrashPage, String>>);
    let (loading, _) = signal(false);
    view! {
        <section class="page trash-page">
            <div class="ops-title">
                <h1>"Trash"</h1>
                <span class="badge">"read-only"</span>
            </div>
            <p class="dim">
                "Inspect soft-deleted artifacts and whether each can still be restored. Times are UTC. Credentials remain in this browser tab and are sent only in the authorization header."
            </p>
            <form class="policy-filters">
                <TrashFilterFields set_user set_password set_filters loading />
            </form>
            <div class="policy-results">{move || trash_results(reactive_value(&loading), reactive_value(&result))}</div>
            <div class="pagination">
                <button type="button" disabled=move || true>"Previous"</button>
                <button type="button" disabled=move || true>"Next"</button>
            </div>
        </section>
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
pub fn Trash() -> impl IntoView {
    let (user, set_user) = signal(String::new());
    let (password, set_password) = signal(String::new());
    let (filters, set_filters) = signal(TrashFilters::default());
    let (active, set_active) = signal(TrashFilters::default());
    let (cursor, set_cursor) = signal(None::<String>);
    let (previous, set_previous) = signal(Vec::<Option<String>>::new());
    let (result, set_result) = signal(None::<Result<UiTrashPage, String>>);
    let (loading, set_loading) = signal(false);
    let ui = TrashUi {
        result: set_result,
        loading: set_loading,
    };
    let state = TrashState {
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
        <section class="page trash-page">
            <div class="ops-title">
                <h1>"Trash"</h1>
                <span class="badge">"read-only"</span>
            </div>
            <p class="dim">
                "Inspect soft-deleted artifacts and whether each can still be restored. Times are UTC. Credentials remain in this browser tab and are sent only in the authorization header."
            </p>
            <form class="policy-filters" on:submit=move |event| submit(&event, state)>
                <TrashFilterFields set_user set_password set_filters loading />
            </form>
            <div class="policy-results">
                {trash_results_view(state)}
            </div>
            <div class="pagination">
                <button type="button" disabled=previous_disabled_view(state) on:click=previous_page_action(state)>"Previous"</button>
                <button type="button" disabled=next_disabled_view(state) on:click=next_page_action(state)>"Next"</button>
            </div>
        </section>
    }
}

fn trash_results(loading: bool, result: Option<Result<UiTrashPage, String>>) -> AnyView {
    if loading {
        return view! { <p class="dim" role="status" aria-live="polite">"Loading trash..."</p> }.into_any();
    }
    match result {
        None => view! { <p class="dim">"Enter credentials and search to load trash records."</p> }.into_any(),
        Some(Err(error)) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
        Some(Ok(page)) => trash_page(page),
    }
}

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
fn TrashFilterFields(
    set_user: WriteSignal<String>,
    set_password: WriteSignal<String>,
    set_filters: WriteSignal<TrashFilters>,
    loading: ReadSignal<bool>,
) -> impl IntoView {
    let _ = (set_user, set_password, set_filters);
    view! {
        <label for="trash-user">"Username"</label>
        <input id="trash-user" autocomplete="username" placeholder="Administrator or token user" required />
        <label for="trash-password">"Password or token"</label>
        <input id="trash-password" type="password" autocomplete="off" required />
        <label for="trash-repository">"Repository"</label>
        <input id="trash-repository" maxlength="512" placeholder="All permitted repositories" />
        <label for="trash-ecosystem">"Ecosystem"</label>
        <input id="trash-ecosystem" placeholder="Any ecosystem" />
        <label for="trash-state">"State"</label>
        <select id="trash-state">
            <option value="">"Any state"</option>
            <option value="restorable">"Restorable"</option>
            <option value="expired">"Expired"</option>
        </select>
        <label for="trash-limit">"Rows"</label>
        <select id="trash-limit">
            <option value="25">"25"</option>
            <option value="50">"50"</option>
            <option value="100">"100"</option>
        </select>
        <button type="submit" disabled=move || reactive_value(&loading)>"Search"</button>
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn TrashFilterFields(
    set_user: WriteSignal<String>,
    set_password: WriteSignal<String>,
    set_filters: WriteSignal<TrashFilters>,
    loading: ReadSignal<bool>,
) -> impl IntoView {
    view! {
        <label for="trash-user">"Username"</label>
        <input
            id="trash-user"
            autocomplete="username"
            placeholder="Administrator or token user"
            required
            on:input:target=move |event| set_text(set_user, event.target().value())
        />
        <label for="trash-password">"Password or token"</label>
        <input
            id="trash-password"
            type="password"
            autocomplete="off"
            required
            on:input:target=move |event| set_text(set_password, event.target().value())
        />
        <label for="trash-repository">"Repository"</label>
        <input
            id="trash-repository"
            maxlength="512"
            placeholder="All permitted repositories"
            on:input:target=move |event| update_filter(set_filters, TrashFilterField::Repository, event.target().value())
        />
        <label for="trash-ecosystem">"Ecosystem"</label>
        <input
            id="trash-ecosystem"
            placeholder="Any ecosystem"
            on:input:target=move |event| update_filter(set_filters, TrashFilterField::Ecosystem, event.target().value())
        />
        <label for="trash-state">"State"</label>
        <select
            id="trash-state"
            on:change:target=move |event| update_filter(set_filters, TrashFilterField::State, event.target().value())
        >
            <option value="">"Any state"</option>
            <option value="restorable">"Restorable"</option>
            <option value="expired">"Expired"</option>
        </select>
        <label for="trash-limit">"Rows"</label>
        <select
            id="trash-limit"
            on:change:target=move |event| update_filter(set_filters, TrashFilterField::Limit, event.target().value())
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
struct TrashUi {
    result: WriteSignal<Option<Result<UiTrashPage, String>>>,
    loading: WriteSignal<bool>,
}

#[derive(Clone, Copy)]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
struct TrashState {
    user: ReadSignal<String>,
    password: ReadSignal<String>,
    filters: ReadSignal<TrashFilters>,
    active: ReadSignal<TrashFilters>,
    set_active: WriteSignal<TrashFilters>,
    cursor: ReadSignal<Option<String>>,
    set_cursor: WriteSignal<Option<String>>,
    previous: ReadSignal<Vec<Option<String>>>,
    set_previous: WriteSignal<Vec<Option<String>>>,
    result: ReadSignal<Option<Result<UiTrashPage, String>>>,
    loading: ReadSignal<bool>,
    ui: TrashUi,
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn submit(event: &leptos::ev::SubmitEvent, state: TrashState) {
    event.prevent_default();
    submit_query(state);
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn submit_query(state: TrashState) {
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
fn trash_results_view(state: TrashState) -> impl Fn() -> AnyView {
    move || trash_results(reactive_value(&state.loading), reactive_value(&state.result))
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn previous_disabled(state: TrashState) -> bool {
    reactive_value(&state.previous).is_empty() || reactive_value(&state.loading)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn previous_disabled_view(state: TrashState) -> impl Fn() -> bool {
    move || previous_disabled(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn previous_page(state: TrashState) {
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
fn previous_page_action<Event>(state: TrashState) -> impl FnMut(Event) {
    move |_| previous_page(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_disabled(state: TrashState) -> bool {
    reactive_value(&state.loading) || next_cursor(reactive_value(&state.result)).is_none()
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_disabled_view(state: TrashState) -> impl Fn() -> bool {
    move || next_disabled(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_page(state: TrashState) {
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
fn next_page_action<Event>(state: TrashState) -> impl FnMut(Event) {
    move |_| next_page(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_cursor(result: Option<Result<UiTrashPage, String>>) -> Option<String> {
    result.and_then(Result::ok).and_then(|page| page.next_cursor)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn set_text(signal: WriteSignal<String>, value: String) {
    signal.set(value);
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn update_filter(signal: WriteSignal<TrashFilters>, field: TrashFilterField, value: String) {
    signal.update(|filters| match field {
        TrashFilterField::Repository => filters.repository = value,
        TrashFilterField::Ecosystem => filters.ecosystem = value,
        TrashFilterField::State => filters.state = value,
        TrashFilterField::Limit => filters.limit = value,
    });
}

#[derive(Clone, Copy)]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
enum TrashFilterField {
    Repository,
    Ecosystem,
    State,
    Limit,
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn run_query(filters: &TrashFilters, cursor: Option<&str>, user: String, password: String, ui: TrashUi) {
    let url = filters.url(cursor);
    ui.loading.set(true);
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    leptos::task::spawn_local(async move {
        ui.result
            .set(Some(crate::data::load_trash(&url, &user, &password).await));
        ui.loading.set(false);
    });
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    {
        let _ = (url, user, password, ui.result);
        ui.loading.set(false);
    }
}

fn trash_page(page: UiTrashPage) -> AnyView {
    if page.trash.is_empty() {
        return view! { <p class="dim" role="status" aria-live="polite">"No trash records matched these filters."</p> }
            .into_any();
    }
    let count = page.trash.len();
    view! {
        <p class="result-count" role="status" aria-live="polite">{format!("Loaded {count} trash records.")}</p>
        <div class="table-scroll">
            <table class="files trash-table">
                <caption>{format!("{count} trash records")}</caption>
                <thead>
                    <tr>
                        <th scope="col">"State"</th>
                        <th scope="col">"Ecosystem"</th>
                        <th scope="col">"Repository"</th>
                        <th scope="col">"Resource"</th>
                        <th scope="col">"Artifact"</th>
                        <th scope="col">"Digest"</th>
                        <th scope="col">"Reason"</th>
                        <th scope="col">"Actor"</th>
                        <th scope="col">"Deleted (UTC)"</th>
                        <th scope="col">"Restorable until (UTC)"</th>
                    </tr>
                </thead>
                <tbody>{page.trash.into_iter().map(trash_row).collect_view()}</tbody>
            </table>
        </div>
    }
    .into_any()
}

fn trash_row(record: UiTrashRecord) -> impl IntoView {
    let state_class = format!("badge trash-{}", record.state);
    let state_label = record.state_label();
    let deleted = record.deleted_at();
    let deadline = record.deadline_at();
    view! {
        <tr>
            <td><span class=state_class>{state_label}</span></td>
            <td>{record.ecosystem}</td>
            <td><code>{record.repository}</code></td>
            <td>{record.resource}</td>
            <td>{or_dash(record.artifact)}</td>
            <td>{or_dash(record.digest)}</td>
            <td>{or_dash(record.reason)}</td>
            <td>{or_dash(record.actor)}</td>
            <td>{deleted}</td>
            <td>{deadline}</td>
        </tr>
    }
}

fn or_dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_owned())
}

#[cfg(test)]
#[cfg(feature = "ssr")]
#[path = "../../tests/unit/pages/trash/tests.rs"]
mod tests;
