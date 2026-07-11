use std::sync::Arc;

use leptos::prelude::*;
use peryx_core::{UiManifest, UiMember, UiMemberChunk, UiProjectView};
use peryx_driver::AppState;

/// The project names of the index at `route`, produced by the index's ecosystem driver.
///
/// # Errors
/// Returns a user-visible message when the index is unknown, its ecosystem is not wired in, or its
/// project list cannot be read.
pub fn projects(route: &str) -> Result<Vec<String>, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    driver.project_names(&app.serving, position)
}

/// One project's browse view: a file listing with metadata or a list of references, produced by the
/// index's ecosystem driver so this crate carries no format-specific logic.
///
/// # Errors
/// Returns a user-visible message when the index is unknown or the project data cannot be read.
pub async fn project_view(route: &str, project: &str) -> Result<Option<UiProjectView>, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    driver
        .browse_project(app.serving.clone(), position, project.to_owned())
        .await
}

/// One reference's manifest view under a repository, produced by the index's ecosystem driver.
///
/// # Errors
/// Returns a user-visible message when the index is unknown or the manifest cannot be read.
pub async fn manifest(route: &str, repo: &str, reference: &str) -> Result<Option<UiManifest>, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    driver
        .manifest_view(app.serving.clone(), position, repo.to_owned(), reference.to_owned())
        .await
}

/// The member listing of one stored layer, produced by the index's ecosystem driver.
///
/// # Errors
/// Returns a user-visible message when the index is unknown or the layer cannot be listed.
pub async fn layer_members(route: &str, repo: &str, digest: &str) -> Result<Vec<UiMember>, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    driver
        .artifact_members(app.serving.clone(), position, repo.to_owned(), digest.to_owned())
        .await
}

/// One text member chunk of a stored layer, produced by the index's ecosystem driver.
///
/// # Errors
/// Returns a user-visible message when the index is unknown or the member cannot be read.
pub async fn layer_chunk(
    route: &str,
    repo: &str,
    digest: &str,
    member: &str,
    offset: u64,
) -> Result<UiMemberChunk, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    driver
        .artifact_member_chunk(
            app.serving.clone(),
            position,
            repo.to_owned(),
            digest.to_owned(),
            member.to_owned(),
            offset,
        )
        .await
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
