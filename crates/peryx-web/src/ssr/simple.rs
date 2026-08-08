use std::sync::Arc;

use leptos::prelude::*;
use peryx_core::{UiManifest, UiMember, UiMemberChunk, UiProjectView};
use peryx_driver::AppState;

use super::{authorize_project, resolve};

/// The project names of the index at `route`, produced by the index's ecosystem driver.
///
/// # Errors
/// Returns a user-visible message when the index is unknown, its ecosystem is not wired in, or its
/// project list cannot be read.
pub async fn projects(route: &str) -> Result<Vec<String>, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    let driver = driver
        .capabilities()
        .browse
        .ok_or_else(|| format!("index {route:?} does not support project browsing"))?;
    if app.index_at(position).acl.anonymous_read {
        return driver.project_names(&app.serving, position);
    }
    let access = super::read_access(&app).await?;
    let access = access.for_index(app.index_at(position));
    access.authorize_any_project().map_err(super::access_error)?;
    let mut names = driver.project_names(&app.serving, position)?;
    names.retain(|project| access.authorize_project(project).is_ok());
    Ok(names)
}

/// One project's browse view: a file listing with metadata or a list of references, produced by the
/// index's ecosystem driver so this crate carries no format-specific logic.
///
/// # Errors
/// Returns a user-visible message when the index is unknown or the project data cannot be read.
pub async fn project_view(route: &str, project: &str) -> Result<Option<UiProjectView>, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    authorize_project(&app, position, project).await?;
    let driver = driver
        .capabilities()
        .browse
        .ok_or_else(|| format!("index {route:?} does not support project browsing"))?;
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
    authorize_project(&app, position, repo).await?;
    let driver = driver
        .capabilities()
        .manifest
        .ok_or_else(|| format!("index {route:?} does not expose manifests"))?;
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
    authorize_project(&app, position, repo).await?;
    let driver = driver
        .capabilities()
        .artifact_members
        .ok_or_else(|| format!("index {route:?} does not expose artifact members"))?;
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
    authorize_project(&app, position, repo).await?;
    let driver = driver
        .capabilities()
        .artifact_members
        .ok_or_else(|| format!("index {route:?} does not expose artifact members"))?;
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
