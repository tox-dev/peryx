//! Data loading for the UI, compiled per side: the server reads `AppState` directly, the hydrated
//! browser fetches peryx's own JSON API. Both produce the same view models.

mod browse;
mod login;
mod operations;
mod placement;
mod search;
mod stats;
mod status;
mod topology;

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub use browse::admin_request;
pub use browse::load_browse;
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
use browse::{fetch_json, fetch_json_optional, fetch_json_required};
pub use login::load_login;
pub use operations::load_operations;
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub use operations::load_policy_decisions;
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub use placement::load_trash;
pub use placement::{load_blob_placement, load_placements};
pub use search::load_search;
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub use stats::load_analytics;
pub use stats::load_stats;
pub use status::{load_admin_snapshot, load_snapshot};
pub use topology::load_topology;
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub use topology::{TopologyStream, subscribe_topology};
