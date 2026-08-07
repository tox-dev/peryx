use serde::{Deserialize, Serialize};

use peryx_storage::meta::{MetaError, MetaStore};

use crate::{ChangelogEntry, ChangelogPage, ChangelogPageError};

/// One recorded mutation in the [`MetaStore`] journal: the append-only changelog that makes peryx
/// an origin others can replicate from. `serial` orders entries; the rest names what changed.
///
/// The neutral serial counter lives in the store, so a `PyPI` publish builds this entry with a
/// placeholder `serial` and lets [`commit_driver_txn`] allocate the authoritative one - see
/// [`publish_file_if`](super::publish_file_if).
///
/// [`MetaStore`]: peryx_storage::meta::MetaStore
/// [`commit_driver_txn`]: peryx_storage::meta::MetaStore::commit_driver_txn
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub serial: u64,
    #[serde(default)]
    pub submitted_at_unix: i64,
    pub action: String,
    pub project: String,
    pub version: Option<String>,
    pub filename: Option<String>,
}

/// Decoded `PyPI` journal entries and the head serial from one storage snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalSnapshot {
    pub current_serial: u64,
    pub entries: Vec<JournalEntry>,
}

/// Why a journal snapshot cannot become a Warehouse changelog page.
#[derive(Debug, thiserror::Error)]
pub enum ChangelogReadError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    InvalidPage(#[from] ChangelogPageError),
}

/// Read one journal snapshot and convert its records to Warehouse tuple values.
///
/// # Errors
/// Returns a storage or page-validation error when the snapshot cannot be served safely.
pub fn read_changelog_page(meta: &MetaStore, after: i64, limit: usize) -> Result<ChangelogPage, ChangelogReadError> {
    let snapshot = read_journal_entries(meta, u64::try_from(after).unwrap_or(0), limit)?;
    let entries = snapshot
        .entries
        .into_iter()
        .map(|entry| ChangelogEntry {
            project: entry.project,
            version: entry.version,
            timestamp: entry.submitted_at_unix,
            action: warehouse_action(&entry.action, entry.filename.as_deref()),
            serial: entry.serial,
        })
        .collect();
    Ok(ChangelogPage::new(after, snapshot.current_serial, entries)?)
}

/// Read a bounded journal snapshot and decode its opaque values as `PyPI` entries.
///
/// Storage owns the serial, so this replaces the serialized placeholder with the record key.
///
/// # Errors
/// Returns a store error if the snapshot cannot be read or an entry cannot be decoded.
pub fn read_journal_entries(meta: &MetaStore, after: u64, limit: usize) -> Result<JournalSnapshot, MetaError> {
    let (current_serial, records) = meta.journal_page_after(after, limit)?;
    let entries = records
        .into_iter()
        .map(|record| {
            let mut entry = serde_json::from_slice::<JournalEntry>(&record.payload)?;
            entry.serial = record.serial;
            Ok(entry)
        })
        .collect::<Result<_, serde_json::Error>>()?;
    Ok(JournalSnapshot {
        current_serial,
        entries,
    })
}

fn warehouse_action(action: &str, filename: Option<&str>) -> String {
    let action = match action {
        "add-file" | "promote" => "add file",
        "delete-file" => "remove file",
        action => action,
    };
    filename.map_or_else(|| action.to_owned(), |filename| format!("{action} {filename}"))
}

#[cfg(test)]
mod tests {
    use super::{ChangelogReadError, JournalEntry, JournalSnapshot, read_changelog_page, read_journal_entries};
    use peryx_storage::meta::{MetaError, MetaStore};

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, store)
    }

    fn value(project: &str) -> Vec<u8> {
        serde_json::to_vec(&JournalEntry {
            serial: 999,
            submitted_at_unix: 123,
            action: "add-file".to_owned(),
            project: project.to_owned(),
            version: Some("1.0".to_owned()),
            filename: Some(format!("{project}-1.0.whl")),
        })
        .unwrap()
    }

    #[test]
    fn test_read_journal_entries_uses_authoritative_serials() {
        let (_dir, store) = store();
        store
            .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![value("first"), value("second")])))
            .unwrap();

        assert_eq!(
            read_journal_entries(&store, 0, 10).unwrap(),
            JournalSnapshot {
                current_serial: 2,
                entries: vec![
                    JournalEntry {
                        serial: 1,
                        submitted_at_unix: 123,
                        action: "add-file".to_owned(),
                        project: "first".to_owned(),
                        version: Some("1.0".to_owned()),
                        filename: Some("first-1.0.whl".to_owned()),
                    },
                    JournalEntry {
                        serial: 2,
                        submitted_at_unix: 123,
                        action: "add-file".to_owned(),
                        project: "second".to_owned(),
                        version: Some("1.0".to_owned()),
                        filename: Some("second-1.0.whl".to_owned()),
                    },
                ],
            }
        );
    }

    #[test]
    fn test_read_journal_entries_passes_the_cursor_and_limit() {
        let (_dir, store) = store();
        store
            .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![value("first"), value("second"), value("third")])))
            .unwrap();

        let snapshot = read_journal_entries(&store, 1, 1).unwrap();
        assert_eq!(snapshot.current_serial, 3);
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].project, "second");
        assert_eq!(snapshot.entries[0].serial, 2);
    }

    #[test]
    fn test_read_journal_entries_rejects_an_invalid_value() {
        let (_dir, store) = store();
        store
            .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![b"{".to_vec()])))
            .unwrap();

        assert!(matches!(read_journal_entries(&store, 0, 10), Err(MetaError::Decode(_))));
    }

    #[test]
    fn test_read_journal_entries_defaults_an_older_timestamp() {
        let (_dir, store) = store();
        store
            .commit_driver_txn(|_| {
                Ok::<_, MetaError>((
                    (),
                    vec![
                        br#"{"serial":0,"action":"add-file","project":"old","version":null,"filename":null}"#.to_vec(),
                    ],
                ))
            })
            .unwrap();

        assert_eq!(
            read_journal_entries(&store, 0, 1).unwrap().entries[0].submitted_at_unix,
            0
        );
    }

    #[test]
    fn test_read_changelog_page_maps_actions_and_preserves_the_snapshot() {
        let (_dir, store) = store();
        let values = [
            ("add-file", Some("first-1.0.whl")),
            ("delete-file", Some("first-1.0.whl")),
            ("yank", Some("first-1.0.whl")),
            ("unyank", Some("first-1.0.whl")),
            ("hide", Some("first-1.0.whl")),
            ("restore", Some("first-1.0.whl")),
            ("promote", None),
        ]
        .map(|(action, filename)| {
            serde_json::to_vec(&JournalEntry {
                serial: 0,
                submitted_at_unix: 123,
                action: action.to_owned(),
                project: "first".to_owned(),
                version: Some("1.0".to_owned()),
                filename: filename.map(str::to_owned),
            })
            .unwrap()
        });
        store
            .commit_driver_txn(|_| Ok::<_, MetaError>(((), values.into())))
            .unwrap();

        let page = read_changelog_page(&store, -1, 7).unwrap();

        assert_eq!(page.current_serial(), 7);
        assert_eq!(
            page.entries()
                .iter()
                .map(|entry| entry.action.as_str())
                .collect::<Vec<_>>(),
            [
                "add file first-1.0.whl",
                "remove file first-1.0.whl",
                "yank first-1.0.whl",
                "unyank first-1.0.whl",
                "hide first-1.0.whl",
                "restore first-1.0.whl",
                "add file",
            ]
        );
    }

    #[test]
    fn test_read_changelog_page_keeps_storage_errors_typed() {
        let (_dir, store) = store();
        store
            .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![b"{".to_vec()])))
            .unwrap();

        let error = read_changelog_page(&store, 0, 1).unwrap_err();

        assert!(matches!(error, ChangelogReadError::Store(MetaError::Decode(_))));
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn test_changelog_read_error_keeps_page_validation_typed() {
        let error = ChangelogReadError::from(crate::ChangelogPageError::TooLarge { actual: 50_001 });

        assert!(matches!(error, ChangelogReadError::InvalidPage(_)));
        assert!(error.to_string().contains("50001"));
    }
}
