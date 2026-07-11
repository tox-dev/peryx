#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;

use super::{ecosystem_stats, human_size, optional_counters_for, start_refresh};
use crate::data::{load_admin_snapshot, load_stats};
use crate::model::{UiCounters, UiIndex, UiRecentUpload, UiSnapshot, UiStats};
use crate::url::{browse_index_url, stats_index_url};

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
                        <th>"Endpoint"</th>
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
                            let endpoint = index.endpoint.clone();
                            let endpoint_href = endpoint.clone();
                            let endpoint_title = endpoint.clone();
                            view! {
                                <tr>
                                    <td><a href=browse>{index.name.clone()}</a></td>
                                    <td><code>{index.route.clone()}</code></td>
                                    <td class="ops-type">
                                        <span class=format!("badge ecosystem-{}", index.ecosystem)>{index.ecosystem.clone()}</span>
                                        <span class=format!("badge kind-{}", index.kind)>{index.kind.clone()}</span>
                                    </td>
                                    <td><a class="ops-simple" href=endpoint_href title=endpoint_title>{endpoint}</a></td>
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
