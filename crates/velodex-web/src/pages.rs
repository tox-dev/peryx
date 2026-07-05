//! The UI pages: a live dashboard and a pypi.org-style package browser.
#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]
#![allow(
    clippy::missing_const_for_fn,
    reason = "cfg-split helpers are const only without the hydrate feature; constness cannot vary by cfg"
)]

use std::sync::Arc;

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};
use regex::Regex;
use velodex_ecosystem_pypi::CoreMetadataDoc;

use crate::data::{
    load_admin_snapshot, load_member_chunk, load_members, load_project, load_projects, load_search, load_snapshot,
    load_stats,
};
use crate::markdown::render_description;
use crate::model::{
    UiCounters, UiFile, UiIndex, UiMember, UiMemberChunk, UiProject, UiRecentUpload, UiSearchPage, UiSnapshot, UiStats,
    source_label,
};
use crate::url::{
    admin_project_url, admin_version_url, browse_archive_listing_url, browse_archive_member_url, browse_archive_url,
    browse_index_url, browse_project_file_search_url, browse_project_url, search_page_url, simple_index_url,
    stats_index_url, stats_project_url,
};

type ProjectPage = Result<Option<(UiProject, Option<CoreMetadataDoc>)>, String>;
type ProjectPageResource = Resource<ProjectPage>;

/// The landing dashboard: identity, live counters, and the configured indexes with their usage.
#[component]
pub fn Dashboard() -> impl IntoView {
    let snapshot = Resource::new(|| (), |()| load_snapshot());
    let stats = Resource::new(|| (), |()| load_stats(None, None));
    start_refresh(snapshot);
    view! {
        <section class="page">
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || Suspend::new(async move {
                    let data = snapshot.await;
                    let usage = stats.await;
                    view! { <DashboardBody data usage /> }
                })}
            </Suspense>
        </section>
    }
}

#[component]
pub fn AdminStatus() -> impl IntoView {
    let snapshot = Resource::new(|| (), |()| load_admin_snapshot());
    let stats = Resource::new(|| (), |()| load_stats(None, None));
    start_refresh(snapshot);
    view! {
        <section class="page ops-page">
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || Suspend::new(async move {
                    let data = snapshot.await;
                    let usage = stats.await;
                    view! { <AdminStatusBody data usage /> }
                })}
            </Suspense>
        </section>
    }
}

/// Refresh the dashboard counters every few seconds once hydrated. Effects never run during server
/// rendering, so this is inert in SSR output.
fn start_refresh(snapshot: Resource<UiSnapshot>) {
    #[cfg(feature = "hydrate")]
    {
        use std::time::Duration;
        Effect::new(move |_| {
            set_interval(move || snapshot.refetch(), Duration::from_secs(5));
        });
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = snapshot;
    }
}

#[component]
fn AdminStatusBody(data: UiSnapshot, usage: UiStats) -> impl IntoView {
    let has_usage = usage.totals != UiCounters::default();
    let indexes = data.indexes.clone();
    let empty = indexes.is_empty();
    let project_count: u64 = indexes.iter().map(|index| index.project_count).sum();
    let upload_count: u64 = indexes.iter().map(|index| index.upload_count).sum();
    view! {
        <div class="ops-title">
            <h1>"Admin status"</h1>
            <span class="badge">"read-only"</span>
            <a href="/+status"><code>"/+status"</code></a>
            <a href="/+stats"><code>"/+stats"</code></a>
            <a href="/metrics"><code>"/metrics"</code></a>
        </div>
        <div class="metrics-group">
            <div class="metrics-label">"Global"</div>
            <div class="stat-row">
                <div class="stat"><strong>{data.version.clone()}</strong><span>"version"</span></div>
                <div class="stat"><strong>{data.serial}</strong><span>"change serial"</span></div>
                <div class="stat"><strong>{data.requests}</strong><span>"requests served"</span></div>
                <div class="stat"><strong>{indexes.len()}</strong><span>"indexes"</span></div>
                <div class="stat"><strong>{kind_count(&indexes, "virtual")}</strong><span>"virtual"</span></div>
                <div class="stat"><strong>{project_count}</strong><span>"observed projects"</span></div>
                <div class="stat"><strong>{upload_count}</strong><span>"uploaded files"</span></div>
            </div>
        </div>
        {ecosystem_stats(&data)}
        <h2>"Indexes"</h2>
        <AdminIndexTable indexes=indexes.clone() all=indexes.clone() />
        {empty.then(|| view! { <p class="dim">"No indexes configured."</p> })}
        <h2>"Recent uploads"</h2>
        <AdminRecentUploads indexes=indexes.clone() />
        <h2>"Usage and health"</h2>
        <AdminUsageTable indexes usage />
        {(!has_usage).then(|| view! { <p class="dim">"No usage recorded yet."</p> })}
    }
}

fn kind_count(indexes: &[UiIndex], kind: &str) -> usize {
    indexes.iter().filter(|index| index.kind == kind).count()
}

/// The per-ecosystem metric groups: one labelled block per ecosystem, so the reader can tell a
/// PyPI-scoped counter (its listings, artifacts, and PEP 658 hits) from the global request count.
fn ecosystem_stats(data: &UiSnapshot) -> impl IntoView + use<> {
    let families = data.families.clone();
    data.ecosystems
        .clone()
        .into_iter()
        .map(move |summary| {
            let badge = format!("badge ecosystem-{}", summary.ecosystem);
            let named = families
                .iter()
                .map(|family| {
                    let total = summary.families.get(&family.key).copied().unwrap_or(0);
                    view! { <div class="stat"><strong>{total}</strong><span>{family.label.clone()}</span></div> }
                })
                .collect_view();
            view! {
                <div class="metrics-group">
                    <div class="metrics-label"><span class=badge>{summary.ecosystem.clone()}</span>" activity"</div>
                    <div class="stat-row">
                        <div class="stat"><strong>{summary.pages}</strong><span>"listings served"</span></div>
                        <div class="stat"><strong>{summary.downloads}</strong><span>"artifacts served"</span></div>
                        <div class="stat"><strong>{summary.uploads}</strong><span>"uploads"</span></div>
                        {named}
                    </div>
                </div>
            }
        })
        .collect_view()
}

#[component]
fn AdminIndexTable(indexes: Vec<UiIndex>, all: Vec<UiIndex>) -> impl IntoView {
    view! {
        <div class="table-scroll">
            <table class="files ops-table">
                <thead>
                    <tr>
                        <th>"Name"</th>
                        <th>"Route"</th>
                        <th>"Type"</th>
                        <th>"Simple API"</th>
                        <th>"Projects"</th>
                        <th>"Files"</th>
                        <th>"Topology"</th>
                        <th>"Uploads"</th>
                        <th>"Status"</th>
                    </tr>
                </thead>
                <tbody>
                    {indexes
                        .into_iter()
                        .map(|index| {
                            let browse = browse_index_url(&index.route);
                            let simple = simple_index_url(&index.route);
                            view! {
                                <tr>
                                    <td><a href=browse>{index.name.clone()}</a></td>
                                    <td><code>{index.route.clone()}</code></td>
                                    <td class="ops-type">
                                        <span class=format!("badge ecosystem-{}", index.ecosystem)>{index.ecosystem.clone()}</span>
                                        <span class=format!("badge kind-{}", index.kind)>{index.kind.clone()}</span>
                                    </td>
                                    <td><a class="ops-simple" href=simple.clone() title=simple>"simple"</a></td>
                                    <td>{index.project_count}</td>
                                    <td>{index.upload_count}</td>
                                    <td><TopologyCell index=index.clone() all=all.clone() /></td>
                                    <td><UploadCell index=index.clone() /></td>
                                    <td><StatusCell index /></td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </div>
    }
}

#[component]
fn TopologyCell(index: UiIndex, all: Vec<UiIndex>) -> impl IntoView {
    if index.layers.is_empty() {
        return view! { <span class="dim">"direct"</span> }.into_any();
    }
    view! {
        <ol class="ops-stack">
            {index
                .layers
                .into_iter()
                .enumerate()
                .map(|(position, name)| {
                    let shown = name.clone();
                    let route = all
                        .iter()
                        .find(|candidate| candidate.name == name)
                        .map(|member| browse_index_url(&member.route));
                    view! {
                        <li>
                            <span class="layer-order">{position + 1}</span>
                            {route
                                .map_or_else(
                                    || view! { <span>{shown}</span> }.into_any(),
                                    |route| view! { <a href=route>{name}</a> }.into_any(),
                                )}
                        </li>
                    }
                })
                .collect_view()}
        </ol>
    }
    .into_any()
}

#[component]
fn UploadCell(index: UiIndex) -> impl IntoView {
    if index.kind == "cached" {
        return view! { <span class="dim">"none"</span> }.into_any();
    }
    let label = if index.uploads { "enabled" } else { "disabled" };
    index.upload_to.map_or_else(
        || view! { <span class=format!("badge upload-{label}")>{label}</span> }.into_any(),
        |target| {
            view! {
                <span class=format!("badge upload-{label}")>{label}</span>
                " "
                <code>{target}</code>
            }
            .into_any()
        },
    )
}

#[component]
fn StatusCell(index: UiIndex) -> impl IntoView {
    if let Some(upstream) = index.upstream {
        return view! {
            <p class="ops-detail">
                <span class="badge status-configured">{upstream.status}</span>
                <code>{upstream.url}</code>
                <span>{auth_label(&upstream.auth_kind)}</span>
                {upstream.auth_redacted.map(|value| view! { <code>{value}</code> })}
            </p>
        }
        .into_any();
    }
    if let Some(hosted) = index.hosted {
        let mode = if hosted.volatile { "volatile" } else { "non-volatile" };
        let token = if hosted.token_configured {
            "token configured"
        } else {
            "no upload token"
        };
        return view! {
            <p class="ops-detail">
                <span>{mode}</span>
                <span>{token}</span>
                {hosted.token_redacted.map(|value| view! { <code>{value}</code> })}
            </p>
        }
        .into_any();
    }
    view! { <span class="dim">"composed from layers"</span> }.into_any()
}

fn auth_label(kind: &str) -> &'static str {
    match kind {
        "basic" => "basic auth",
        "bearer" => "bearer auth",
        _ => "anonymous",
    }
}

#[component]
fn AdminRecentUploads(indexes: Vec<UiIndex>) -> impl IntoView {
    let rows = indexes
        .into_iter()
        .flat_map(|index| {
            let name = index.name;
            index
                .recent_uploads
                .into_iter()
                .map(move |upload| recent_upload_row(name.clone(), upload))
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return view! { <p class="dim">"No uploads recorded yet."</p> }.into_any();
    }
    view! {
        <div class="table-scroll">
            <table class="files ops-table">
                <thead>
                    <tr>
                        <th>"Index"</th>
                        <th>"Project"</th>
                        <th>"File"</th>
                        <th>"Version"</th>
                        <th>"Uploaded"</th>
                        <th>"Size"</th>
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
        </div>
    }
    .into_any()
}

fn recent_upload_row(index: String, upload: UiRecentUpload) -> AnyView {
    view! {
        <tr>
            <td>{index}</td>
            <td><code>{upload.project}</code></td>
            <td><code>{upload.filename}</code></td>
            <td>{upload.version}</td>
            <td>{upload.uploaded_at.map_or_else(|| "n/a".to_owned(), |time| time.chars().take(10).collect())}</td>
            <td>{upload.size.map_or_else(|| "n/a".to_owned(), human_size)}</td>
        </tr>
    }
    .into_any()
}

#[component]
fn AdminUsageTable(indexes: Vec<UiIndex>, usage: UiStats) -> impl IntoView {
    view! {
        <div class="table-scroll">
            <table class="files ops-table">
                <thead>
                    <tr>
                        <th>"Index"</th>
                        <th>"Pages"</th>
                        <th>"Downloads"</th>
                        <th>"Served"</th>
                        <th>"Metadata"</th>
                        <th>"Uploads"</th>
                        <th>"Refreshes"</th>
                        <th>"Changed"</th>
                        <th>"Stale"</th>
                        <th>"Errors"</th>
                        <th>"Rejected"</th>
                    </tr>
                </thead>
                <tbody>
                    {indexes
                        .into_iter()
                        .map(|index| {
                            let counters = counters_for(&usage, &index.route);
                            let stats = stats_index_url(&index.route);
                            view! {
                                <tr>
                                    <td><a href=stats>{index.route}</a></td>
                                    <td>{counters.pages}</td>
                                    <td>{counters.downloads}</td>
                                    <td>{human_size(counters.bytes)}</td>
                                    <td>{counters.metadata}</td>
                                    <td>{counters.uploads}</td>
                                    <td>{counters.refreshes}</td>
                                    <td>{counters.changed}</td>
                                    <td>{counters.stale_served}</td>
                                    <td>{counters.upstream_errors}</td>
                                    <td>{counters.rejected}</td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </div>
    }
}

fn counters_for(usage: &UiStats, route: &str) -> UiCounters {
    optional_counters_for(usage, route).unwrap_or_default()
}

fn optional_counters_for(usage: &UiStats, route: &str) -> Option<UiCounters> {
    usage
        .rows
        .iter()
        .find(|(candidate, _)| candidate == route)
        .map(|(_, counters)| *counters)
}

#[component]
fn DashboardBody(data: UiSnapshot, usage: UiStats) -> impl IntoView {
    let layered: std::collections::HashSet<String> = data
        .indexes
        .iter()
        .flat_map(|index| index.layers.iter().cloned())
        .collect();
    let all = data.indexes.clone();
    let overlay_cards = data
        .indexes
        .iter()
        .filter(|index| !index.layers.is_empty())
        .cloned()
        .map(|index| {
            let counters = optional_counters_for(&usage, &index.route);
            view! { <OverlayCard index all=all.clone() counters /> }
        })
        .collect_view();
    let standalone: Vec<UiIndex> = data
        .indexes
        .iter()
        .filter(|index| index.layers.is_empty() && !layered.contains(&index.name))
        .cloned()
        .collect();
    let standalone_cards = (!standalone.is_empty()).then(|| {
        view! {
            <h2>"Standalone indexes"</h2>
            <div class="index-grid">
                {standalone
                    .into_iter()
                    .map(|index| {
                        let counters = optional_counters_for(&usage, &index.route);
                        view! { <IndexCard index counters /> }
                    })
                    .collect_view()}
            </div>
        }
    });
    view! {
        <div class="metrics-group">
            <div class="metrics-label">"Global"</div>
            <div class="stat-row">
                <div class="stat"><strong>{data.version.clone()}</strong><span>"version"</span></div>
                <div class="stat"><strong>{data.serial}</strong><span>"change serial"</span></div>
                <div class="stat"><strong>{data.requests}</strong><span>"requests served"</span></div>
            </div>
        </div>
        {ecosystem_stats(&data)}
        <h2>"Indexes"</h2>
        <div class="index-grid">{overlay_cards}</div>
        {standalone_cards}
    }
}

/// A virtual index drawn as what it is: an ordered stack of layers under one route, resolved top to
/// bottom with the first file match winning.
#[component]
fn OverlayCard(index: UiIndex, all: Vec<UiIndex>, counters: Option<UiCounters>) -> impl IntoView {
    let browse = browse_index_url(&index.route);
    let stats_href = stats_index_url(&index.route);
    let simple = simple_index_url(&index.route);
    let upload_to = index.upload_to.clone();
    let layers = index
        .layers
        .iter()
        .enumerate()
        .map(|(position, name)| {
            let member = all.iter().find(|candidate| candidate.name == *name).cloned();
            let kind = member
                .as_ref()
                .map_or_else(|| "?".to_owned(), |member| member.kind.clone());
            let route = member.as_ref().map(|member| simple_index_url(&member.route));
            let is_upload_target = upload_to.as_deref() == Some(name.as_str());
            view! {
                <li class="layer">
                    <span class="layer-order">{position + 1}</span>
                    <span class="layer-name">{name.clone()}</span>
                    <span class=format!("badge kind-{kind}")>{kind.clone()}</span>
                    {is_upload_target
                        .then(|| view! { <span class="badge uploads">"uploads land here"</span> })}
                    {route.map(|route| view! { <code class="layer-route">{route}</code> })}
                </li>
            }
        })
        .collect_view();
    let usage = counters.map(|c| {
        view! {
            <p class="card-usage">
                <span>{c.pages}" pages"</span>
                <span>{c.downloads}" downloads"</span>
                <span>{human_size(c.bytes)}" served"</span>
                <a href=stats_href.clone()>"usage"</a>
            </p>
        }
    });
    view! {
        <div class="card virtual-card">
            <div class="card-head">
                <a href=browse class="card-title">{index.name.clone()}</a>
                <span class="badge kind-virtual">"virtual"</span>
                {index.uploads.then(|| view! { <span class="badge uploads">"uploads"</span> })}
            </div>
            <p class="dim"><code>{simple}</code></p>
            <ol class="layer-stack">{layers}</ol>
            <p class="layer-hint">"resolves top to bottom; first file match wins"</p>
            {usage}
        </div>
    }
}

#[component]
fn IndexCard(index: UiIndex, counters: Option<UiCounters>) -> impl IntoView {
    let browse = browse_index_url(&index.route);
    let stats_href = stats_index_url(&index.route);
    let simple = simple_index_url(&index.route);
    let layers = (!index.layers.is_empty()).then(|| {
        view! {
            <p class="layers">
                "layers: "
                {index.layers.iter().map(|layer| view! { <code>{layer.clone()}</code> }).collect_view()}
            </p>
        }
    });
    let usage = counters.map(|c| {
        view! {
            <p class="card-usage">
                <span>{c.pages}" pages"</span>
                <span>{c.downloads}" downloads"</span>
                <span>{human_size(c.bytes)}" served"</span>
                <a href=stats_href.clone()>"usage"</a>
            </p>
        }
    });
    view! {
        <div class="card">
            <div class="card-head">
                <a href=browse class="card-title">{index.name.clone()}</a>
                <span class=format!("badge kind-{}", index.kind)>{index.kind.clone()}</span>
                {index.uploads.then(|| view! { <span class="badge uploads">"uploads"</span> })}
            </div>
            <p class="dim"><code>{simple}</code></p>
            {layers}
            {usage}
        </div>
    }
}

#[component]
pub fn Search() -> impl IntoView {
    let query_map = use_query_map();
    let query = Memo::new(move |_| query_map.read().get("q").unwrap_or_default());
    let source_type = Memo::new(move |_| {
        query_map
            .read()
            .get("type")
            .filter(|value| matches!(value.as_str(), "uploaded" | "cached" | "override"))
            .unwrap_or_else(|| "all".to_owned())
    });
    let page = Memo::new(move |_| {
        query_map
            .read()
            .get("page")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1)
    });
    let page_size = Memo::new(move |_| {
        let size = query_map
            .read()
            .get("page_size")
            .and_then(|value| value.parse::<usize>().ok());
        size.filter(|size| matches!(size, 25 | 50 | 100)).unwrap_or(25)
    });
    let results = Resource::new(
        move || (query.get(), source_type.get(), page.get(), page_size.get()),
        |(query, source_type, page, page_size)| load_search(query, source_type, page, page_size),
    );
    view! {
        <section class="page search-page">
            <h1>"Package search"</h1>
            <SearchForm query=query.get() source_type=source_type.get() page_size=page_size.get() />
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || {
                    let query = query.get();
                    let source_type = source_type.get();
                    Suspend::new(async move {
                        match results.await {
                            Ok(page) => view! { <SearchResults query source_type page_data=page /> }.into_any(),
                            Err(message) => view! { <ErrorMessage message /> }.into_any(),
                        }
                    })
                }}
            </Suspense>
        </section>
    }
}

#[component]
fn SearchForm(query: String, source_type: String, page_size: usize) -> impl IntoView {
    view! {
        <form class="search-controls" method="get" action="/search">
            <input class="search" type="search" name="q" value=query placeholder="Search packages" />
            <select name="type" aria-label="Source type">
                <option value="all" selected=source_type == "all">"All"</option>
                <option value="uploaded" selected=source_type == "uploaded">"Uploaded"</option>
                <option value="cached" selected=source_type == "cached">"Cached"</option>
                <option value="override" selected=source_type == "override">"Override"</option>
            </select>
            <select
                name="page_size"
                aria-label="Page size"
                on:change:target=move |event| store_search_page_size(&event.target().value())
            >
                <option value="25" selected=page_size == 25>"25"</option>
                <option value="50" selected=page_size == 50>"50"</option>
                <option value="100" selected=page_size == 100>"100"</option>
            </select>
            <button type="submit">"Search"</button>
        </form>
    }
}

fn store_search_page_size(value: &str) {
    #[cfg(feature = "hydrate")]
    {
        if let Some(window) = web_sys::window()
            && let Ok(Some(storage)) = window.local_storage()
        {
            let _ = storage.set_item("velodex.search.page_size", value);
        }
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = value;
    }
}

#[component]
fn SearchResults(query: String, source_type: String, page_data: UiSearchPage) -> impl IntoView {
    if page_data.total == 0 {
        let message = if query.trim().is_empty() {
            "No indexed packages yet. Cached projects appear after their pages are fetched."
        } else {
            "No packages matched this search."
        };
        return view! { <p class="dim">{message}</p> }.into_any();
    }
    let start = page_data.page.saturating_sub(1).saturating_mul(page_data.page_size) + 1;
    let end = page_data.total.min(start + page_data.results.len().saturating_sub(1));
    let previous =
        (page_data.page > 1).then(|| search_page_url(&query, &source_type, page_data.page - 1, page_data.page_size));
    let next =
        (end < page_data.total).then(|| search_page_url(&query, &source_type, page_data.page + 1, page_data.page_size));
    view! {
        <p class="result-count">"Showing "{start}"-"{end}" of "{page_data.total}</p>
        <div class="table-scroll">
            <table class="files search-results">
                <thead>
                    <tr>
                        <th>"Package"</th>
                        <th>"Normalized"</th>
                        <th>"Source"</th>
                        <th>"Index"</th>
                        <th>"Summary"</th>
                    </tr>
                </thead>
                <tbody>
                    {page_data
                        .results
                        .into_iter()
                        .map(|result| {
                            let href = browse_project_url(&result.route, &result.normalized_name);
                            let source_class = format!("badge source-{}", result.source_type);
                            let source_title = (result.source_type == "override")
                                .then_some("Hosted files or hosted overrides affect this upstream package");
                            view! {
                                <tr>
                                    <td><a href=href>{result.display_name}</a></td>
                                    <td><code>{result.normalized_name}</code></td>
                                    <td><span class=source_class title=source_title>{source_label(&result.source_type)}</span></td>
                                    <td><code>{result.index}</code></td>
                                    <td>{result.summary.unwrap_or_default()}</td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </div>
        <nav class="pagination" aria-label="Search pages">
            {previous
                .map_or_else(
                    || view! { <span class="page-link disabled">"Previous"</span> }.into_any(),
                    |href| view! { <a class="page-link" href=href>"Previous"</a> }.into_any(),
                )}
            <span>"Page "{page_data.page}</span>
            {next
                .map_or_else(
                    || view! { <span class="page-link disabled">"Next"</span> }.into_any(),
                    |href| view! { <a class="page-link" href=href>"Next"</a> }.into_any(),
                )}
        </nav>
    }
    .into_any()
}

/// The browse page: a searchable project list, one project's detail, or an archive's contents,
/// selected by query parameters.
#[component]
pub fn Browse() -> impl IntoView {
    let query = use_query_map();
    let route = Memo::new(move |_| query.read().get("index").unwrap_or_default());
    let project = Memo::new(move |_| query.read().get("project").filter(|name| !name.is_empty()));
    let file = Memo::new(move |_| query.read().get("file").filter(|name| !name.is_empty()));
    let sha256 = Memo::new(move |_| query.read().get("sha256").filter(|digest| !digest.is_empty()));
    let member = Memo::new(move |_| query.read().get("member").filter(|name| !name.is_empty()));
    let containers = Memo::new(move |_| {
        query
            .read()
            .get_all("container")
            .unwrap_or_default()
            .into_iter()
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>()
    });
    let offset = Memo::new(move |_| {
        query
            .read()
            .get("offset")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default()
    });
    view! {
        <section class="page">
            {move || match (project.get(), sha256.get(), file.get()) {
                (Some(name), Some(sha256), Some(file)) => {
                    view! {
                        <ArchiveView
                            route=route.get()
                            project=name
                            sha256
                            filename=file
                            containers=containers.get()
                            member=member.get()
                            offset=offset.get()
                        />
                    }.into_any()
                }
                (Some(name), None, Some(file)) => {
                    let (sha256, filename) = split_legacy_archive_file(&file);
                    view! {
                        <ArchiveView
                            route=route.get()
                            project=name
                            sha256
                            filename
                            containers=containers.get()
                            member=member.get()
                            offset=offset.get()
                        />
                    }.into_any()
                }
                (Some(name), _, None) => view! { <ProjectView route=route.get() project=name /> }.into_any(),
                (None, _, _) => view! { <IndexView route=route.get() /> }.into_any(),
            }}
        </section>
    }
}

#[component]
fn IndexView(route: String) -> impl IntoView {
    let projects = Resource::new(
        {
            let route = route.clone();
            move || route.clone()
        },
        load_projects,
    );
    let (filter, set_filter) = signal(String::new());
    let heading = route.clone();
    view! {
        <h1><code>{heading}</code></h1>
        <input
            class="search"
            type="search"
            placeholder="Filter projects…"
            on:input:target=move |event| set_filter.set(event.target().value())
        />
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                Suspend::new(async move {
                    match projects.await {
                        Ok(names) => view! { <ProjectList route names filter /> }.into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })
            }}
        </Suspense>
    }
}

#[component]
fn ProjectList(route: String, names: Vec<String>, filter: ReadSignal<String>) -> impl IntoView {
    let empty = names.is_empty();
    view! {
        <ul class="project-list">
            {move || {
                let needle = filter.get().to_lowercase();
                names
                    .iter()
                    .filter(|name| name.to_lowercase().contains(&needle))
                    .map(|name| {
                        let href = browse_project_url(&route, name);
                        view! { <li><a href=href>{name.clone()}</a></li> }
                    })
                    .collect_view()
            }}
        </ul>
        {empty.then(|| view! { <p class="dim">"No projects observed on this index yet."</p> })}
    }
}

#[component]
fn ErrorMessage(message: String) -> impl IntoView {
    view! { <p class="error">{message}</p> }
}

#[component]
fn ProjectView(route: String, project: String) -> impl IntoView {
    let page = Resource::new(
        {
            let key = (route.clone(), project.clone());
            move || key.clone()
        },
        |(route, project)| load_project(route, project),
    );
    // Admin state lives here, outside the Suspend scope: signals created inside async-hydrated
    // suspense content are disposed once hydration completes, which would make them inert.
    let (token, set_token) = signal(String::new());
    let (outcome, set_outcome) = signal(String::new());
    view! {
        <p class="breadcrumb">
            <a href=browse_index_url(&route)>{route.clone()}</a>
            " / "
            <span>{project}</span>
        </p>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                Suspend::new(async move {
                    match page.await {
                        Ok(Some((ui, doc))) => {
                            view! { <ProjectBody route ui doc refresh=page token set_token set_outcome /> }
                                .into_any()
                        }
                        Ok(None) => view! { <p class="dim">"Project not found on this index."</p> }.into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })
            }}
        </Suspense>
        {move || {
            let text = outcome.get();
            (!text.is_empty()).then(|| view! { <p class="outcome">{text}</p> })
        }}
    }
}

#[component]
fn ProjectBody(
    route: String,
    ui: UiProject,
    doc: Option<CoreMetadataDoc>,
    refresh: ProjectPageResource,
    token: ReadSignal<String>,
    set_token: WriteSignal<String>,
    set_outcome: WriteSignal<String>,
) -> impl IntoView {
    let doc = doc.unwrap_or_default();
    let latest = ui.versions.last().cloned().unwrap_or_else(|| doc.version.clone());
    let install = format!("uv pip install --index-url {} {}", simple_index_url(&route), ui.name);
    let description = render_description(&doc);
    view! {
        <header class="project-head">
            <h1>{ui.name.clone()} <span class="version">{latest}</span></h1>
            {doc.summary.clone().map(|summary| view! { <p class="summary">{summary}</p> })}
            <InstallSnippet install />
        </header>
        <div class="project-grid">
            <div class="project-main">
                <h2>"Description"</h2>
                <div class="description" inner_html=description></div>
                <h2>"Files"</h2>
                <FileTable route=route.clone() project=ui.name.clone() files=ui.files.clone() />
                <AdminPanel route=route project=ui.name.clone() versions=ui.versions.clone() refresh token set_token set_outcome />
            </div>
            <aside class="project-side">
                <MetaPanel doc versions=ui.versions />
            </aside>
        </div>
    }
}

#[component]
fn InstallSnippet(install: String) -> impl IntoView {
    let shown = install.clone();
    view! {
        <div class="install">
            <code>{shown}</code>
            <button class="copy" title="Copy" on:click=move |_| copy_to_clipboard(&install)>"copy"</button>
        </div>
    }
}

fn copy_to_clipboard(text: &str) {
    #[cfg(feature = "hydrate")]
    {
        if let Some(window) = web_sys::window() {
            let _ = window.navigator().clipboard().write_text(text);
        }
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = text;
    }
}

#[component]
fn FileTable(route: String, project: String, files: Vec<UiFile>) -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let files = Arc::new(files);
    let filenames = Arc::new(
        files
            .iter()
            .map(|file| file.filename.to_lowercase())
            .collect::<Vec<_>>(),
    );
    let initial = FileSearch::from_query(&query.read());
    let (initial_matches, initial_error) =
        match matching_file_indexes(&files, &filenames, &initial.pattern, initial.mode) {
            Ok(indexes) => (indexes, None),
            Err(message) => ((0..files.len()).collect(), Some(message)),
        };
    let (pattern, set_pattern) = signal(initial.pattern);
    let (mode, set_mode) = signal(initial.mode);
    let (matches, set_matches) = signal(initial_matches);
    let (error, set_error) = signal(initial_error);
    let total = files.len();
    Effect::new(move |_| {
        let search = FileSearch::from_query(&query.read());
        if pattern.get_untracked() != search.pattern {
            set_pattern.set(search.pattern);
        }
        if mode.get_untracked() != search.mode {
            set_mode.set(search.mode);
        }
    });
    Effect::new({
        let files = files.clone();
        move |_| match matching_file_indexes(&files, &filenames, &pattern.get(), mode.get()) {
            Ok(indexes) => {
                set_error.set(None);
                set_matches.set(indexes);
            }
            Err(message) => set_error.set(Some(message)),
        }
    });
    view! {
        <div class="file-filter">
            <input
                class="search file-search"
                type="search"
                placeholder="Filter filenames"
                prop:value=move || pattern.get()
                on:input:target={
                    let navigate = navigate.clone();
                    let route = route.clone();
                    let project = project.clone();
                    move |event| {
                        let next = event.target().value();
                        let replace = pattern.get_untracked().is_empty() == next.is_empty();
                        set_pattern.set(next.clone());
                        navigate(
                            &browse_project_file_search_url(
                                &route,
                                &project,
                                &next,
                                mode.get_untracked() == FileSearchMode::Regex,
                            ),
                            NavigateOptions { replace, scroll: false, ..NavigateOptions::default() },
                        );
                    }
                }
            />
            <label class="file-filter-mode">
                <input
                    type="checkbox"
                    prop:checked=move || mode.get() == FileSearchMode::Regex
                    on:change:target={
                        let navigate = navigate.clone();
                        let route = route.clone();
                        let project = project.clone();
                        move |event| {
                            let next = if event.target().checked() {
                                FileSearchMode::Regex
                            } else {
                                FileSearchMode::Substring
                            };
                            set_mode.set(next);
                            navigate(
                                &browse_project_file_search_url(
                                    &route,
                                    &project,
                                    &pattern.get_untracked(),
                                    next == FileSearchMode::Regex,
                                ),
                                NavigateOptions { replace: false, scroll: false, ..NavigateOptions::default() },
                            );
                        }
                    }
                />
                <span>"regex"</span>
            </label>
            <span class="file-filter-count">
                {move || file_count(matches.with(Vec::len), total)}
            </span>
        </div>
        {move || error.get().map(|message| view! { <p class="error">{message}</p> })}
        <table class="files">
            <thead><tr><th>"File"</th><th>"Size"</th><th>"Uploaded"</th><th>"sha256"</th><th>"Flags"</th></tr></thead>
            <tbody>
                {move || {
                    if matches.with(Vec::is_empty) {
                        view! { <tr><td colspan="5" class="empty">"No artifacts match this filename filter."</td></tr> }
                            .into_any()
                    } else {
                        matches
                            .get()
                            .into_iter()
                            .map(|index| file_row(&route, &project, &files[index]))
                            .collect_view()
                            .into_any()
                    }
                }}
            </tbody>
        </table>
    }
}

fn file_row(route: &str, project: &str, file: &UiFile) -> impl IntoView {
    let class = if file.yanked { "yanked" } else { "" };
    let inspect = browse_archive_url(route, project, &file.sha256, &file.filename);
    let short_hash = file.sha256.get(..12).unwrap_or_default().to_owned();
    view! {
        <tr class=class>
            <td>
                <a href=file.url.clone()>{file.filename.clone()}</a>
                {supports_archive_browser(&file.filename)
                    .then(|| view! {
                        " · "
                        <a class="inspect" href=inspect>"contents"</a>
                    })}
            </td>
            <td>{file.size.map_or_else(|| "—".to_owned(), human_size)}</td>
            <td>{file.upload_time.clone().map_or_else(|| "—".to_owned(), |time| time.chars().take(10).collect())}</td>
            <td><code title=file.sha256.clone()>{short_hash}</code></td>
            <td>
                {file.yanked.then(|| view! { <span class="badge yanked-badge">"yanked"</span> })}
                {file.has_metadata.then(|| view! { <span class="badge meta-badge">"metadata"</span> })}
            </td>
        </tr>
    }
}

fn file_count(shown: usize, total: usize) -> String {
    if shown == total {
        format!("{total} {}", file_label(total))
    } else {
        format!("{shown} of {total} {}", file_label(total))
    }
}

fn file_label(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSearch {
    pattern: String,
    mode: FileSearchMode,
}

impl FileSearch {
    fn from_query(query: &leptos_router::params::ParamsMap) -> Self {
        Self {
            pattern: query.get("filename").unwrap_or_default(),
            mode: FileSearchMode::from_query(query.get_str("filename_match")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileSearchMode {
    Substring,
    Regex,
}

impl FileSearchMode {
    fn from_query(value: Option<&str>) -> Self {
        match value {
            Some("regex") => Self::Regex,
            _ => Self::Substring,
        }
    }
}

fn matching_file_indexes(
    files: &[UiFile],
    filenames: &[String],
    pattern: &str,
    mode: FileSearchMode,
) -> Result<Vec<usize>, String> {
    if pattern.is_empty() {
        return Ok((0..files.len()).collect());
    }
    match mode {
        FileSearchMode::Substring => {
            let needle = pattern.to_lowercase();
            Ok(filenames
                .iter()
                .enumerate()
                .filter_map(|(index, filename)| filename.contains(&needle).then_some(index))
                .collect())
        }
        FileSearchMode::Regex => {
            let regex = Regex::new(pattern).map_err(|err| format!("Invalid regex: {err}"))?;
            Ok(files
                .iter()
                .enumerate()
                .filter_map(|(index, file)| regex.is_match(&file.filename).then_some(index))
                .collect())
        }
    }
}

fn supports_archive_browser(filename: &str) -> bool {
    let path = std::path::Path::new(filename);
    path.extension().is_some_and(|ext| {
        ext.eq_ignore_ascii_case("whl")
            || ext.eq_ignore_ascii_case("zip")
            || ext.eq_ignore_ascii_case("egg")
            || ext.eq_ignore_ascii_case("tar")
    }) || filename
        .get(filename.len().saturating_sub(7)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar.gz"))
        || filename
            .get(filename.len().saturating_sub(4)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tgz"))
}

/// The archive browser: member listing of one distribution, or one member's content.
#[component]
fn ArchiveView(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    member: Option<String>,
    offset: u64,
) -> impl IntoView {
    let back = browse_project_url(&route, &project);
    view! {
        <p class="breadcrumb">
            <a href=browse_index_url(&route)>{route.clone()}</a>
            " / "
            <a href=back>{project.clone()}</a>
            " / "
            <ArchiveBreadcrumb route=route.clone() project=project.clone() sha256=sha256.clone() filename=filename.clone() containers=containers.clone() />
        </p>
        {match member {
            Some(path) => {
                view! { <MemberView route project sha256 filename containers member=path offset /> }.into_any()
            }
            None => view! { <MemberList route project sha256 filename containers /> }.into_any(),
        }}
    }
}

#[component]
fn ArchiveBreadcrumb(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
) -> impl IntoView {
    let root = browse_archive_listing_url(&route, &project, &sha256, &filename, &[]);
    let filename_view = if containers.is_empty() {
        view! { <span>{filename.clone()}</span> }.into_any()
    } else {
        view! { <a href=root>{filename.clone()}</a> }.into_any()
    };
    view! {
        {filename_view}
        {containers
            .iter()
            .enumerate()
            .map(|(position, container)| {
                let next = position + 1;
                let prefix = containers[..next].to_vec();
                let href = browse_archive_listing_url(&route, &project, &sha256, &filename, &prefix);
                let container = container.clone();
                if next == containers.len() {
                    view! { " / " <span>{container}</span> }.into_any()
                } else {
                    view! { " / " <a href=href>{container}</a> }.into_any()
                }
            })
            .collect_view()}
    }
}

#[component]
fn MemberList(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
) -> impl IntoView {
    let members = Resource::new(
        {
            let key = (route.clone(), sha256.clone(), filename.clone(), containers.clone());
            move || key.clone()
        },
        |(route, sha256, filename, containers)| load_members(route, sha256, filename, containers),
    );
    let heading = containers.last().cloned().unwrap_or_else(|| filename.clone());
    view! {
        <h1><code>{heading}</code></h1>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let project = project.clone();
                let sha256 = sha256.clone();
                let filename = filename.clone();
                let containers = containers.clone();
                Suspend::new(async move {
                    match members.await {
                        Ok(entries) => view! { <ArchiveTree route project sha256 filename containers entries /> }
                            .into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })
            }}
        </Suspense>
    }
}

#[component]
fn ArchiveTree(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    entries: Vec<UiMember>,
) -> impl IntoView {
    let nodes = archive_tree(entries);
    if nodes.is_empty() {
        return view! { <p class="dim">"No files found in this archive."</p> }.into_any();
    }
    view! {
        <ul class="archive-tree">
            {nodes
                .into_iter()
                .map(|node| {
                    view! {
                        <ArchiveTreeNode
                            route=route.clone()
                            project=project.clone()
                            sha256=sha256.clone()
                            filename=filename.clone()
                            containers=containers.clone()
                            node
                        />
                    }
                })
                .collect_view()}
        </ul>
    }
    .into_any()
}

#[component]
fn ArchiveTreeNode(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    node: ArchiveNode,
) -> impl IntoView {
    let ArchiveNode {
        name,
        path,
        size,
        kind,
        previewable,
        directory,
        children,
    } = node;
    if directory {
        return view! {
            <li>
                <details open>
                    <summary><span class="archive-name folder">{name}</span></summary>
                    <ul>
                        {children
                            .into_iter()
                            .map(|child| {
                                view! {
                                    <ArchiveTreeNode
                                        route=route.clone()
                                        project=project.clone()
                                        sha256=sha256.clone()
                                        filename=filename.clone()
                                        containers=containers.clone()
                                        node=child
                                    />
                                }
                            })
                            .collect_view()}
                    </ul>
                </details>
            </li>
        }
        .into_any();
    }
    let size = size.unwrap_or_default();
    let label = view! {
        <span class="archive-meta">{human_size(size)}" · "{kind.clone()}</span>
    };
    let class = format!("archive-name kind-{kind}");
    view! {
        <li>
            {if kind == "archive" {
                let mut next_containers = containers;
                next_containers.push(path);
                let href = browse_archive_listing_url(&route, &project, &sha256, &filename, &next_containers);
                view! { <a class=class href=href>{name}</a> }.into_any()
            } else if previewable {
                let href = browse_archive_member_url(&route, &project, &sha256, &filename, &containers, &path, 0);
                view! { <a class=class href=href>{name}</a> }.into_any()
            } else {
                view! { <span class=class>{name}</span> }.into_any()
            }}
            {label}
        </li>
    }
    .into_any()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ArchiveNode {
    name: String,
    path: String,
    size: Option<u64>,
    kind: String,
    previewable: bool,
    directory: bool,
    children: Vec<Self>,
}

#[derive(Default)]
struct ArchiveBranch {
    directories: std::collections::BTreeMap<String, Self>,
    files: Vec<UiMember>,
}

fn archive_tree(entries: Vec<UiMember>) -> Vec<ArchiveNode> {
    let mut root = ArchiveBranch::default();
    for entry in entries {
        root.insert(entry);
    }
    root.into_nodes("")
}

impl ArchiveBranch {
    fn insert(&mut self, entry: UiMember) {
        let parts = entry.path.split('/').map(str::to_owned).collect::<Vec<_>>();
        let mut branch = self;
        for directory in parts.iter().take(parts.len().saturating_sub(1)) {
            branch = branch.directories.entry(directory.clone()).or_default();
        }
        branch.files.push(entry);
    }

    fn into_nodes(self, prefix: &str) -> Vec<ArchiveNode> {
        self.directories
            .into_iter()
            .map(|(name, branch)| {
                let path = archive_child_path(prefix, &name);
                ArchiveNode {
                    name,
                    path: path.clone(),
                    size: None,
                    kind: "folder".to_owned(),
                    previewable: false,
                    directory: true,
                    children: branch.into_nodes(&path),
                }
            })
            .chain(self.files.into_iter().map(|file| {
                let name = file.path.rsplit('/').next().unwrap_or(&file.path).to_owned();
                ArchiveNode {
                    name,
                    path: file.path,
                    size: Some(file.size),
                    kind: file.kind,
                    previewable: file.previewable,
                    directory: false,
                    children: Vec::new(),
                }
            }))
            .collect()
    }
}

fn archive_child_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

#[component]
fn MemberView(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    member: String,
    offset: u64,
) -> impl IntoView {
    let content = Resource::new(
        {
            let key = (
                route.clone(),
                sha256.clone(),
                filename.clone(),
                containers.clone(),
                member.clone(),
                offset,
            );
            move || key.clone()
        },
        |(route, sha256, filename, containers, member, offset)| {
            load_member_chunk(route, sha256, filename, containers, member, offset)
        },
    );
    let back = browse_archive_listing_url(&route, &project, &sha256, &filename, &containers);
    view! {
        <h1><code>{member.clone()}</code></h1>
        <p><a href=back>"back to archive"</a></p>
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let project = project.clone();
                let sha256 = sha256.clone();
                let filename = filename.clone();
                let containers = containers.clone();
                let member = member.clone();
                Suspend::new(async move {
                    match content.await {
                        Ok(chunk) => view! { <MemberChunk route project sha256 filename containers member chunk /> }
                            .into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })
            }}
        </Suspense>
    }
}

#[component]
fn MemberChunk(
    route: String,
    project: String,
    sha256: String,
    filename: String,
    containers: Vec<String>,
    member: String,
    chunk: UiMemberChunk,
) -> impl IntoView {
    let next = chunk
        .next_offset
        .map(|offset| browse_archive_member_url(&route, &project, &sha256, &filename, &containers, &member, offset));
    let end = chunk
        .next_offset
        .or(chunk.size)
        .unwrap_or_else(|| chunk.offset + chunk.text.len() as u64);
    let range = chunk.size.map(|size| {
        view! { <p class="dim">"bytes "{chunk.offset}"-"{end}" of "{size}</p> }
    });
    view! {
        {range}
        <pre class="member-content"><code>{chunk.text}</code></pre>
        {next.map(|href| view! { <p><a class="button-link" href=href>"next chunk"</a></p> })}
    }
}

#[component]
fn MetaPanel(doc: CoreMetadataDoc, versions: Vec<String>) -> impl IntoView {
    view! {
        <h3>"Versions"</h3>
        <p class="chips">{versions.into_iter().map(|version| view! { <code>{version}</code> }).collect_view()}</p>
        {doc.requires_python.map(|value| view! { <h3>"Requires Python"</h3><p><code>{value}</code></p> })}
        {doc.license.map(|value| view! { <h3>"License"</h3><p>{value}</p> })}
        {doc.author.map(|value| view! { <h3>"Author"</h3><p>{value}</p> })}
        {doc.maintainer.map(|value| view! { <h3>"Maintainer"</h3><p>{value}</p> })}
        {(!doc.keywords.is_empty()).then(|| view! {
            <h3>"Keywords"</h3>
            <p class="chips">{doc.keywords.into_iter().map(|word| view! { <code>{word}</code> }).collect_view()}</p>
        })}
        {(!doc.requires_dist.is_empty()).then(|| view! {
            <h3>"Dependencies"</h3>
            <p class="chips">
                {doc.requires_dist.into_iter().map(|dep| view! { <code>{dep}</code> }).collect_view()}
            </p>
        })}
        {(!doc.project_urls.is_empty()).then(|| view! {
            <h3>"Links"</h3>
            <ul class="links-list">
                {doc.project_urls
                    .into_iter()
                    .map(|(label, url)| view! { <li><a href=url rel="external">{label}</a></li> })
                    .collect_view()}
            </ul>
        })}
        <ClassifierGroups classifiers=doc.classifiers />
    }
}

/// Classifiers grouped by their top-level category, the way pypi.org presents them.
#[component]
fn ClassifierGroups(classifiers: Vec<String>) -> impl IntoView {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for classifier in classifiers {
        let (group, rest) = classifier.split_once(" :: ").map_or_else(
            || (classifier.clone(), classifier.clone()),
            |(g, r)| (g.to_owned(), r.to_owned()),
        );
        match groups.iter_mut().find(|(name, _)| *name == group) {
            Some((_, values)) => values.push(rest),
            None => groups.push((group, vec![rest])),
        }
    }
    (!groups.is_empty()).then(|| {
        view! {
            <h3>"Classifiers"</h3>
            {groups
                .into_iter()
                .map(|(group, values)| {
                    view! {
                        <p class="classifier-group">{group}</p>
                        <ul class="classifiers">
                            {values.into_iter().map(|value| view! { <li>{value}</li> }).collect_view()}
                        </ul>
                    }
                })
                .collect_view()}
        }
    })
}

/// Yank, un-yank, and delete for the index's hosted layer, driven from the browser with the upload
/// token. The buttons only act once hydrated; the server renders them inert.
#[component]
fn AdminPanel(
    route: String,
    project: String,
    versions: Vec<String>,
    refresh: ProjectPageResource,
    token: ReadSignal<String>,
    set_token: WriteSignal<String>,
    set_outcome: WriteSignal<String>,
) -> impl IntoView {
    let act = move |method: &'static str, url: String| {
        run_admin(method, url, token.get_untracked(), set_outcome, refresh);
    };
    let rows = versions
        .into_iter()
        .map(|version| {
            let yank_url = admin_version_url(&route, &project, &version, Some("yank"));
            let delete_url = admin_version_url(&route, &project, &version, None);
            let unyank_url = yank_url.clone();
            view! {
                <tr>
                    <td><code>{version}</code></td>
                    <td>
                        <button on:click=move |_| act("PUT", yank_url.clone())>"yank"</button>
                        <button on:click=move |_| act("DELETE", unyank_url.clone())>"un-yank"</button>
                        <button class="danger" on:click=move |_| act("DELETE", delete_url.clone())>"delete"</button>
                    </td>
                </tr>
            }
        })
        .collect_view();
    let delete_all = admin_project_url(&route, &project);
    view! {
        <details class="admin">
            <summary>"Manage uploads"</summary>
            <p class="dim">"Actions apply to files uploaded to this index's hosted layer and need its upload token."</p>
            <input
                class="token"
                type="password"
                placeholder="upload token"
                on:input:target=move |event| set_token.set(event.target().value())
            />
            <table class="admin-table"><tbody>{rows}</tbody></table>
            <button class="danger" on:click=move |_| act("DELETE", delete_all.clone())>"delete whole project"</button>
        </details>
    }
}

fn run_admin(
    method: &'static str,
    url: String,
    token: String,
    outcome: WriteSignal<String>,
    refresh: ProjectPageResource,
) {
    #[cfg(feature = "hydrate")]
    {
        leptos::task::spawn_local(async move {
            let result = crate::data::admin_request(method, &url, &token).await;
            outcome.set(result);
            refresh.refetch();
        });
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = (method, url, token, outcome, refresh);
    }
}

/// Render a byte count like pypi.org: one decimal in the largest fitting unit.
fn human_size(bytes: u64) -> String {
    #[allow(
        clippy::cast_precision_loss,
        reason = "display only; artifacts are far below 2^52 bytes"
    )]
    let mut value = bytes as f64;
    for unit in ["B", "kB", "MB", "GB"] {
        if value < 1024.0 {
            return format!("{value:.1} {unit}");
        }
        value /= 1024.0;
    }
    format!("{value:.1} TB")
}

fn split_legacy_archive_file(file: &str) -> (String, String) {
    file.split_once('/')
        .map(|(sha256, filename)| (sha256.to_owned(), filename.to_owned()))
        .unwrap_or_default()
}

/// The usage statistics drill-down: every index, one index's projects, or one project's files,
/// selected by query parameters, with the aggregate of the current level on top.
#[component]
pub fn Stats() -> impl IntoView {
    let query = use_query_map();
    let route = Memo::new(move |_| query.read().get("index").filter(|name| !name.is_empty()));
    let project = Memo::new(move |_| query.read().get("project").filter(|name| !name.is_empty()));
    view! {
        <section class="page">
            {move || {
                let key = (route.get(), project.get());
                view! { <StatsView route=key.0 project=key.1 /> }
            }}
        </section>
    }
}

#[component]
fn StatsView(route: Option<String>, project: Option<String>) -> impl IntoView {
    let stats = Resource::new(
        {
            let key = (route.clone(), project.clone());
            move || key.clone()
        },
        |(route, project)| load_stats(route, project),
    );
    let crumb = match (&route, &project) {
        (Some(index), Some(name)) => view! {
            <p class="breadcrumb">
                <a href="/stats">"usage"</a>
                " / "
                <a href=stats_index_url(index)>{index.clone()}</a>
                " / "
                <span>{name.clone()}</span>
            </p>
        }
        .into_any(),
        (Some(index), None) => view! {
            <p class="breadcrumb">
                <a href="/stats">"usage"</a>
                " / "
                <span>{index.clone()}</span>
            </p>
        }
        .into_any(),
        _ => view! { <p class="breadcrumb"><span>"usage"</span></p> }.into_any(),
    };
    view! {
        {crumb}
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let project = project.clone();
                Suspend::new(async move {
                    let data = stats.await;
                    view! { <StatsBody route project data /> }
                })
            }}
        </Suspense>
    }
}

#[component]
fn StatsBody(route: Option<String>, project: Option<String>, data: UiStats) -> impl IntoView {
    let totals = data.totals;
    let empty = data.rows.is_empty();
    let (label, rows) = match (&route, &project) {
        (Some(_), Some(_)) => ("File", file_rows(data.rows)),
        (Some(index), None) => ("Project", drill_rows(data.rows, |name| stats_project_url(index, name))),
        _ => ("Index", drill_rows(data.rows, stats_index_url)),
    };
    view! {
        <div class="stat-row">
            <div class="stat"><strong>{totals.pages}</strong><span>"pages"</span></div>
            <div class="stat"><strong>{totals.downloads}</strong><span>"downloads"</span></div>
            <div class="stat"><strong>{human_size(totals.bytes)}</strong><span>"served"</span></div>
            <div class="stat"><strong>{totals.metadata}</strong><span>"metadata hits"</span></div>
            <div class="stat"><strong>{totals.uploads}</strong><span>"uploads"</span></div>
            <div class="stat"><strong>{totals.refreshes}</strong><span>"refreshes"</span></div>
            <div class="stat"><strong>{totals.changed}</strong><span>"upstream changes"</span></div>
            <div class="stat"><strong>{totals.stale_served}</strong><span>"stale fallbacks"</span></div>
            <div class="stat"><strong>{totals.upstream_errors}</strong><span>"upstream errors"</span></div>
            <div class="stat"><strong>{totals.rejected}</strong><span>"rejected downloads"</span></div>
        </div>
        <table class="files stats-table">
            <thead>
                <tr>
                    <th>{label}</th><th>"Pages"</th><th>"Downloads"</th><th>"Served"</th>
                    <th>"Metadata"</th><th>"Uploads"</th>
                </tr>
            </thead>
            <tbody>{rows}</tbody>
        </table>
        {empty.then(|| view! { <p class="dim">"Nothing recorded at this level yet."</p> })}
    }
}

/// Rows whose names drill one level deeper.
fn drill_rows(rows: Vec<(String, UiCounters)>, href: impl Fn(&str) -> String) -> Vec<AnyView> {
    rows.into_iter()
        .map(|(name, c)| {
            let link = href(&name);
            view! {
                <tr>
                    <td><a href=link>{name}</a></td>
                    <td>{c.pages}</td>
                    <td>{c.downloads}</td>
                    <td>{human_size(c.bytes)}</td>
                    <td>{c.metadata}</td>
                    <td>{c.uploads}</td>
                </tr>
            }
            .into_any()
        })
        .collect()
}

/// Leaf rows: files have no deeper level to link to.
fn file_rows(rows: Vec<(String, UiCounters)>) -> Vec<AnyView> {
    rows.into_iter()
        .map(|(name, c)| {
            view! {
                <tr>
                    <td><code>{name}</code></td>
                    <td>{c.pages}</td>
                    <td>{c.downloads}</td>
                    <td>{human_size(c.bytes)}</td>
                    <td>{c.metadata}</td>
                    <td>{c.uploads}</td>
                </tr>
            }
            .into_any()
        })
        .collect()
}
