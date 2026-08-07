#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;

use super::human_size;
use crate::model::{
    AnalyticsFilters, UiInterval, UiPackageRow, UiSourceRow, UiTimelineRow, UiUnusedRow, UiUsagePage, UiUsageRows,
    UiVersionRow, format_instant,
};

#[component]
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
    view! {
        <section class="page analytics-page">
            <div class="ops-title">
                <h1>"Usage analytics"</h1>
                <span class="badge">"read-only"</span>
            </div>
            <p class="dim">
                "Query the retained daily download aggregate. Times are UTC. The source split needs operator access; \
                 other views accept a repository grant. Credentials remain in this browser tab and are sent only in \
                 the authorization header."
            </p>
            <form class="policy-filters analytics-filters" on:submit=move |event| {
                event.prevent_default();
                let filters = filters.get_untracked();
                set_active.set(filters.clone());
                set_cursor.set(None);
                set_previous.set(Vec::new());
                run_query(&filters, None, user.get_untracked(), password.get_untracked(), ui);
            }>
                <AnalyticsFilterFields set_user set_password set_filters loading />
            </form>
            <div class="analytics-results">
                {move || {
                    if loading.get() {
                        return view! { <p class="dim" role="status" aria-live="polite">"Loading usage analytics..."</p> }.into_any();
                    }
                    match result.get() {
                        None => view! { <p class="dim">"Enter credentials and search to load usage."</p> }.into_any(),
                        Some(Err(error)) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
                        Some(Ok(page)) => usage_page(page),
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
            placeholder="Local user or __token__"
            required
            on:input:target=move |event| set_user.set(event.target().value())
        />
        <label for="analytics-password">"Password or upload token"</label>
        <input
            id="analytics-password"
            type="password"
            autocomplete="off"
            required
            on:input:target=move |event| set_password.set(event.target().value())
        />
        <label for="analytics-view">"View"</label>
        <select
            id="analytics-view"
            on:change:target=move |event| set_filters.update(|value| value.view = event.target().value())
        >
            <option value="top">"Top packages"</option>
            <option value="versions">"Version usage"</option>
            <option value="sources">"Source split"</option>
            <option value="unused">"Unused packages"</option>
            <option value="timeline">"Timeline"</option>
        </select>
        <label for="analytics-repository">"Repository"</label>
        <input
            id="analytics-repository"
            maxlength="512"
            placeholder="All permitted repositories"
            on:input:target=move |event| set_filters.update(|value| value.repository = event.target().value())
        />
        <label for="analytics-from">"From (UTC day)"</label>
        <input
            id="analytics-from"
            type="date"
            on:input:target=move |event| set_filters.update(|value| value.from = event.target().value())
        />
        <label for="analytics-to">"To (UTC day)"</label>
        <input
            id="analytics-to"
            type="date"
            on:input:target=move |event| set_filters.update(|value| value.to = event.target().value())
        />
        <label for="analytics-limit">"Rows"</label>
        <select
            id="analytics-limit"
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
struct AnalyticsUi {
    result: WriteSignal<Option<Result<UiUsagePage, String>>>,
    loading: WriteSignal<bool>,
}

fn run_query(filters: &AnalyticsFilters, cursor: Option<&str>, user: String, password: String, ui: AnalyticsUi) {
    let view = filters.view();
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
            .set(Some(crate::data::load_analytics(&url, view, &user, &password).await));
        ui.loading.set(false);
    });
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    {
        let _ = (url, view, user, password);
        ui.loading.set(false);
    }
}

fn usage_page(page: UiUsagePage) -> AnyView {
    let interval = interval_banner(&page.interval);
    let count = page.rows.len();
    if page.rows.is_empty() {
        return view! {
            {interval}
            <p class="dim" role="status" aria-live="polite">"No usage recorded for this view in the resolved window."</p>
        }
        .into_any();
    }
    let table = match page.rows {
        UiUsageRows::Top(rows) => top_table(rows),
        UiUsageRows::Versions(rows) => versions_table(rows),
        UiUsageRows::Sources(rows) => sources_table(rows),
        UiUsageRows::Unused(rows) => unused_table(rows),
        UiUsageRows::Timeline(rows) => timeline_table(rows),
    };
    view! {
        {interval}
        <p class="result-count" role="status" aria-live="polite">{format!("Loaded {count} rows.")}</p>
        <div class="table-scroll">{table}</div>
    }
    .into_any()
}

fn interval_banner(interval: &UiInterval) -> AnyView {
    let window = interval.window();
    let retention = if interval.window_clamped_to_retention {
        let floor = interval
            .retained_from()
            .unwrap_or_else(|| "the retention floor".to_owned());
        view! {
            <p class="usage-retention" role="note">
                {format!("Window clamped to retention. Data before {floor} has aged out and is not counted here.")}
            </p>
        }
        .into_any()
    } else {
        ().into_any()
    };
    view! {
        <p class="usage-interval">"Resolved window: "<strong>{window}</strong>" (UTC, inclusive)"</p>
        {retention}
    }
    .into_any()
}

fn top_table(rows: Vec<UiPackageRow>) -> AnyView {
    let count = rows.len();
    view! {
        <table class="files usage-table usage-top-table">
            <caption>{format!("{count} top packages")}</caption>
            <thead>
                <tr>
                    <th scope="col">"Repository"</th>
                    <th scope="col">"Package"</th>
                    <th scope="col" class="num">"Downloads"</th>
                    <th scope="col" class="num">"Bytes"</th>
                </tr>
            </thead>
            <tbody>{rows.into_iter().map(|row| view! {
                <tr>
                    <td><code>{row.repository}</code></td>
                    <td>{row.project}</td>
                    <td class="num">{row.downloads.to_string()}</td>
                    <td class="num">{human_size(row.bytes)}</td>
                </tr>
            }).collect_view()}</tbody>
        </table>
    }
    .into_any()
}

fn versions_table(rows: Vec<UiVersionRow>) -> AnyView {
    let count = rows.len();
    view! {
        <table class="files usage-table usage-versions-table">
            <caption>{format!("{count} versions")}</caption>
            <thead>
                <tr>
                    <th scope="col">"Repository"</th>
                    <th scope="col">"Package"</th>
                    <th scope="col">"Version"</th>
                    <th scope="col" class="num">"Downloads"</th>
                    <th scope="col" class="num">"Bytes"</th>
                </tr>
            </thead>
            <tbody>{rows.into_iter().map(|row| view! {
                <tr>
                    <td><code>{row.repository}</code></td>
                    <td>{row.project}</td>
                    <td>{or_dash(row.version)}</td>
                    <td class="num">{row.downloads.to_string()}</td>
                    <td class="num">{human_size(row.bytes)}</td>
                </tr>
            }).collect_view()}</tbody>
        </table>
    }
    .into_any()
}

fn sources_table(rows: Vec<UiSourceRow>) -> AnyView {
    let count = rows.len();
    view! {
        <table class="files usage-table usage-sources-table">
            <caption>{format!("{count} source rows")}</caption>
            <thead>
                <tr>
                    <th scope="col">"Repository"</th>
                    <th scope="col">"Package"</th>
                    <th scope="col">"Source"</th>
                    <th scope="col" class="num">"Downloads"</th>
                    <th scope="col" class="num">"Bytes"</th>
                </tr>
            </thead>
            <tbody>{rows.into_iter().map(|row| view! {
                <tr>
                    <td><code>{row.repository}</code></td>
                    <td>{row.project}</td>
                    <td>{row.source.unwrap_or_else(|| "local store".to_owned())}</td>
                    <td class="num">{row.downloads.to_string()}</td>
                    <td class="num">{human_size(row.bytes)}</td>
                </tr>
            }).collect_view()}</tbody>
        </table>
    }
    .into_any()
}

fn unused_table(rows: Vec<UiUnusedRow>) -> AnyView {
    let count = rows.len();
    view! {
        <table class="files usage-table usage-unused-table">
            <caption>{format!("{count} unused packages")}</caption>
            <thead>
                <tr>
                    <th scope="col">"Repository"</th>
                    <th scope="col">"Package"</th>
                    <th scope="col" class="num">"Lifetime downloads"</th>
                </tr>
            </thead>
            <tbody>{rows.into_iter().map(|row| view! {
                <tr>
                    <td><code>{row.repository}</code></td>
                    <td>{row.project}</td>
                    <td class="num">{row.lifetime_downloads.to_string()}</td>
                </tr>
            }).collect_view()}</tbody>
        </table>
    }
    .into_any()
}

fn timeline_table(rows: Vec<UiTimelineRow>) -> AnyView {
    let count = rows.len();
    view! {
        <table class="files usage-table usage-timeline-table">
            <caption>{format!("{count} daily buckets")}</caption>
            <thead>
                <tr>
                    <th scope="col">"Start (UTC)"</th>
                    <th scope="col">"End (UTC)"</th>
                    <th scope="col" class="num">"Downloads"</th>
                    <th scope="col" class="num">"Bytes"</th>
                </tr>
            </thead>
            <tbody>{rows.into_iter().map(|row| view! {
                <tr>
                    <td>{format_instant(row.start_unix)}</td>
                    <td>{format_instant(row.end_unix)}</td>
                    <td class="num">{row.downloads.to_string()}</td>
                    <td class="num">{human_size(row.bytes)}</td>
                </tr>
            }).collect_view()}</tbody>
        </table>
    }
    .into_any()
}

fn or_dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_owned())
}
