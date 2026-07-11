use std::collections::HashMap;

use peryx_driver::serving::{IndexSummary, RecentUpload};
use peryx_storage::meta::{MetaError, MetaStore};

use super::{PROJECTS_PREFIX, UPLOAD_PREFIX};

/// Summarize observed projects and uploads for configured indexes.
///
/// # Errors
/// Returns a store error if the read fails.
///
/// # Panics
/// Never in practice: `upload_key_parts` yields one of the configured index names.
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
        let logical = &key[PROJECTS_PREFIX.len()..];
        if let Some(index) = matching_index(logical, &ordered)
            && let Some(summary) = summaries.get_mut(index)
        {
            summary.project_count += 1;
        }
    }
    for key in meta.driver_prefix_keys(UPLOAD_PREFIX)? {
        let Some((index, project, fallback_filename)) = upload_key_parts(&key[UPLOAD_PREFIX.len()..], &ordered) else {
            continue;
        };
        let summary = summaries
            .get_mut(index)
            .expect("upload_key_parts yields one of the configured index names");
        summary.upload_count += 1;
        if let Some(upload) = meta
            .get_driver_value(&key)?
            .and_then(|value| recent_upload(project, fallback_filename, &value))
        {
            push_recent(&mut summary.recent_uploads, upload, recent_limit);
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

#[cfg(test)]
mod tests {
    use super::{MetaStore, UPLOAD_PREFIX};
    use crate::store::PypiStore as _;

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    fn upload(meta: &MetaStore, index: &str, project: &str, filename: &str, version: &str, at: &str, size: u64) {
        let record = format!(
            r#"{{"version":"{version}","file":{{"filename":"{filename}","upload-time":"{at}","size":{size}}}}}"#,
        );
        meta.put_upload(index, project, filename, record.as_bytes()).unwrap();
    }

    #[test]
    fn test_summarize_indexes_counts_projects_and_orders_recent_uploads() {
        let (_dir, meta) = store();
        meta.put_project("hosted", "flask", "Flask").unwrap();
        meta.put_project("root/hosted", "django", "Django").unwrap();
        upload(
            &meta,
            "hosted",
            "flask",
            "flask-1.0.whl",
            "1.0",
            "2026-01-01T00:00:00Z",
            10,
        );
        upload(
            &meta,
            "root/hosted",
            "django",
            "django-4.0.whl",
            "4.0",
            "2026-02-01T00:00:00Z",
            20,
        );
        upload(
            &meta,
            "root/hosted",
            "django",
            "django-3.2.whl",
            "3.2",
            "2025-12-01T00:00:00Z",
            15,
        );
        // An upload on an index the caller did not ask about is ignored.
        upload(
            &meta,
            "foreign",
            "flask",
            "ignored.whl",
            "1.0",
            "2026-03-01T00:00:00Z",
            5,
        );

        let indexes = vec!["hosted".to_owned(), "root/hosted".to_owned()];
        let summary = meta.summarize_indexes(&indexes, 5).unwrap();

        assert_eq!(summary["hosted"].project_count, 1);
        assert_eq!(summary["hosted"].upload_count, 1);
        assert_eq!(summary["root/hosted"].project_count, 1);
        assert_eq!(summary["root/hosted"].upload_count, 2);
        // Newest upload-time first.
        let recent: Vec<&str> = summary["root/hosted"]
            .recent_uploads
            .iter()
            .map(|upload| upload.filename.as_str())
            .collect();
        assert_eq!(recent, vec!["django-4.0.whl", "django-3.2.whl"]);
    }

    #[test]
    fn test_summarize_indexes_breaks_an_upload_time_tie_by_filename() {
        let (_dir, meta) = store();
        // Same upload-time on both, so the sort falls through to the filename tiebreak.
        upload(
            &meta,
            "hosted",
            "flask",
            "flask-2.0.whl",
            "2.0",
            "2026-01-01T00:00:00Z",
            10,
        );
        upload(
            &meta,
            "hosted",
            "flask",
            "flask-1.0.whl",
            "1.0",
            "2026-01-01T00:00:00Z",
            10,
        );
        let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
        let recent: Vec<&str> = summary["hosted"]
            .recent_uploads
            .iter()
            .map(|upload| upload.filename.as_str())
            .collect();
        assert_eq!(recent, vec!["flask-1.0.whl", "flask-2.0.whl"]);
    }

    #[test]
    fn test_summarize_indexes_truncates_recent_to_the_limit() {
        let (_dir, meta) = store();
        upload(
            &meta,
            "hosted",
            "flask",
            "flask-2.0.whl",
            "2.0",
            "2026-02-01T00:00:00Z",
            10,
        );
        upload(
            &meta,
            "hosted",
            "flask",
            "flask-1.0.whl",
            "1.0",
            "2026-01-01T00:00:00Z",
            10,
        );
        let summary = meta.summarize_indexes(&["hosted".to_owned()], 1).unwrap();
        assert_eq!(summary["hosted"].recent_uploads.len(), 1);
        assert_eq!(summary["hosted"].recent_uploads[0].filename, "flask-2.0.whl");
    }

    #[test]
    fn test_summarize_indexes_with_a_zero_limit_counts_but_keeps_no_recent() {
        let (_dir, meta) = store();
        upload(
            &meta,
            "hosted",
            "flask",
            "flask-1.0.whl",
            "1.0",
            "2026-01-01T00:00:00Z",
            10,
        );
        let summary = meta.summarize_indexes(&["hosted".to_owned()], 0).unwrap();
        assert_eq!(summary["hosted"].upload_count, 1);
        assert!(summary["hosted"].recent_uploads.is_empty());
    }

    #[test]
    fn test_summarize_indexes_counts_an_unparsable_upload_without_a_recent_entry() {
        let (_dir, meta) = store();
        // A stored upload whose body is not valid JSON still counts, but contributes no recent entry.
        meta.put_upload("hosted", "flask", "flask-1.0.whl", b"not json")
            .unwrap();
        let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
        assert_eq!(summary["hosted"].upload_count, 1);
        assert!(summary["hosted"].recent_uploads.is_empty());
    }

    #[test]
    fn test_summarize_indexes_skips_a_malformed_upload_key() {
        let (_dir, meta) = store();
        // A row whose key carries no project/filename split is skipped rather than counted.
        meta.put_driver_value(&format!("{UPLOAD_PREFIX}hosted/onlyproject"), b"{}")
            .unwrap();
        let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
        assert_eq!(summary["hosted"].upload_count, 0);
    }
}
