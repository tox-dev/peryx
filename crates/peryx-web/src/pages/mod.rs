//! The UI pages: a live dashboard and a pypi.org-style package browser.
#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]
#![allow(
    clippy::missing_const_for_fn,
    reason = "cfg-split helpers are const only without the hydrate feature; constness cannot vary by cfg"
)]

use leptos::prelude::*;

use crate::model::{UiCounters, UiSnapshot, UiStats};

mod admin;
mod archive;
mod browse;
mod dashboard;
mod manifest;
mod project;
mod search;
mod stats;

pub use admin::AdminStatus;
pub use browse::Browse;
pub use dashboard::Dashboard;
pub use search::Search;
pub use stats::Stats;

/// Refresh the dashboard counters every few seconds once hydrated. Effects never run during server
/// rendering, so this is inert in SSR output.
fn start_refresh(snapshot: Resource<UiSnapshot>) {
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        use std::time::Duration;
        Effect::new(move |_| {
            set_interval(move || snapshot.refetch(), Duration::from_secs(5));
        });
    }
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    {
        let _ = snapshot;
    }
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

fn optional_counters_for(usage: &UiStats, route: &str) -> Option<UiCounters> {
    usage
        .rows
        .iter()
        .find(|(candidate, _)| candidate == route)
        .map(|(_, counters)| *counters)
}

#[component]
fn ErrorMessage(message: String) -> impl IntoView {
    view! { <p class="error">{message}</p> }
}

fn copy_to_clipboard(text: &str) {
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        if let Some(window) = web_sys::window() {
            let _ = window.navigator().clipboard().write_text(text);
        }
    }
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    {
        let _ = text;
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
