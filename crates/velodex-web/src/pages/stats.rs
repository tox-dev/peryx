#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use super::human_size;
use crate::data::load_stats;
use crate::model::{UiCounters, UiStats};
use crate::url::{stats_index_url, stats_project_url};

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
