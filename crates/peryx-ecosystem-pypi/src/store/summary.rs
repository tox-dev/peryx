use std::collections::HashMap;

use peryx_driver::serving::{IndexSummary, RecentUpload};
use peryx_storage::meta::{MetaError, MetaStore};

use super::{PROJECTS_PREFIX, UPLOAD_PREFIX};

/// Summarize observed projects and uploads for configured indexes.
///
/// # Errors
/// Returns a store error if the read fails.
pub fn summarize_indexes(
    meta: &MetaStore,
    index_names: &[String],
    recent_limit: usize,
) -> Result<HashMap<String, IndexSummary>, MetaError> {
    let mut summaries: HashMap<String, IndexSummary> = index_names
        .iter()
        .map(|name| (name.clone(), IndexSummary::default()))
        .collect();
    let ordered = ordered_index_names(index_names);
    for key in meta.driver_prefix_keys(PROJECTS_PREFIX)? {
        let Some(logical) = key.strip_prefix(PROJECTS_PREFIX) else {
            continue;
        };
        if let Some(index) = matching_index(logical, &ordered)
            && let Some(summary) = summaries.get_mut(index)
        {
            summary.project_count += 1;
        }
    }
    for key in meta.driver_prefix_keys(UPLOAD_PREFIX)? {
        let Some(logical) = key.strip_prefix(UPLOAD_PREFIX) else {
            continue;
        };
        let Some((index, project, fallback_filename)) = upload_key_parts(logical, &ordered) else {
            continue;
        };
        if let Some(summary) = summaries.get_mut(index) {
            summary.upload_count += 1;
            if let Some(upload) = meta
                .get_driver_value(&key)?
                .and_then(|value| recent_upload(project, fallback_filename, &value))
            {
                push_recent(&mut summary.recent_uploads, upload, recent_limit);
            }
        }
    }
    Ok(summaries)
}

fn ordered_index_names(index_names: &[String]) -> Vec<&str> {
    let mut ordered: Vec<&str> = index_names.iter().map(String::as_str).collect();
    ordered.sort_by_key(|name| std::cmp::Reverse(name.len()));
    ordered
}

fn matching_index<'a>(key: &str, ordered: &'a [&str]) -> Option<&'a str> {
    ordered
        .iter()
        .copied()
        .find(|index| key.strip_prefix(index).is_some_and(|rest| rest.starts_with('/')))
}

fn upload_key_parts<'a>(key: &'a str, ordered: &'a [&str]) -> Option<(&'a str, &'a str, &'a str)> {
    let index = matching_index(key, ordered)?;
    let rest = key.strip_prefix(index)?.strip_prefix('/')?;
    let (project, filename) = rest.split_once('/')?;
    Some((index, project, filename))
}

fn recent_upload(project: &str, fallback_filename: &str, bytes: &[u8]) -> Option<RecentUpload> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    Some(RecentUpload {
        project: project.to_owned(),
        filename: value["file"]["filename"]
            .as_str()
            .unwrap_or(fallback_filename)
            .to_owned(),
        version: value["version"].as_str().unwrap_or_default().to_owned(),
        uploaded_at: value["file"]["upload-time"].as_str().map(str::to_owned),
        size: value["file"]["size"].as_u64(),
    })
}

fn push_recent(recent: &mut Vec<RecentUpload>, upload: RecentUpload, limit: usize) {
    if limit == 0 {
        return;
    }
    recent.push(upload);
    recent.sort_by(|left, right| {
        right
            .uploaded_at
            .cmp(&left.uploaded_at)
            .then_with(|| left.filename.cmp(&right.filename))
    });
    recent.truncate(limit);
}
