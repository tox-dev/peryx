use std::sync::Arc;

use leptos::prelude::*;
use peryx_core::{UiMeta, UiProject};
use peryx_driver::AppState;

/// The project names of the index at `route`, produced by the index's ecosystem driver.
///
/// # Errors
/// Returns a user-visible message when the index is unknown, its ecosystem is not wired in, or its
/// project list cannot be read.
pub fn projects(route: &str) -> Result<Vec<String>, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    driver.project_names(&app, position)
}

/// One project's page: its files and neutral metadata, produced by the index's ecosystem driver so
/// this crate carries no format-specific logic.
///
/// # Errors
/// Returns a user-visible message when the index is unknown or the project data cannot be read.
pub async fn project(route: &str, project: &str) -> Result<Option<(UiProject, UiMeta)>, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    driver.project_page(app.clone(), position, project.to_owned()).await
}

/// The position of the index at `route` and the driver serving its ecosystem.
fn resolve<'a>(
    app: &'a AppState,
    route: &str,
) -> Result<(usize, &'a Arc<dyn peryx_driver::serving::EcosystemDriver>), String> {
    let position = app
        .indexes
        .iter()
        .position(|index| index.route == route)
        .ok_or_else(|| format!("index {route:?} is not configured"))?;
    let driver = app
        .driver_for(app.index_at(position).ecosystem)
        .ok_or_else(|| format!("index {route:?} has no ecosystem driver"))?;
    Ok((position, driver))
}
