use std::sync::Arc;

use leptos::prelude::*;
use peryx_driver::AppState;
use peryx_driver::serving::{ArchiveMemberRequest, ArchiveRequest};

use super::{authorize_project, resolve};
use crate::model::{UiMember, UiMemberChunk};

/// The member listing of a cached archive, for server rendering.
///
/// # Errors
/// Returns a user-visible message when the project cannot be read, or the artifact cannot be found,
/// fetched, or listed.
pub async fn members(
    route: &str,
    project: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
) -> Result<Vec<UiMember>, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    authorize_project(&app, position, project).await?;
    let driver = driver
        .capabilities()
        .archive
        .ok_or_else(|| format!("index {route:?} does not expose archives"))?;
    driver
        .archive_members(ArchiveRequest {
            state: app.serving.clone(),
            position,
            project: project.to_owned(),
            digest: sha256.to_owned(),
            filename: filename.to_owned(),
            containers: containers.to_vec(),
        })
        .await
}

/// One archive member chunk, for server rendering.
///
/// # Errors
/// Returns a user-visible message when the project cannot be read, or the member cannot be previewed
/// as UTF-8 text.
pub async fn member_chunk(
    route: &str,
    project: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
    member: &str,
    offset: u64,
) -> Result<UiMemberChunk, String> {
    let app = expect_context::<Arc<AppState>>();
    let (position, driver) = resolve(&app, route)?;
    authorize_project(&app, position, project).await?;
    let driver = driver
        .capabilities()
        .archive
        .ok_or_else(|| format!("index {route:?} does not expose archives"))?;
    driver
        .archive_member_chunk(ArchiveMemberRequest {
            archive: ArchiveRequest {
                state: app.serving.clone(),
                position,
                project: project.to_owned(),
                digest: sha256.to_owned(),
                filename: filename.to_owned(),
                containers: containers.to_vec(),
            },
            member: member.to_owned(),
            offset,
        })
        .await
}
