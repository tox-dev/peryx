//! Server-rendered routes and data builders.

mod browse;
mod login;
mod operations;
mod placement;
mod router;
mod search;
mod snapshot;
mod topology;

pub use browse::browse;
pub use login::login_state;
pub use operations::operations;
pub use placement::{blob_placements, placements};
pub use router::{UiState, ui_router};
pub use search::search;
pub use snapshot::{admin_snapshot, snapshot, stats};
pub use topology::topology;

use std::sync::Arc;

use peryx_driver::{AppState, ServingState};

async fn read_access(state: &ServingState) -> Result<peryx_driver::access::ReadAccess, String> {
    let headers = match leptos_axum::extract::<axum::http::HeaderMap>().await {
        Ok(headers) => headers,
        Err(error) => return Err(format!("request headers: {error}")),
    };
    Ok(peryx_driver::access::ReadAccess::from_headers(state, &headers))
}

/// The position of the index at `route` and the driver serving its ecosystem.
fn resolve<'a>(
    app: &'a AppState,
    route: &str,
) -> Result<(usize, &'a Arc<dyn peryx_driver::serving::EcosystemDriver>), String> {
    let mut position = None;
    for (candidate, index) in app.serving.indexes.iter().enumerate() {
        if index.route == route {
            position = Some(candidate);
        }
    }
    let Some(position) = position else {
        return Err(format!("index {route:?} is not configured"));
    };
    let Some(driver) = app.driver_for(&app.serving.index_at(position).ecosystem) else {
        return Err(format!("index {route:?} has no ecosystem driver"));
    };
    Ok((position, driver))
}

#[cfg(test)]
#[path = "../../tests/unit/ssr/tests.rs"]
mod tests;
