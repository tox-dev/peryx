#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;

use super::{ecosystem_stats, human_size, optional_counters_for, start_refresh};
use crate::data::{load_snapshot, load_stats};
use crate::model::{UiCounters, UiIndex, UiSnapshot, UiStats};
use crate::url::{browse_index_url, index_endpoint, stats_index_url};

/// The diving-peregrine silhouette, traced from a photo. Shared by the home hero (dives once on load)
/// and the loading state (loops), each animated through its own `.falcon` class in the stylesheet.
const FALCON: &str = "M45.2 8.2C40.5 9.5 40.4 10.5 41.3 31.7C41.8 42.9 41.7 43.3 37.4 43.3C34.3 43.3 31.7 44.0 29.7 45.3C27.6 46.6 24.0 46.9 22.7 45.8C20.8 44.4 16.9 36.2 15.3 30.6C14.7 28.7 13.7 25.5 13.0 23.7C12.2 21.8 11.4 19.6 11.1 18.8C9.8 15.2 8.3 14.8 6.8 17.8C6.0 19.3 6.2 20.8 7.7 26.8C8.4 29.9 9.8 36.1 10.7 40.7C12.2 47.7 14.6 57.0 15.6 59.7C15.8 60.2 16.4 62.4 17.0 64.7C19.0 72.4 23.1 79.6 25.5 79.6C26.6 79.6 29.2 78.1 29.2 77.5C29.2 77.1 32.0 75.3 32.9 75.1C35.3 74.6 40.5 79.8 42.6 84.8C43.5 87.1 46.0 91.1 47.2 92.2C47.7 92.7 48.3 92.8 50.0 92.8C51.7 92.8 52.3 92.7 52.8 92.2C54.0 91.1 56.5 87.1 57.4 84.8C59.5 79.8 64.7 74.6 67.1 75.1C68.0 75.3 70.8 77.1 70.8 77.5C70.8 78.1 73.4 79.6 74.5 79.6C76.9 79.6 81.0 72.4 83.0 64.7C83.6 62.4 84.2 60.2 84.4 59.7C85.4 57.0 87.8 47.7 89.3 40.7C90.2 36.1 91.6 29.9 92.3 26.8C93.8 20.8 94.0 19.3 93.2 17.8C91.7 14.8 90.2 15.2 88.9 18.8C88.6 19.6 87.8 21.8 87.0 23.7C86.3 25.5 85.3 28.7 84.7 30.6C83.1 36.2 79.2 44.4 77.3 45.8C76.0 46.9 72.4 46.6 70.3 45.3C68.3 44.0 65.7 43.3 62.6 43.3C58.3 43.3 58.2 42.9 58.7 31.7C59.5 14.2 59.3 11.0 57.4 9.3C55.9 7.8 48.8 7.2 45.2 8.2Z";

/// The landing dashboard: identity, live counters, and the configured indexes with their usage.
#[component]
pub fn Dashboard() -> impl IntoView {
    let snapshot = Resource::new(|| (), |()| load_snapshot());
    let stats = Resource::new(|| (), |()| load_stats(None, None));
    start_refresh(snapshot);
    view! {
        <section class="page">
            <Suspense fallback=|| view! { <StoopLoader /> }>
                {move || Suspend::new(async move {
                    let data = snapshot.await;
                    let usage = stats.await;
                    view! { <DashboardBody data usage /> }
                })}
            </Suspense>
        </section>
    }
}

/// The home identity: the falcon in a full stoop, diving once on load, beside the wordmark and the
/// "artifact vault" descriptor. `prefers-reduced-motion` paints it settled.
#[component]
fn StoopHero(version: String) -> impl IntoView {
    view! {
        <div class="hero-brand">
            <span class="stoop-stage">
                <span class="streaks" aria-hidden="true"><span></span><span></span><span></span></span>
                <svg class="stoop" viewBox="0 0 100 100" role="img" aria-label="peryx logo, a diving peregrine falcon">
                    <defs>
                        <linearGradient id="peryxStoop" x1="0" y1="0" x2="1" y2="1">
                            <stop offset="0" stop-color="#F74C00" />
                            <stop offset="1" stop-color="#FFB600" />
                        </linearGradient>
                    </defs>
                    <path class="falcon" fill="url(#peryxStoop)" d=FALCON />
                </svg>
            </span>
            <span class="brand-text">
                <span class="wordmark">"peryx"</span>
                <span class="tagline">"the artifact vault · v"{version}</span>
            </span>
        </div>
    }
}

/// The loading state: the same stoop, looped, so a slow first paint still reads as peryx.
#[component]
fn StoopLoader() -> impl IntoView {
    view! {
        <div class="stoop-loader">
            <svg class="stoop" viewBox="0 0 100 100" aria-hidden="true">
                <defs>
                    <linearGradient id="peryxStoop" x1="0" y1="0" x2="1" y2="1">
                        <stop offset="0" stop-color="#F74C00" />
                        <stop offset="1" stop-color="#FFB600" />
                    </linearGradient>
                </defs>
                <path class="falcon" fill="url(#peryxStoop)" d=FALCON />
            </svg>
            <span class="cap">"loading"</span>
        </div>
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
        <StoopHero version=data.version.clone() />
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
