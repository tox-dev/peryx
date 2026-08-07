//! Serializable view models shared by the server renderer and the hydrated client.
//!
//! The server builds them from `AppState`; the browser rebuilds them from peryx's own JSON API
//! (`/+status` and ecosystem endpoints), so both sides render identical pages.

mod analytics;
mod login;
mod operations;
mod placement;
mod policy_decision;
mod project;
mod search;
mod shadow;
mod snapshot;
mod stats;
mod topology;
mod trash;

pub use analytics::{
    AnalyticsFilters, AnalyticsView, UiInterval, UiPackageRow, UiSourceRow, UiTimelineRow, UiUnusedRow, UiUsagePage,
    UiUsageRows, UiVersionRow, format_instant,
};
pub use login::UiLoginState;
pub use operations::{OperationRow, OperationsHealth, OperationsView, UiOperationStatus, operation_status_label};
pub use placement::{
    BlobDatacenterPlacement, BlobPlacementStatus, BlobPlacementView, PlacementHealth, PlacementRow, PlacementView,
    blob_placement_status_label,
};
pub use policy_decision::{PolicyDecisionFilters, UiPolicyDecision, UiPolicyDecisionPage};
pub use project::{
    PlacementLabel, UiArtifactRef, UiArtifactSource, UiAttestation, UiByteAvailability, UiFile, UiManifest, UiMember,
    UiMemberChunk, UiProject, UiProjectStatus, UiProjectView, UiProvenance, UiProvenanceSource, UiRelease,
    UiSubjectMatch, byte_availability_label, file_source_label, members_from_listing, projects_from_list,
    provenance_source_label, provenance_validation_label, subject_match_label,
};
pub use search::{UiSearchPage, UiSearchResult, source_label};
pub use shadow::{ShadowInspectionFilters, UiShadowCandidate, UiShadowDecision, UiShadowPage};
pub use snapshot::{UiEcosystemSummary, UiHosted, UiIndex, UiMetricFamily, UiRecentUpload, UiSnapshot, UiUpstream};
pub use stats::{UiCounters, UiStats, stats_index, stats_project, stats_routes};
pub use topology::{
    HealthLabel, LocalNode, RoleFilter, StreamStatus, TopologyNode, TopologySnapshot, liveness_health, mode_label,
    role_label, stream_status_label,
};
pub use trash::{TrashFilters, UiTrashPage, UiTrashRecord};

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

fn u64_at(value: &serde_json::Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or_default()
}
