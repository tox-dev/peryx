use std::sync::Arc;

use leptos::prelude::*;
use peryx_ecosystem_pypi::cache;
use peryx_ecosystem_pypi::{CoreMetadataDoc, normalize_name, parse_metadata, to_json};
use peryx_http::AppState;
use peryx_storage::blob::Digest;

use crate::model::UiProject;

/// The project names of the index at `route`.
///
/// # Errors
/// Returns a user-visible message when the index is unknown or its project list cannot be read.
pub fn projects(route: &str) -> Result<Vec<String>, String> {
    let app = expect_context::<Arc<AppState>>();
    let Some(index) = find_index(&app, route) else {
        return Err(format!("index {route:?} is not configured"));
    };
    cache::resolve_list(&app, index)
        .map(|list| list.projects.into_iter().map(|entry| entry.name).collect())
        .map_err(|err| format!("project list on index {route:?}: {}", err.user_message()))
}

/// One project's page data: files plus the parsed core metadata of its newest wheel with a PEP 658
/// sibling.
///
/// # Errors
/// Returns a user-visible message when project detail or metadata cannot be read.
pub async fn project(route: &str, project: &str) -> Result<Option<(UiProject, Option<CoreMetadataDoc>)>, String> {
    let app = expect_context::<Arc<AppState>>();
    let Some(index) = find_index(&app, route) else {
        return Err(format!("index {route:?} is not configured"));
    };
    let normalized = normalize_name(project);
    let Some(detail) = cache::resolve_detail(&app, index, &normalized, route)
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
    let value = serde_json::from_str(&to_json(&detail))
        .map_err(|err| format!("project detail on index {route:?} for project {normalized:?}: {err}"))?;
    let ui = UiProject::from_detail(&value);
    let doc = match ui.files.iter().rev().find(|file| file.has_metadata) {
        Some(file) => {
            let Some(digest) = Digest::from_hex(&file.sha256) else {
                return Err(format!(
                    "metadata fetch on index {route:?} for file {:?}: invalid sha256 digest {:?}",
                    file.filename, file.sha256
                ));
            };
            let metadata_filename = format!("{}.metadata", file.filename);
            let bytes = cache::metadata_bytes(&app, &digest, route, &metadata_filename)
                .await
                .map_err(|err| {
                    format!(
                        "metadata fetch on index {route:?} for file {:?} with digest {}: {}",
                        file.filename,
                        digest.as_str(),
                        err.user_message()
                    )
                })?;
            Some(parse_metadata(&String::from_utf8_lossy(&bytes)))
        }
        None => None,
    };
    Ok(Some((ui, doc)))
}

fn find_index<'a>(app: &'a AppState, route: &str) -> Option<&'a peryx_http::Index> {
    app.indexes.iter().find(|index| index.route == route)
}
