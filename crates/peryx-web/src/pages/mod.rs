//! The UI dashboard and browser.
use leptos::prelude::*;

use crate::model::{UiCounters, UiSnapshot, UiStats};

mod admin;
mod analytics;
mod browse;
mod dashboard;
mod login;
mod operations;
mod placement;
mod policy_decisions;
mod search;
mod stats;
mod topology;
mod trash;

pub use admin::AdminStatus;
pub use analytics::UsageAnalytics;
pub use browse::Browse;
pub use dashboard::Dashboard;
pub use login::Login;
pub use operations::PendingOperations;
pub use placement::ArtifactPlacements;
pub use policy_decisions::PolicyDecisions;
pub use search::Search;
pub use stats::Stats;
pub use topology::AvailabilityTopology;
pub use trash::Trash;

/// Refresh the dashboard counters every few seconds once hydrated. Effects never run during server
/// rendering, so this is inert in SSR output.
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn start_refresh(snapshot: Resource<UiSnapshot>) {
    use std::time::Duration;
    Effect::new(move |_| {
        set_interval(move || snapshot.refetch(), Duration::from_secs(5));
    });
}

#[cfg(any(feature = "ssr", not(feature = "hydrate")))]
const fn start_refresh(_snapshot: Resource<UiSnapshot>) {}

/// The per-ecosystem metric groups: one labelled block per ecosystem, so the reader can tell a
/// ecosystem-scoped counter from the global request count.
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
                        <div class="stat"><strong>{summary.reads}</strong><span>"artifacts served"</span></div>
                        <div class="stat"><strong>{summary.writes}</strong><span>"writes"</span></div>
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
    view! { <p class="error" role="alert">{message}</p> }
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn copy_to_clipboard(text: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().clipboard().write_text(text);
    }
}

/// Render a byte count with one decimal in the largest fitting unit.
fn human_size(bytes: u64) -> String {
    let mut divisor = 1;
    let mut unit = "B";
    for next_unit in ["kB", "MB", "GB", "TB"] {
        if bytes < divisor * 1024 {
            break;
        }
        divisor *= 1024;
        unit = next_unit;
    }
    let mut whole = bytes / divisor;
    let mut tenth = ((bytes % divisor) * 10 + divisor / 2) / divisor;
    if tenth == 10 {
        whole += 1;
        tenth = 0;
    }
    format!("{whole}.{tenth} {unit}")
}

#[cfg(test)]
#[path = "../../tests/unit/pages/tests.rs"]
mod tests;
