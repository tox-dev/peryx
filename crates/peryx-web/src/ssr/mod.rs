//! The server half: an axum router that renders the app with data read straight from `AppState`,
//! plus the data builders the resource fetchers use during server rendering.

mod archive;
mod router;
mod search;
mod simple;
mod snapshot;

pub use archive::{member_chunk, members};
pub use router::{UiState, ui_router};
pub use search::search;
pub use simple::{layer_chunk, layer_members, manifest, project_view, projects};
pub use snapshot::{admin_snapshot, snapshot, stats};
