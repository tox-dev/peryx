use std::collections::HashMap;

use redb::{ReadableDatabase as _, ReadableTable as _};

use super::error::MetaError;
use super::{MetaStore, PROJECTS, UPLOAD};

/// Per-index package and upload counts for read-only status pages.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexSummary {
    pub project_count: u64,
    pub upload_count: u64,
    pub recent_uploads: Vec<RecentUpload>,
}

/// One uploaded file summary with token-free metadata only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentUpload {
    pub project: String,
    pub filename: String,
    pub version: String,
    pub uploaded_at: Option<String>,
    pub size: Option<u64>,
}

impl MetaStore {
    /// Summarize observed projects and uploads for configured indexes in one read transaction.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn summarize_indexes(
        &self,
        index_names: &[String],
        recent_limit: usize,
    ) -> Result<HashMap<String, IndexSummary>, MetaError> {
        let mut summaries: HashMap<String, IndexSummary> = index_names
            .iter()
            .map(|name| (name.clone(), IndexSummary::default()))
            .collect();
        let txn = self.db.begin_read()?;
        let projects = txn.open_table(PROJECTS)?;
        let ordered = ordered_index_names(index_names);
        for entry in projects.iter()? {
            let (key, _) = entry?;
            if let Some(index) = matching_index(key.value(), &ordered)
                && let Some(summary) = summaries.get_mut(index)
            {
                summary.project_count += 1;
            }
        }
        let uploads = txn.open_table(UPLOAD)?;
        for entry in uploads.iter()? {
            let (key, value) = entry?;
            if let Some((index, project, fallback_filename)) = upload_key_parts(key.value(), &ordered)
                && let Some(summary) = summaries.get_mut(index)
            {
                summary.upload_count += 1;
                if let Some(upload) = recent_upload(project, fallback_filename, value.value()) {
                    push_recent(&mut summary.recent_uploads, upload, recent_limit);
                }
            }
        }
        Ok(summaries)
    }
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
