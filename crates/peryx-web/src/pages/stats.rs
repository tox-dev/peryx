use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use super::{human_size, reactive_value};
use crate::data::load_stats;
use crate::model::{UiCounters, UiStats};
use crate::url::{stats_index_url, stats_resource_url};

#[component]
pub fn Stats() -> impl IntoView {
    let query = use_query_map();
    #[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
    {
        let route = Memo::new(move |_| query.read().get("index").filter(|name| !name.is_empty()));
        let resource = Memo::new(move |_| query.read().get("resource").filter(|name| !name.is_empty()));
        view! {
            <section class="page">
                {move || {
                    let key = (reactive_value(&route), reactive_value(&resource));
                    view! { <StatsView route=key.0 resource=key.1 /> }
                }}
            </section>
        }
    }
    #[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
    {
        let query = reactive_value(&query);
        let route = match query.get("index") {
            Some(name) if !name.is_empty() => Some(name),
            _ => None,
        };
        let resource = match query.get("resource") {
            Some(name) if !name.is_empty() => Some(name),
            _ => None,
        };
        view! { <section class="page"><StatsView route resource /></section> }
    }
}

#[component]
fn StatsView(route: Option<String>, resource: Option<String>) -> impl IntoView {
    let stats = Resource::new(
        {
            let key = (route.clone(), resource.clone());
            move || key.clone()
        },
        |(route, resource)| load_stats(route, resource),
    );
    view! {
        <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
            {move || {
                let route = route.clone();
                let resource = resource.clone();
                Suspend::new(async move {
                    let data = stats.await;
                    view! { <StatsBody route resource data /> }
                })
            }}
        </Suspense>
    }
}

#[component]
fn StatsBody(route: Option<String>, resource: Option<String>, data: UiStats) -> impl IntoView {
    let totals = data.totals;
    let empty = data.rows.is_empty();
    let crumb = match (&route, &resource) {
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
    let (label, rows) = match (&route, &resource) {
        (Some(_), Some(_)) => ("Artifact", artifact_rows(data.rows)),
        (Some(index), None) => (
            "Resource",
            drill_rows(data.rows, |name| stats_resource_url(index, name)),
        ),
        _ => ("Index", drill_rows(data.rows, stats_index_url)),
    };
    view! {
        {crumb}
        <div class="stat-row">
            <div class="stat"><strong>{totals.pages}</strong><span>"listings"</span></div>
            <div class="stat"><strong>{totals.reads}</strong><span>"reads"</span></div>
            <div class="stat"><strong>{human_size(totals.bytes)}</strong><span>"served"</span></div>
            <div class="stat"><strong>{totals.metadata}</strong><span>"metadata hits"</span></div>
            <div class="stat"><strong>{totals.writes}</strong><span>"writes"</span></div>
            <div class="stat"><strong>{totals.refreshes}</strong><span>"refreshes"</span></div>
            <div class="stat"><strong>{totals.changed}</strong><span>"upstream changes"</span></div>
            <div class="stat"><strong>{totals.stale_served}</strong><span>"stale fallbacks"</span></div>
            <div class="stat"><strong>{totals.upstream_errors}</strong><span>"upstream errors"</span></div>
            <div class="stat"><strong>{totals.rejected}</strong><span>"rejected reads"</span></div>
        </div>
        <table class="files stats-table">
            <thead>
                <tr>
                    <th>{label}</th><th>"Listings"</th><th>"Reads"</th><th>"Served"</th>
                    <th>"Metadata"</th><th>"Writes"</th>
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
                    <td>{c.reads}</td>
                    <td>{human_size(c.bytes)}</td>
                    <td>{c.metadata}</td>
                    <td>{c.writes}</td>
                </tr>
            }
            .into_any()
        })
        .collect()
}

fn artifact_rows(rows: Vec<(String, UiCounters)>) -> Vec<AnyView> {
    rows.into_iter()
        .map(|(name, c)| {
            view! {
                <tr>
                    <td><code>{name}</code></td>
                    <td>{c.pages}</td>
                    <td>{c.reads}</td>
                    <td>{human_size(c.bytes)}</td>
                    <td>{c.metadata}</td>
                    <td>{c.writes}</td>
                </tr>
            }
            .into_any()
        })
        .collect()
}

#[cfg(test)]
#[cfg(feature = "ssr")]
#[path = "../../tests/unit/pages/stats/tests.rs"]
mod tests;
