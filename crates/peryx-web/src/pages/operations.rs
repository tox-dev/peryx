#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;

use crate::data::load_operations;
use crate::model::{OperationRow, OperationsHealth, OperationsView, format_instant, operation_status_label};

#[component]
pub fn PendingOperations() -> impl IntoView {
    // `None` is the first page; a cursor pages the administrator's rows in operation-id order. The
    // resource re-reads whenever the cursor moves, so a click fetches the next page without a navigation.
    let (cursor, set_cursor) = signal(None::<String>);
    let view = Resource::new(move || cursor.get(), load_operations);
    view! {
        <section class="page operations-page">
            <div class="ops-title">
                <h1>"Pending operations"</h1>
                <span class="badge">"read-only"</span>
                <a href="/+availability/operations"><code>"/+availability/operations"</code></a>
            </div>
            <p class="dim">
                "The admitted writes this node retains: how many are still in flight, how many finalized, \
                 how many gave up, and how many outlived their retention deadline. The counts cover the \
                 whole ledger; the per-operation rows need administrator access and page in operation-id \
                 order. A row names an operation without revealing what it wrote or who owns it."
            </p>
            <Suspense fallback=|| view! { <p class="dim" role="status" aria-live="polite">"loading"</p> }>
                {move || Suspend::new(async move {
                    match view.await {
                        Ok(view) => view! { <OperationsBody view set_cursor /> }.into_any(),
                        Err(error) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
                    }
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn OperationsBody(view: OperationsView, set_cursor: WriteSignal<Option<String>>) -> impl IntoView {
    let captured = format_instant(view.captured_at);
    let health = view.health;
    let rows = view.rows;
    let next_cursor = view.next_cursor;
    view! {
        <OperationsSummary health captured />
        {rows.map_or_else(
            || view! {
                <p class="dim" role="note">
                    "Per-operation rows need administrator access. The counts above cover the whole ledger."
                </p>
            }.into_any(),
            |rows| view! { <OperationsRows rows next_cursor set_cursor /> }.into_any(),
        )}
    }
}

#[component]
fn OperationsSummary(health: OperationsHealth, captured: String) -> impl IntoView {
    view! {
        <div class="stat-row operations-summary">
            <div class="stat"><strong>{health.pending}</strong><span class="badge health-unready">"pending"</span></div>
            <div class="stat"><strong>{health.published}</strong><span class="badge health-live">"published"</span></div>
            <div class="stat"><strong>{health.failed}</strong><span class="badge health-unknown">"failed"</span></div>
            <div class="stat"><strong>{health.expired}</strong><span class="badge health-restricted">"expired"</span></div>
            <div class="stat"><strong>{health.total}</strong><span>"total operations"</span></div>
            <div class="stat"><strong>{captured}</strong><span>"observed at (UTC)"</span></div>
        </div>
    }
}

#[component]
fn OperationsRows(
    rows: Vec<OperationRow>,
    next_cursor: Option<String>,
    set_cursor: WriteSignal<Option<String>>,
) -> impl IntoView {
    if rows.is_empty() {
        return view! {
            <p class="dim" role="status">"No operations are recorded yet."</p>
        }
        .into_any();
    }
    let count = rows.len();
    let table_rows = rows.into_iter().map(operation_table_row).collect_view();
    view! {
        <div class="table-scroll">
            <table class="files ops-table operations-table">
                <caption>"Retained write operations, one row per operation, in operation-id order."</caption>
                <thead>
                    <tr>
                        <th scope="col">"Operation"</th>
                        <th scope="col">"Status"</th>
                        <th scope="col">"Updated at (UTC)"</th>
                        <th scope="col">"Expires at (UTC)"</th>
                    </tr>
                </thead>
                <tbody>{table_rows}</tbody>
            </table>
        </div>
        <div class="pager operations-pager">
            <p class="result-count" role="status" aria-live="polite">
                {format!("Showing {count} operation rows on this page.")}
            </p>
            <button type="button" on:click=move |_| set_cursor.set(None)>"First page"</button>
            {next_cursor.map(|cursor| view! {
                <button type="button" on:click=move |_| set_cursor.set(Some(cursor.clone()))>"Next page"</button>
            })}
        </div>
    }
    .into_any()
}

fn operation_table_row(row: OperationRow) -> AnyView {
    let status = operation_status_label(row.status);
    let updated = format_instant(row.updated_at);
    let expires = row.expires_at.map_or_else(|| "-".to_owned(), format_instant);
    view! {
        <tr>
            <td><code>{row.operation}</code></td>
            <td><span class=format!("badge {}", status.class)>{status.text}</span></td>
            <td>{updated}</td>
            <td>{expires}</td>
        </tr>
    }
    .into_any()
}
