use leptos::prelude::*;

use super::{
    ErrorMessage, LoadState, ecosystem_stats, human_size, optional_counters_for, retain, start_refresh, usage_or_error,
};
use crate::data::{UiOverview, load_overview};
use crate::model::{UiCounters, UiIndex, UiSnapshot, UiStats};
use crate::url::{browse_index_url, stats_index_url};

/// The landing dashboard: identity, live counters, and the configured indexes with their usage.
#[component]
pub fn Dashboard() -> impl IntoView {
    let overview = Resource::new(|| (), |()| load_overview());
    let loaded = RwSignal::new(LoadState::default());
    start_refresh(overview);
    view! {
        <section class="page">
            <StoopHero overview=loaded />
            <Suspense fallback=|| view! { <StoopLoader /> }>
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
                                    <DashboardBody data usage />
                                }
                            })}
                    }
                })}
            </Suspense>
        </section>
    }
}

/// The home identity: the falcon in a full stoop, diving once on load, beside the wordmark and the
/// "artifact vault" descriptor. `prefers-reduced-motion` paints it settled.
///
/// The hero sits outside the body's `Suspense`, and only the version text awaits the snapshot. A
/// refetch every few seconds would otherwise rebuild this `<svg>`, and a fresh node restarts the
/// once-on-load dive, so the falcon would re-dive on every poll.
#[component]
fn StoopHero(overview: RwSignal<LoadState<UiOverview>>) -> impl IntoView {
    view! {
        <div class="hero-brand">
            <span class="stoop-stage">
                <span class="streaks" aria-hidden="true"><span></span><span></span><span></span></span>
                <img class="stoop falcon" src="/mark.svg" alt="peryx logo, a diving peregrine falcon" />
            </span>
            <span class="brand-text">
                <span class="wordmark">"peryx"</span>
                <span class="tagline">
                    "the artifact vault · v"
                    {move || overview.get().value.map(|(snapshot, _)| snapshot.version)}
                </span>
            </span>
        </div>
    }
}

/// The loading state: the same stoop, looped, so a slow first paint still reads as peryx.
#[component]
fn StoopLoader() -> impl IntoView {
    view! {
        <div class="stoop-loader">
            <img class="stoop falcon" src="/mark.svg" alt="" />
            <span class="cap">"loading"</span>
        </div>
    }
}

#[component]
fn DashboardBody(data: UiSnapshot, usage: Option<UiStats>) -> impl IntoView {
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
            let counters = usage
                .as_ref()
                .and_then(|usage| optional_counters_for(usage, &index.route));
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
                        let counters = usage
                            .as_ref()
                            .and_then(|usage| optional_counters_for(usage, &index.route));
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
                <div class="stat"><strong>{data.requests}</strong><span>"accepted requests"</span></div>
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
    let endpoint = index.endpoint.clone();
    let upload_to = index.upload_to.clone();
    let layers = index
        .layers
        .iter()
        .enumerate()
        .map(|(position, name)| {
            let member = all.iter().find(|candidate| candidate.name == *name);
            let kind = member.map_or_else(|| "?".to_owned(), |member| member.kind.clone());
            let route = member.map(|member| member.endpoint.clone());
            let is_upload_target = upload_to.as_deref() == Some(name.as_str());
            view! {
                <li class="layer">
                    <span class="layer-order">{position + 1}</span>
                    <span class="layer-name">{name.clone()}</span>
                    <span class=format!("badge kind-{kind}")>{kind.clone()}</span>
                    {is_upload_target
                        .then(|| view! { <span class="badge uploads">"writes land here"</span> })}
                    {route.map(|route| view! { <code class="layer-route">{route}</code> })}
                </li>
            }
        })
        .collect_view();
    let usage = counters.map(|c| {
        view! {
            <p class="card-usage">
                <span>{c.pages}" listings"</span>
                <span>{c.reads}" reads"</span>
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
                {index.uploads.then(|| view! { <span class="badge uploads">"writes"</span> })}
            </div>
            <p class="dim"><code>{endpoint}</code></p>
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
    let endpoint = index.endpoint.clone();
    let usage = counters.map(|c| {
        view! {
            <p class="card-usage">
                <span>{c.pages}" listings"</span>
                <span>{c.reads}" reads"</span>
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
                {index.uploads.then(|| view! { <span class="badge uploads">"writes"</span> })}
            </div>
            <p class="dim"><code>{endpoint}</code></p>
            {usage}
        </div>
    }
}

#[cfg(test)]
#[path = "../../tests/unit/pages/dashboard/tests.rs"]
mod tests;
