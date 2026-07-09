//! The server half: an axum router that renders the app with data read straight from `AppState`,
//! plus the data builders the resource fetchers use during server rendering.

mod archive;
mod oci;
mod router;
mod search;
mod simple;
mod snapshot;

pub use archive::{member_chunk, members};
pub use oci::{oci_layer_chunk, oci_layer_members, oci_manifest, oci_tags};
pub use router::{UiState, ui_router};
pub use search::{repositories, search};
pub use simple::{project, projects};
pub use snapshot::{admin_snapshot, snapshot, stats};
