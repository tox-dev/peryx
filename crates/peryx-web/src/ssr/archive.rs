use std::sync::Arc;

use leptos::prelude::*;
use peryx_driver::AppState;
use peryx_storage::archive;

use crate::model::{UiMember, UiMemberChunk};

/// The member listing of a cached archive, for server rendering.
///
/// # Errors
/// Returns a user-visible message when the artifact cannot be found, fetched, or listed.
pub async fn members(
    route: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
) -> Result<Vec<UiMember>, String> {
    let path = artifact_path(route, sha256, filename).await?;
    let archive = filename.to_owned();
    let containers = containers.to_vec();
    let members = tokio::task::spawn_blocking(move || archive::list_members_nested_path(&archive, &path, &containers))
        .await
        .map_err(|err| format!("archive listing on index {route:?} for file {filename:?}: {err}"))?
        .map_err(|err| format!("archive listing on index {route:?} for file {filename:?}: {err}"))?;
    Ok(members
        .into_iter()
        .map(|member| UiMember {
            path: member.path,
            size: member.size,
            kind: member.kind.as_str().to_owned(),
            previewable: member.previewable,
        })
        .collect())
}

/// One archive member chunk, for server rendering.
///
/// # Errors
/// Returns a user-visible message when the member cannot be previewed as UTF-8 text.
pub async fn member_chunk(
    route: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
    member: &str,
    offset: u64,
) -> Result<UiMemberChunk, String> {
    let path = artifact_path(route, sha256, filename).await?;
    let archive = filename.to_owned();
    let containers = containers.to_vec();
    let selected = member.to_owned();
    let chunk = tokio::task::spawn_blocking(move || {
        archive::read_text_member_chunk_nested_path(
            &archive,
            &path,
            &containers,
            &selected,
            offset,
            archive::DEFAULT_MEMBER_CHUNK,
        )
    })
    .await
    .map_err(|err| format!("archive member {member:?} on index {route:?} for file {filename:?}: {err}"))?
    .map_err(|err| format!("archive member {member:?} on index {route:?} for file {filename:?}: {err}"))?;
    Ok(UiMemberChunk {
        text: String::from_utf8(chunk.bytes).map_err(|err| {
            format!("archive member {member:?} on index {route:?} for file {filename:?} is not valid UTF-8: {err}")
        })?,
        size: Some(chunk.size),
        offset: chunk.offset,
        next_offset: chunk.next_offset,
    })
}

/// The local path of the artifact `sha256`/`filename` on the index at `route`, fetched through that
/// index's ecosystem driver so this crate carries no format-specific fetch logic.
async fn artifact_path(route: &str, sha256: &str, filename: &str) -> Result<std::path::PathBuf, String> {
    let app = expect_context::<Arc<AppState>>();
    let position = app
        .indexes
        .iter()
        .position(|index| index.route == route)
        .ok_or_else(|| format!("index {route:?} is not configured"))?;
    let driver = app
        .driver_for(app.index_at(position).ecosystem)
        .ok_or_else(|| format!("index {route:?} has no ecosystem driver"))?
        .clone();
    driver
        .artifact_path(app.serving.clone(), position, sha256.to_owned(), filename.to_owned())
        .await
}
