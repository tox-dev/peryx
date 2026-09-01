//! The UI dashboard and browser.
use leptos::prelude::*;

use crate::data::LoaderError;
use crate::model::{UiCounters, UiSnapshot, UiStats};

#[cfg(feature = "ssr")]
fn reactive_value<S: GetUntracked>(signal: &S) -> S::Value {
    signal.get_untracked()
}

#[cfg(not(feature = "ssr"))]
fn reactive_value<S: Get>(signal: &S) -> S::Value {
    signal.get()
}

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

/// Refresh browser data every five seconds after hydration.
///
/// A tick that lands while the previous load is still outstanding is skipped: refetching then
/// would stack a second request on a slow endpoint and let a later generation overtake an earlier
/// one.
///
/// The interval outlives the page that started it, so a tick can arrive after the route has been
/// left and the resource disposed. Reading a disposed resource panics, and there is nothing left
/// to refresh, so a disposed resource skips the tick as well.
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn start_refresh<T>(resource: Resource<Result<T, LoaderError>>)
where
    T: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    use std::time::Duration;

    use futures_util::FutureExt as _;

    Effect::new(move |_| {
        set_interval(
            move || {
                if !resource.is_disposed() && resource.ready().now_or_never().is_some() {
                    resource.refetch();
                }
            },
            Duration::from_secs(5),
        );
    });
}

#[cfg(any(feature = "ssr", not(feature = "hydrate")))]
const fn start_refresh<T>(_resource: Resource<Result<T, LoaderError>>)
where
    T: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
}

#[derive(Clone)]
struct LoadState<T> {
    value: Option<T>,
    error: Option<String>,
}

impl<T> Default for LoadState<T> {
    fn default() -> Self {
        Self {
            value: None,
            error: None,
        }
    }
}

fn retain<T: Clone + Send + Sync + 'static>(
    state: RwSignal<LoadState<T>>,
    result: Result<T, LoaderError>,
) -> LoadState<T> {
    state.update(|state| match result {
        Ok(value) => {
            state.value = Some(value);
            state.error = None;
        }
        Err(error) => state.error = Some(error.to_string()),
    });
    state.get_untracked()
}

/// The counters to render from the usage half of a published pair, and the message to report when
/// that half did not answer. The counters are dropped rather than carried over, so a page never
/// prints usage measured in a refresh other than the snapshot beside it.
fn usage_or_error(usage: Result<UiStats, LoaderError>) -> (Option<UiStats>, Option<String>) {
    match usage {
        Ok(usage) => (Some(usage), None),
        Err(error) => (None, Some(error.to_string())),
    }
}

/// A serial the caller may not see, or that the metadata store could not report, prints as absent
/// rather than as a change serial of zero.
fn serial_stat(serial: Option<u64>) -> String {
    serial.map_or_else(|| "n/a".to_owned(), |serial| serial.to_string())
}

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
                .filter(|family| family.ecosystem == summary.ecosystem)
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
