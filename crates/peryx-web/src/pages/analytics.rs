use leptos::either::{Either, EitherOf4, EitherOf5};
use leptos::prelude::*;

use super::human_size;
use crate::model::AnalyticsFilters;
use crate::model::{
    UiGroupRow, UiInterval, UiResourceRow, UiSourceRow, UiTimelineRow, UiUnusedRow, UiUsagePage, UiUsageRows,
    format_instant,
};

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
pub fn UsageAnalytics() -> impl IntoView {
    let (_, set_user) = signal(String::new());
    let (_, set_password) = signal(String::new());
    let (_, set_filters) = signal(AnalyticsFilters::default());
    let (result, _) = signal(None::<Result<UiUsagePage, String>>);
    let (loading, _) = signal(false);
    view! {
        <section class="page analytics-page">
            <div class="ops-title">
                <h1>"Usage analytics"</h1>
                <span class="badge">"read-only"</span>
            </div>
            <p class="dim">
                "Query the retained daily read aggregate. Times are UTC. The source split needs operator access; \
                 other views accept a repository grant. Credentials remain in this browser tab and are sent only in \
                 the authorization header."
            </p>
            <form class="policy-filters analytics-filters">
                <AnalyticsFilterFields set_user set_password set_filters loading />
            </form>
            <div class="analytics-results">{move || analytics_results(loading.get(), result.get())}</div>
            <div class="pagination">
                <button type="button" disabled=move || true>"Previous"</button>
                <button type="button" disabled=move || true>"Next"</button>
            </div>
        </section>
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
pub fn UsageAnalytics() -> impl IntoView {
    let (user, set_user) = signal(String::new());
    let (password, set_password) = signal(String::new());
    let (filters, set_filters) = signal(AnalyticsFilters::default());
    let (active, set_active) = signal(AnalyticsFilters::default());
    let (cursor, set_cursor) = signal(None::<String>);
    let (previous, set_previous) = signal(Vec::<Option<String>>::new());
    let (result, set_result) = signal(None::<Result<UiUsagePage, String>>);
    let (loading, set_loading) = signal(false);
    let ui = AnalyticsUi {
        result: set_result,
        loading: set_loading,
    };
    let state = AnalyticsState {
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
        <section class="page analytics-page">
            <div class="ops-title">
                <h1>"Usage analytics"</h1>
                <span class="badge">"read-only"</span>
            </div>
            <p class="dim">
                "Query the retained daily read aggregate. Times are UTC. The source split needs operator access; \
                 other views accept a repository grant. Credentials remain in this browser tab and are sent only in \
                 the authorization header."
            </p>
            <form class="policy-filters analytics-filters" on:submit=move |event| submit(&event, state)>
                <AnalyticsFilterFields set_user set_password set_filters loading />
            </form>
            <div class="analytics-results">
                {move || analytics_results(state.loading.get(), state.result.get())}
            </div>
            <div class="pagination">
                <button type="button" disabled=previous_disabled_view(state) on:click=previous_page_action(state)>
                    "Previous"
                </button>
                <button type="button" disabled=next_disabled_view(state) on:click=next_page_action(state)>
                    "Next"
                </button>
            </div>
        </section>
    }
}

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
fn AnalyticsFilterFields(
    set_user: WriteSignal<String>,
    set_password: WriteSignal<String>,
    set_filters: WriteSignal<AnalyticsFilters>,
    loading: ReadSignal<bool>,
) -> impl IntoView {
    let _ = (set_user, set_password, set_filters);
    view! {
        <label for="analytics-user">"Username"</label>
        <input id="analytics-user" autocomplete="username" placeholder="Username" required />
        <label for="analytics-password">"Credential"</label>
        <input id="analytics-password" type="password" autocomplete="off" required />
        <label for="analytics-view">"View"</label>
        <select id="analytics-view">
            <option value="top">"Top resources"</option>
            <option value="groups">"Group usage"</option>
            <option value="sources">"Source split"</option>
            <option value="unused">"Unused resources"</option>
            <option value="timeline">"Timeline"</option>
        </select>
        <label for="analytics-repository">"Repository"</label>
        <input id="analytics-repository" maxlength="512" placeholder="All permitted repositories" />
        <label for="analytics-from">"From (UTC day)"</label>
        <input id="analytics-from" type="date" />
        <label for="analytics-to">"To (UTC day)"</label>
        <input id="analytics-to" type="date" />
        <label for="analytics-limit">"Rows"</label>
        <select id="analytics-limit">
            <option value="25">"25"</option>
            <option value="50">"50"</option>
            <option value="100">"100"</option>
        </select>
        <button type="submit" disabled=move || loading.get()>"Search"</button>
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn AnalyticsFilterFields(
    set_user: WriteSignal<String>,
    set_password: WriteSignal<String>,
    set_filters: WriteSignal<AnalyticsFilters>,
    loading: ReadSignal<bool>,
) -> impl IntoView {
    view! {
        <label for="analytics-user">"Username"</label>
        <input
            id="analytics-user"
            autocomplete="username"
            placeholder="Username"
            required
            on:input:target=move |event| set_text(set_user, event.target().value())
        />
        <label for="analytics-password">"Credential"</label>
        <input
            id="analytics-password"
            type="password"
            autocomplete="off"
            required
            on:input:target=move |event| set_text(set_password, event.target().value())
        />
        <label for="analytics-view">"View"</label>
        <select
            id="analytics-view"
            on:change:target=move |event| update_filter(set_filters, AnalyticsFilterField::View, event.target().value())
        >
            <option value="top">"Top resources"</option>
            <option value="groups">"Group usage"</option>
            <option value="sources">"Source split"</option>
            <option value="unused">"Unused resources"</option>
            <option value="timeline">"Timeline"</option>
        </select>
        <label for="analytics-repository">"Repository"</label>
        <input
            id="analytics-repository"
            maxlength="512"
            placeholder="All permitted repositories"
            on:input:target=move |event| update_filter(set_filters, AnalyticsFilterField::Repository, event.target().value())
        />
        <label for="analytics-from">"From (UTC day)"</label>
        <input
            id="analytics-from"
            type="date"
            on:input:target=move |event| update_filter(set_filters, AnalyticsFilterField::From, event.target().value())
        />
        <label for="analytics-to">"To (UTC day)"</label>
        <input
            id="analytics-to"
            type="date"
            on:input:target=move |event| update_filter(set_filters, AnalyticsFilterField::To, event.target().value())
        />
        <label for="analytics-limit">"Rows"</label>
        <select
            id="analytics-limit"
            on:change:target=move |event| update_filter(set_filters, AnalyticsFilterField::Limit, event.target().value())
        >
            <option value="25">"25"</option>
            <option value="50">"50"</option>
            <option value="100">"100"</option>
        </select>
        <button type="submit" disabled=move || loading.get()>"Search"</button>
    }
}

#[derive(Clone, Copy)]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
struct AnalyticsUi {
    result: WriteSignal<Option<Result<UiUsagePage, String>>>,
    loading: WriteSignal<bool>,
}

#[derive(Clone, Copy)]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
struct AnalyticsState {
    user: ReadSignal<String>,
    password: ReadSignal<String>,
    filters: ReadSignal<AnalyticsFilters>,
    active: ReadSignal<AnalyticsFilters>,
    set_active: WriteSignal<AnalyticsFilters>,
    cursor: ReadSignal<Option<String>>,
    set_cursor: WriteSignal<Option<String>>,
    previous: ReadSignal<Vec<Option<String>>>,
    set_previous: WriteSignal<Vec<Option<String>>>,
    result: ReadSignal<Option<Result<UiUsagePage, String>>>,
    loading: ReadSignal<bool>,
    ui: AnalyticsUi,
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn submit(event: &leptos::ev::SubmitEvent, state: AnalyticsState) {
    event.prevent_default();
    submit_query(state);
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn submit_query(state: AnalyticsState) {
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

fn analytics_results(loading: bool, result: Option<Result<UiUsagePage, String>>) -> impl IntoView {
    if loading {
        return EitherOf4::A(
            view! { <p class="dim" role="status" aria-live="polite">"Loading usage analytics..."</p> },
        );
    }
    match result {
        None => EitherOf4::B(view! { <p class="dim">"Enter credentials and search to load usage."</p> }),
        Some(Err(error)) => EitherOf4::C(view! { <p class="error" role="alert">{error}</p> }),
        Some(Ok(page)) => EitherOf4::D(usage_page(page)),
    }
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn previous_disabled(state: AnalyticsState) -> bool {
    state.previous.get().is_empty() || state.loading.get()
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn previous_disabled_view(state: AnalyticsState) -> impl Fn() -> bool {
    move || previous_disabled(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn previous_page(state: AnalyticsState) {
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
fn previous_page_action<Event>(state: AnalyticsState) -> impl FnMut(Event) {
    move |_| previous_page(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_disabled(state: AnalyticsState) -> bool {
    state.loading.get() || next_cursor(state.result.get()).is_none()
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_disabled_view(state: AnalyticsState) -> impl Fn() -> bool {
    move || next_disabled(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_page(state: AnalyticsState) {
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
fn next_page_action<Event>(state: AnalyticsState) -> impl FnMut(Event) {
    move |_| next_page(state)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn next_cursor(result: Option<Result<UiUsagePage, String>>) -> Option<String> {
    result.and_then(Result::ok).and_then(|page| page.next_cursor)
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn set_text(signal: WriteSignal<String>, value: String) {
    signal.set(value);
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn update_filter(signal: WriteSignal<AnalyticsFilters>, field: AnalyticsFilterField, value: String) {
    signal.update(|filters| match field {
        AnalyticsFilterField::View => filters.view = value,
        AnalyticsFilterField::Repository => filters.repository = value,
        AnalyticsFilterField::From => filters.from = value,
        AnalyticsFilterField::To => filters.to = value,
        AnalyticsFilterField::Limit => filters.limit = value,
    });
}

#[derive(Clone, Copy)]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
enum AnalyticsFilterField {
    View,
    Repository,
    From,
    To,
    Limit,
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn run_query(filters: &AnalyticsFilters, cursor: Option<&str>, user: String, password: String, ui: AnalyticsUi) {
    let url = match filters.url(cursor) {
        Ok(url) => url,
        Err(error) => {
            ui.loading.set(false);
            ui.result.set(Some(Err(error)));
            return;
        }
    };
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        let view = filters.view();
        ui.loading.set(true);
        leptos::task::spawn_local(load_analytics(url, view, user, password, ui));
    }
    #[cfg(not(all(not(feature = "ssr"), feature = "hydrate")))]
    drop((url, user, password));
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
async fn load_analytics(
    url: String,
    view: crate::model::AnalyticsView,
    user: String,
    password: String,
    ui: AnalyticsUi,
) {
    ui.result
        .set(Some(crate::data::load_analytics(&url, view, &user, &password).await));
    ui.loading.set(false);
}

fn usage_page(page: UiUsagePage) -> impl IntoView {
    let interval = interval_banner(&page.interval);
    let count = page.rows.len();
    if page.rows.is_empty() {
        return Either::Left(view! {
            {interval}
            <p class="dim" role="status" aria-live="polite">"No usage recorded for this view in the resolved window."</p>
        });
    }
    let table = match page.rows {
        UiUsageRows::Top(rows) => EitherOf5::A(top_table(rows)),
        UiUsageRows::Groups(rows) => EitherOf5::B(groups_table(rows)),
        UiUsageRows::Sources(rows) => EitherOf5::C(sources_table(rows)),
        UiUsageRows::Unused(rows) => EitherOf5::D(unused_table(rows)),
        UiUsageRows::Timeline(rows) => EitherOf5::E(timeline_table(rows)),
    };
    Either::Right(view! {
        {interval}
        <p class="result-count" role="status" aria-live="polite">{format!("Loaded {count} rows.")}</p>
        <div class="table-scroll">{table}</div>
    })
}

fn interval_banner(interval: &UiInterval) -> impl IntoView + use<> {
    let window = interval.window();
    let retention = if interval.window_clamped_to_retention {
        let floor = interval
            .retained_from()
            .unwrap_or_else(|| "the retention floor".to_owned());
        Either::Left(view! {
            <p class="usage-retention" role="note">
                {format!("Window clamped to retention. Data before {floor} has aged out and is not counted here.")}
            </p>
        })
    } else {
        Either::Right(())
    };
    view! {
        <p class="usage-interval">"Resolved window: "<strong>{window}</strong>" (UTC, inclusive)"</p>
        {retention}
    }
}

fn top_table(rows: Vec<UiResourceRow>) -> impl IntoView {
    let count = rows.len();
    view! {
        <table class="files usage-table usage-top-table">
            <caption>{format!("{count} top resources")}</caption>
            <thead>
                <tr>
                    <th scope="col">"Repository"</th>
                    <th scope="col">"Resource"</th>
                    <th scope="col" class="num">"Reads"</th>
                    <th scope="col" class="num">"Bytes"</th>
                </tr>
            </thead>
            <tbody>{rows.into_iter().map(|row| view! {
                <tr>
                    <td><code>{row.repository}</code></td>
                    <td>{row.resource}</td>
                    <td class="num">{row.reads.to_string()}</td>
                    <td class="num">{human_size(row.bytes)}</td>
                </tr>
            }).collect_view()}</tbody>
        </table>
    }
}

fn groups_table(rows: Vec<UiGroupRow>) -> impl IntoView {
    let count = rows.len();
    view! {
        <table class="files usage-table usage-groups-table">
            <caption>{format!("{count} groups")}</caption>
            <thead>
                <tr>
                    <th scope="col">"Repository"</th>
                    <th scope="col">"Resource"</th>
                    <th scope="col">"Group"</th>
                    <th scope="col" class="num">"Reads"</th>
                    <th scope="col" class="num">"Bytes"</th>
                </tr>
            </thead>
            <tbody>{rows.into_iter().map(|row| view! {
                <tr>
                    <td><code>{row.repository}</code></td>
                    <td>{row.resource}</td>
                    <td>{or_dash(row.group)}</td>
                    <td class="num">{row.reads.to_string()}</td>
                    <td class="num">{human_size(row.bytes)}</td>
                </tr>
            }).collect_view()}</tbody>
        </table>
    }
}

fn sources_table(rows: Vec<UiSourceRow>) -> impl IntoView {
    let count = rows.len();
    view! {
        <table class="files usage-table usage-sources-table">
            <caption>{format!("{count} source rows")}</caption>
            <thead>
                <tr>
                    <th scope="col">"Repository"</th>
                    <th scope="col">"Resource"</th>
                    <th scope="col">"Source"</th>
                    <th scope="col" class="num">"Reads"</th>
                    <th scope="col" class="num">"Bytes"</th>
                </tr>
            </thead>
            <tbody>{rows.into_iter().map(|row| view! {
                <tr>
                    <td><code>{row.repository}</code></td>
                    <td>{row.resource}</td>
                    <td>{row.source.unwrap_or_else(|| "local store".to_owned())}</td>
                    <td class="num">{row.reads.to_string()}</td>
                    <td class="num">{human_size(row.bytes)}</td>
                </tr>
            }).collect_view()}</tbody>
        </table>
    }
}

fn unused_table(rows: Vec<UiUnusedRow>) -> impl IntoView {
    let count = rows.len();
    view! {
        <table class="files usage-table usage-unused-table">
            <caption>{format!("{count} unused resources")}</caption>
            <thead>
                <tr>
                    <th scope="col">"Repository"</th>
                    <th scope="col">"Resource"</th>
                    <th scope="col" class="num">"Lifetime reads"</th>
                </tr>
            </thead>
            <tbody>{rows.into_iter().map(|row| view! {
                <tr>
                    <td><code>{row.repository}</code></td>
                    <td>{row.resource}</td>
                    <td class="num">{row.lifetime_reads.to_string()}</td>
                </tr>
            }).collect_view()}</tbody>
        </table>
    }
}

fn timeline_table(rows: Vec<UiTimelineRow>) -> impl IntoView {
    let count = rows.len();
    view! {
        <table class="files usage-table usage-timeline-table">
            <caption>{format!("{count} daily buckets")}</caption>
            <thead>
                <tr>
                    <th scope="col">"Start (UTC)"</th>
                    <th scope="col">"End (UTC)"</th>
                    <th scope="col" class="num">"Reads"</th>
                    <th scope="col" class="num">"Bytes"</th>
                </tr>
            </thead>
            <tbody>{rows.into_iter().map(|row| view! {
                <tr>
                    <td>{format_instant(row.start_unix)}</td>
                    <td>{format_instant(row.end_unix)}</td>
                    <td class="num">{row.reads.to_string()}</td>
                    <td class="num">{human_size(row.bytes)}</td>
                </tr>
            }).collect_view()}</tbody>
        </table>
    }
}

fn or_dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_owned())
}

#[cfg(test)]
#[path = "../../tests/unit/pages/analytics/tests.rs"]
mod tests;
