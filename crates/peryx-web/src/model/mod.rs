//! Serializable view models shared by the server renderer and the hydrated client.
//!
//! The server builds them from `AppState`; the browser rebuilds them from peryx's own JSON API
//! (`/+status` and the PEP 691 simple endpoints), so both sides render identical pages.

mod oci;
mod project;
mod search;
mod snapshot;
mod stats;

pub use oci::{UiOciBlob, UiOciManifest};
pub use project::{UiFile, UiMember, UiMemberChunk, UiProject, members_from_listing, projects_from_list};
pub use search::{UiSearchPage, UiSearchResult, source_label};
pub use snapshot::{UiEcosystemSummary, UiHosted, UiIndex, UiMetricFamily, UiRecentUpload, UiSnapshot, UiUpstream};
pub use stats::{UiCounters, UiStats, stats_index, stats_project, stats_routes};

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

fn u64_at(value: &serde_json::Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or_default()
}

fn usize_from(value: Option<u64>, default: usize) -> usize {
    value.and_then(|value| usize::try_from(value).ok()).unwrap_or(default)
}
