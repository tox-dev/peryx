use leptos::prelude::*;

use super::{
    ErrorMessage, LoadState, ecosystem_stats, human_size, optional_counters_for, retain, serial_stat, start_refresh,
    usage_or_error,
};
use crate::data::load_admin_overview;
use crate::model::{UiCounters, UiIndex, UiRecentWrite, UiSnapshot, UiStats, UiSummaryStatus};
use crate::url::{browse_index_url, stats_index_url};

#[component]
pub fn AdminStatus() -> impl IntoView {
    let overview = Resource::new(|| (), |()| load_admin_overview());
    let loaded = RwSignal::new(LoadState::default());
    start_refresh(overview);
    view! {
        <section class="page ops-page">
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || Suspend::new(async move {
                    let loaded = retain(loaded, overview.await);
                    view! {
                        {loaded.error.map(|message| view! { <ErrorMessage message /> })}
                        {loaded
                            .value
                            .map(|(data, usage)| {
                                let (usage, error) = usage_or_error(usage);
                                view! {
                                    {error.map(|message| view! { <ErrorMessage message /> })}
                                    <AdminStatusBody data usage />
                                }
                            })}
                    }
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn AdminStatusBody(data: UiSnapshot, usage: Option<UiStats>) -> impl IntoView {
    let empty_usage = usage
        .as_ref()
        .is_some_and(|usage| usage.totals == UiCounters::default());
    let indexes = data.indexes.clone();
    let empty = indexes.is_empty();
    let resource_count = summary_total(&indexes, |index| index.resource_count);
    let write_count = summary_total(&indexes, |index| index.write_count);
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
                <div class="stat"><strong>{serial_stat(data.serial)}</strong><span>"change serial"</span></div>
                <div class="stat"><strong>{data.requests}</strong><span>"accepted requests"</span></div>
                <div class="stat"><strong>{indexes.len()}</strong><span>"indexes"</span></div>
                <div class="stat"><strong>{kind_count(&indexes, "virtual")}</strong><span>"virtual"</span></div>
                <div class="stat"><strong>{resource_count}</strong><span>"observed resources"</span></div>
                <div class="stat"><strong>{write_count}</strong><span>"artifact writes"</span></div>
            </div>
        </div>
        {ecosystem_stats(&data)}
        <h2>"Indexes"</h2>
        <AdminIndexTable indexes=indexes.clone() all=indexes.clone() />
        {empty.then(|| view! { <p class="dim">"No indexes configured."</p> })}
        <h2>"Recent writes"</h2>
        <AdminRecentWrites indexes=indexes.clone() />
        <h2>"Usage and health"</h2>
        {usage.map(|usage| view! { <AdminUsageTable indexes usage /> })}
        {empty_usage.then(|| view! { <p class="dim">"No usage recorded yet."</p> })}
    }
}

fn kind_count(indexes: &[UiIndex], kind: &str) -> usize {
    indexes.iter().filter(|index| index.kind == kind).count()
}

fn summary_total(indexes: &[UiIndex], count: fn(&UiIndex) -> u64) -> String {
    if indexes
        .iter()
        .all(|index| index.summary_status == UiSummaryStatus::Available)
    {
        indexes.iter().map(count).sum::<u64>().to_string()
    } else {
        "unavailable".to_owned()
    }
}

fn summary_count(index: &UiIndex, count: u64) -> String {
    if index.summary_status == UiSummaryStatus::Available {
        count.to_string()
    } else {
        "unavailable".to_owned()
    }
}

#[component]
fn AdminIndexTable(indexes: Vec<UiIndex>, all: Vec<UiIndex>) -> impl IntoView {
    view! {
        <div class="table-scroll">
            <table class="artifacts ops-table">
                <thead>
                    <tr>
                        <th>"Name"</th>
                        <th>"Route"</th>
                        <th>"Type"</th>
                        <th>"Endpoint"</th>
                        <th>"Resources"</th>
                        <th>"Artifacts"</th>
                        <th>"Topology"</th>
                        <th>"Writes"</th>
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
                                    <td><a class="ops-endpoint" href=endpoint_href title=endpoint_title>{endpoint}</a></td>
                                    <td>{summary_count(&index, index.resource_count)}</td>
                                    <td>{summary_count(&index, index.write_count)}</td>
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
            "no write token"
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
fn AdminRecentWrites(indexes: Vec<UiIndex>) -> impl IntoView {
    let rows = indexes
        .into_iter()
        .flat_map(|index| {
            let name = index.name;
            index
                .recent_writes
                .into_iter()
                .map(move |write| recent_write_row(name.clone(), write))
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return view! { <p class="dim">"No writes recorded yet."</p> }.into_any();
    }
    view! {
        <div class="table-scroll">
            <table class="artifacts ops-table">
                <thead>
                    <tr>
                        <th>"Index"</th>
                        <th>"Resource"</th>
                        <th>"Artifact"</th>
                        <th>"Group"</th>
                        <th>"Written"</th>
                        <th>"Size"</th>
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
        </div>
    }
    .into_any()
}

fn recent_write_row(index: String, write: UiRecentWrite) -> AnyView {
    view! {
        <tr>
            <td>{index}</td>
            <td><code>{write.resource}</code></td>
            <td><code>{write.artifact}</code></td>
            <td>{write.group}</td>
            <td>{write.written_at.map_or_else(|| "n/a".to_owned(), |time| time.chars().take(10).collect())}</td>
            <td>{write.size.map_or_else(|| "n/a".to_owned(), human_size)}</td>
        </tr>
    }
    .into_any()
}

#[component]
fn AdminUsageTable(indexes: Vec<UiIndex>, usage: UiStats) -> impl IntoView {
    view! {
        <div class="table-scroll">
            <table class="artifacts ops-table">
                <thead>
                    <tr>
                        <th>"Index"</th>
                        <th>"Listings"</th>
                        <th>"Reads"</th>
                        <th>"Served"</th>
                        <th>"Metadata"</th>
                        <th>"Writes"</th>
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
                                    <td>{counters.reads}</td>
                                    <td>{human_size(counters.bytes)}</td>
                                    <td>{counters.metadata}</td>
                                    <td>{counters.writes}</td>
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

#[cfg(test)]
#[path = "../../tests/unit/pages/admin/tests.rs"]
mod tests;
