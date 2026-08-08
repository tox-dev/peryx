#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;

use crate::model::{TrashFilters, UiTrashPage, UiTrashRecord};

#[component]
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
    view! {
        <section class="page trash-page">
            <div class="ops-title">
                <h1>"Trash"</h1>
                <span class="badge">"read-only"</span>
            </div>
            <p class="dim">
                "Inspect soft-deleted artifacts and whether each can still be restored. Times are UTC. Credentials remain in this browser tab and are sent only in the authorization header."
            </p>
            <form class="policy-filters" on:submit=move |event| {
                event.prevent_default();
                let filters = filters.get_untracked();
                set_active.set(filters.clone());
                set_cursor.set(None);
                set_previous.set(Vec::new());
                run_query(&filters, None, user.get_untracked(), password.get_untracked(), ui);
            }>
                <TrashFilterFields set_user set_password set_filters loading />
            </form>
            <div class="policy-results">
                {move || {
                    if loading.get() {
                        return view! { <p class="dim" role="status" aria-live="polite">"Loading trash..."</p> }.into_any();
                    }
                    match result.get() {
                        None => view! { <p class="dim">"Enter credentials and search to load trash records."</p> }.into_any(),
                        Some(Err(error)) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
                        Some(Ok(page)) => trash_page(page),
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
            on:input:target=move |event| set_user.set(event.target().value())
        />
        <label for="trash-password">"Password or token"</label>
        <input
            id="trash-password"
            type="password"
            autocomplete="off"
            required
            on:input:target=move |event| set_password.set(event.target().value())
        />
        <label for="trash-repository">"Repository"</label>
        <input
            id="trash-repository"
            maxlength="512"
            placeholder="All permitted repositories"
            on:input:target=move |event| set_filters.update(|value| value.repository = event.target().value())
        />
        <label for="trash-ecosystem">"Ecosystem"</label>
        <input
            id="trash-ecosystem"
            placeholder="Any ecosystem"
            on:input:target=move |event| set_filters.update(|value| value.ecosystem = event.target().value())
        />
        <label for="trash-state">"State"</label>
        <select
            id="trash-state"
            on:change:target=move |event| set_filters.update(|value| value.state = event.target().value())
        >
            <option value="">"Any state"</option>
            <option value="restorable">"Restorable"</option>
            <option value="expired">"Expired"</option>
        </select>
        <label for="trash-limit">"Rows"</label>
        <select
            id="trash-limit"
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
struct TrashUi {
    result: WriteSignal<Option<Result<UiTrashPage, String>>>,
    loading: WriteSignal<bool>,
}

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
                        <th scope="col">"Artifact"</th>
                        <th scope="col">"Reference"</th>
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
            <td>{record.name}</td>
            <td>{or_dash(record.reference)}</td>
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
