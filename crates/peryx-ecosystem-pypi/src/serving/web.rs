//! Producing the web UI's neutral view models from the `PyPI` serving layer, so the web crate renders
//! a project page without any knowledge of the Simple API, wheels, or PEP 658.

use std::sync::Arc;

use peryx_core::{UiMeta, UiProject};
use peryx_driver::ServingState;
use peryx_storage::blob::Digest;

use crate::cache;
use crate::{normalize_name, to_json, ui_meta, ui_project_from_detail};

/// The project names of the cached/hosted/virtual index at `position`.
pub(super) fn project_names(state: &ServingState, position: usize) -> Result<Vec<String>, String> {
    let list = cache::resolve_list(state, state.index_at(position))?;
    Ok(list.projects.into_iter().map(|entry| entry.name).collect())
}

/// A project's page data: its files as a neutral [`UiProject`], and the neutral [`UiMeta`] of its
/// newest file that carries a PEP 658 metadata sibling.
pub(super) async fn project_page(
    state: Arc<ServingState>,
    position: usize,
    project: String,
) -> Result<Option<(UiProject, UiMeta)>, String> {
    let route = state.index_at(position).route.clone();
    let normalized = normalize_name(&project);
    let index = state.index_at(position);
    let Some(detail) = cache::resolve_detail(&state, index, &normalized, &route)
        .await
        .map_err(|err| {
            format!(
                "project detail on index {route:?} for project {normalized:?}: {}",
                err.user_message()
            )
        })?
    else {
        return Ok(None);
    };
    // `to_json` serializes the detail, so parsing it straight back cannot fail.
    let value = serde_json::from_str(&to_json(&detail)).expect("to_json emits JSON that round-trips");
    let ui = ui_project_from_detail(&value);
    let mut meta = match ui.files.iter().rev().find(|file| file.has_metadata) {
        Some(file) => metadata_for(&state, &route, file).await?,
        None => UiMeta::default(),
    };
    meta.version = latest_version(&ui.versions).or(meta.version);
    Ok(Some((ui, meta)))
}

fn latest_version(versions: &[String]) -> Option<String> {
    versions
        .iter()
        .filter_map(|version| crate::parse_version(version).map(|parsed| (parsed, version)))
        .max_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)))
        .map(|(_, version)| version)
        .or_else(|| versions.iter().max())
        .cloned()
}

/// Fetch and parse the PEP 658 metadata sibling of `file` into the neutral view model.
async fn metadata_for(state: &Arc<ServingState>, route: &str, file: &peryx_core::UiFile) -> Result<UiMeta, String> {
    let Some(digest) = Digest::from_hex(&file.sha256) else {
        return Err(format!(
            "metadata fetch on index {route:?} for file {:?}: invalid sha256 digest {:?}",
            file.filename, file.sha256
        ));
    };
    let metadata_filename = format!("{}.metadata", file.filename);
    let bytes = cache::metadata_bytes(state, &digest, route, &metadata_filename)
        .await
        .map_err(|err| {
            format!(
                "metadata fetch on index {route:?} for file {:?} with digest {}: {}",
                file.filename,
                digest.as_str(),
                err.user_message()
            )
        })?;
    Ok(ui_meta(&String::from_utf8_lossy(&bytes)))
}

/// The local blob-store path of the artifact `digest_hex`/`filename` on the index at `position`,
/// fetching it through the proxy on a miss.
pub(super) async fn artifact_path(
    state: Arc<ServingState>,
    position: usize,
    digest_hex: String,
    filename: String,
) -> Result<std::path::PathBuf, String> {
    let route = state.index_at(position).route.clone();
    let Some(digest) = Digest::from_hex(&digest_hex) else {
        return Err(format!(
            "artifact on index {route:?} for file {filename:?}: invalid sha256 digest {digest_hex:?}"
        ));
    };
    cache::file_path(state, digest, route.clone(), filename.clone())
        .await
        .map_err(|err| {
            format!(
                "artifact on index {route:?} for file {filename:?} with digest {digest_hex}: {}",
                err.user_message()
            )
        })
}
