use leptos::prelude::*;

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
use crate::data::load_blob_placement;
use crate::data::load_placements;
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
use crate::model::{BlobDatacenterPlacement, BlobPlacementView, blob_placement_status_label};
use crate::model::{PlacementHealth, PlacementRow, PlacementView, format_instant};

struct PlacementLabel {
    key: &'static str,
    text: &'static str,
    hint: &'static str,
}

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
pub fn ArtifactPlacements() -> impl IntoView {
    let (_, set_cursor) = signal(None::<String>);
    view! {
        <section class="page placements-page">
            <div class="ops-title">
                <h1>"Artifact placement health"</h1>
                <span class="badge">"read-only"</span>
                <a href="/+availability/placements"><code>"/+availability/placements"</code></a>
            </div>
            <p class="dim">
                "How the store's bytes are placed: how many artifacts serve locally, how many depend on an \
                 upstream, and how many cannot be served at all. The counts cover the whole store; the \
                 per-digest rows need administrator access and page in digest order. A digest names an \
                 artifact without revealing where it lives or who owns it."
            </p>
            <Suspense fallback=|| view! { <p class="dim" role="status" aria-live="polite">"loading"</p> }>
                {move || Suspend::new(async move {
                    match load_placements(None).await {
                        Ok(view) => view! { <PlacementBody view set_cursor /> }.into_any(),
                        Err(error) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
                    }
                })}
            </Suspense>
        </section>
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
pub fn ArtifactPlacements() -> impl IntoView {
    // `None` is the first page; a cursor pages the administrator's rows in digest order. The resource
    // re-reads whenever the cursor moves, so a click fetches the next page without a full navigation.
    let (cursor, set_cursor) = signal(None::<String>);
    let view = Resource::new(move || cursor.get(), load_placements);
    view! {
        <section class="page placements-page">
            <div class="ops-title">
                <h1>"Artifact placement health"</h1>
                <span class="badge">"read-only"</span>
                <a href="/+availability/placements"><code>"/+availability/placements"</code></a>
            </div>
            <p class="dim">
                "How the store's bytes are placed: how many artifacts serve locally, how many depend on an \
                 upstream, and how many cannot be served at all. The counts cover the whole store; the \
                 per-digest rows need administrator access and page in digest order. A digest names an \
                 artifact without revealing where it lives or who owns it."
            </p>
            <Suspense fallback=|| view! { <p class="dim" role="status" aria-live="polite">"loading"</p> }>
                {move || Suspend::new(async move {
                    match view.await {
                        Ok(view) => view! { <PlacementBody view set_cursor /> }.into_any(),
                        Err(error) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
                    }
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn PlacementBody(view: PlacementView, set_cursor: WriteSignal<Option<String>>) -> impl IntoView {
    let captured = format_instant(view.captured_at);
    let health = view.health;
    let rows = view.rows;
    let next_cursor = view.next_cursor;
    view! {
        <HealthSummary health captured />
        {rows.map_or_else(
            || view! {
                <p class="dim" role="note">
                    "Per-digest placement rows need administrator access. The counts above cover the whole store."
                </p>
            }.into_any(),
            |rows| view! { <PlacementRows rows next_cursor set_cursor /> }.into_any(),
        )}
    }
}

#[component]
fn HealthSummary(health: PlacementHealth, captured: String) -> impl IntoView {
    let local = byte_availability_label(peryx_core::UiByteAvailability::Local);
    let remote = byte_availability_label(peryx_core::UiByteAvailability::RemoteOnly);
    let unavailable = byte_availability_label(peryx_core::UiByteAvailability::Unavailable);
    view! {
        <div class="stat-row placement-summary">
            <div class="stat">
                <strong>{health.local}</strong>
                <span class="badge avail-local" title=local.hint>{local.text}</span>
            </div>
            <div class="stat">
                <strong>{health.remote_only}</strong>
                <span class="badge avail-remote-only" title=remote.hint>{remote.text}</span>
            </div>
            <div class="stat">
                <strong>{health.unavailable}</strong>
                <span class="badge avail-unavailable" title=unavailable.hint>{unavailable.text}</span>
            </div>
            <div class="stat"><strong>{health.total}</strong><span>"total artifacts"</span></div>
            <div class="stat"><strong>{captured}</strong><span>"observed at (UTC)"</span></div>
        </div>
    }
}

#[component]
fn PlacementRows(
    rows: Vec<PlacementRow>,
    next_cursor: Option<String>,
    set_cursor: WriteSignal<Option<String>>,
) -> impl IntoView {
    if rows.is_empty() {
        return view! {
            <p class="dim" role="status">"No artifact placements are recorded yet."</p>
        }
        .into_any();
    }
    let count = rows.len();
    let selected = RwSignal::new(None::<String>);
    let table_rows = rows
        .into_iter()
        .map(move |row| placement_row(row, selected))
        .collect_view();
    view! {
        <div class="table-scroll">
            <table class="files ops-table placement-table">
                <caption>"Recorded artifact placements, one row per digest, in digest order. Select a digest to see which datacenters hold it."</caption>
                <thead>
                    <tr>
                        <th scope="col">"Digest"</th>
                        <th scope="col">"Source"</th>
                        <th scope="col">"Byte availability"</th>
                    </tr>
                </thead>
                <tbody>{table_rows}</tbody>
            </table>
        </div>
        <BlobPlacementDetail selected />
        <PlacementPager count next_cursor set_cursor />
    }
    .into_any()
}

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
fn PlacementPager(count: usize, next_cursor: Option<String>, set_cursor: WriteSignal<Option<String>>) -> impl IntoView {
    let _ = set_cursor;
    view! {
        <div class="pager placement-pager">
            <p class="result-count" role="status" aria-live="polite">
                {format!("Showing {count} placement rows on this page.")}
            </p>
            <button type="button">"First page"</button>
            {next_cursor.map(|_| view! { <button type="button">"Next page"</button> })}
        </div>
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn PlacementPager(count: usize, next_cursor: Option<String>, set_cursor: WriteSignal<Option<String>>) -> impl IntoView {
    view! {
        <div class="pager placement-pager">
            <p class="result-count" role="status" aria-live="polite">
                {format!("Showing {count} placement rows on this page.")}
            </p>
            <button type="button" onclick="location.href='/admin/placements'">"First page"</button>
            {next_cursor.map(|cursor| view! {
                <button type="button" on:click=move |_| set_cursor.set(Some(cursor.clone()))>"Next page"</button>
            })}
        </div>
    }
}

fn placement_row(row: PlacementRow, selected: RwSignal<Option<String>>) -> AnyView {
    let source = file_source_label(row.source);
    let availability = byte_availability_label(row.availability);
    let digest = row.digest;
    view! {
        <tr>
            <td>
                <DigestDrill digest selected />
            </td>
            <td><span class=format!("badge placement-source src-{}", source.key) title=source.hint>{source.text}</span></td>
            <td>
                <span class=format!("badge placement-avail avail-{}", availability.key) title=availability.hint>
                    {availability.text}
                </span>
            </td>
        </tr>
    }
    .into_any()
}

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
fn DigestDrill(digest: String, selected: RwSignal<Option<String>>) -> impl IntoView {
    let _ = selected;
    view! {
        <button type="button" class="digest-drill" title="Show which datacenters hold this blob">
            <code>{digest}</code>
        </button>
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn DigestDrill(digest: String, selected: RwSignal<Option<String>>) -> impl IntoView {
    let drill = digest.clone();
    view! {
        <button
            type="button"
            class="digest-drill"
            title="Show which datacenters hold this blob"
            on:click=move |_| selected.set(Some(drill.clone()))
        >
            <code>{digest}</code>
        </button>
    }
}

/// The per-datacenter placement of the digest a reader has drilled into, loaded on demand.
///
/// Empty until a digest is selected; an error or an empty datacenter list reads as such rather than as
/// convergence.
#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
fn BlobPlacementDetail(selected: RwSignal<Option<String>>) -> impl IntoView {
    let _ = selected;
    view! {
        <Suspense>
            {None::<AnyView>}
        </Suspense>
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn BlobPlacementDetail(selected: RwSignal<Option<String>>) -> impl IntoView {
    let detail = Resource::new(
        move || selected.get(),
        |digest| async move {
            match digest {
                Some(digest) => Some(load_blob_placement(digest).await),
                None => None,
            }
        },
    );
    view! {
        <Suspense>
            {move || Suspend::new(async move {
                match detail.await {
                    None => ().into_any(),
                    Some(Ok(view)) => blob_placement_detail(&view).into_any(),
                    Some(Err(error)) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
                }
            })}
        </Suspense>
    }
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn blob_placement_detail(view: &BlobPlacementView) -> AnyView {
    if view.datacenters.is_empty() {
        return view! {
            <p class="dim placement-detail" role="status">
                {format!("No datacenter holds {} yet.", view.digest)}
            </p>
        }
        .into_any();
    }
    let rows = view.datacenters.iter().map(datacenter_row).collect_view();
    view! {
        <div class="table-scroll placement-detail">
            <table class="files ops-table placement-dc-table">
                <caption>{format!("Datacenters holding {}", view.digest)}</caption>
                <thead>
                    <tr>
                        <th scope="col">"Datacenter"</th>
                        <th scope="col">"Status"</th>
                        <th scope="col" class="num">"Size"</th>
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
        </div>
    }
    .into_any()
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn datacenter_row(placement: &BlobDatacenterPlacement) -> AnyView {
    let status = blob_placement_status_label(placement.status);
    view! {
        <tr>
            <td>{placement.data_center.clone()}</td>
            <td><span class=format!("badge {}", status.class)>{status.text}</span></td>
            <td class="num">{placement.size.map_or_else(|| "-".to_owned(), |size| size.to_string())}</td>
        </tr>
    }
    .into_any()
}

const fn file_source_label(source: peryx_core::UiArtifactSource) -> PlacementLabel {
    match source {
        peryx_core::UiArtifactSource::Hosted => PlacementLabel {
            key: "hosted",
            text: "hosted",
            hint: "Published into this instance",
        },
        peryx_core::UiArtifactSource::Proxy => PlacementLabel {
            key: "proxy",
            text: "proxy",
            hint: "Cached from an upstream",
        },
        peryx_core::UiArtifactSource::Generated => PlacementLabel {
            key: "generated",
            text: "generated",
            hint: "Generated by this instance",
        },
    }
}

const fn byte_availability_label(availability: peryx_core::UiByteAvailability) -> PlacementLabel {
    match availability {
        peryx_core::UiByteAvailability::Local => PlacementLabel {
            key: "local",
            text: "local",
            hint: "Bytes are held by this instance",
        },
        peryx_core::UiByteAvailability::RemoteOnly => PlacementLabel {
            key: "remote-only",
            text: "remote only",
            hint: "Bytes require a remote source",
        },
        peryx_core::UiByteAvailability::Unavailable => PlacementLabel {
            key: "unavailable",
            text: "unavailable",
            hint: "No source can serve these bytes",
        },
    }
}

#[cfg(all(test, feature = "ssr"))]
#[path = "../../tests/unit/pages/placement/tests.rs"]
mod tests;
