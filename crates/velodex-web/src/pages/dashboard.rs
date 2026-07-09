#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;

use super::{ecosystem_stats, human_size, optional_counters_for, start_refresh};
use crate::data::{load_snapshot, load_stats};
use crate::model::{UiCounters, UiIndex, UiSnapshot, UiStats};
use crate::url::{browse_index_url, index_endpoint, stats_index_url};

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
    let simple = index_endpoint(&index.route, &index.ecosystem);
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
            let route = member
                .as_ref()
                .map(|member| index_endpoint(&member.route, &member.ecosystem));
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
                <span class=format!("badge ecosystem-{}", index.ecosystem)>{index.ecosystem.clone()}</span>
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
    let simple = index_endpoint(&index.route, &index.ecosystem);
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
                <span class=format!("badge ecosystem-{}", index.ecosystem)>{index.ecosystem.clone()}</span>
                <span class=format!("badge kind-{}", index.kind)>{index.kind.clone()}</span>
                {index.uploads.then(|| view! { <span class="badge uploads">"uploads"</span> })}
            </div>
            <p class="dim"><code>{simple}</code></p>
            {layers}
            {usage}
        </div>
    }
}
