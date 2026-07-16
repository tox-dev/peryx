use serde::{Deserialize, Serialize};

use peryx_storage::meta::{MetaError, MetaStore};

/// One recorded mutation in the [`MetaStore`] journal: the append-only changelog that makes peryx
/// an origin others can replicate from. `serial` orders entries; the rest names what changed.
///
/// The neutral serial counter lives in the store, so a `PyPI` publish builds this entry with a
/// placeholder `serial` and lets [`commit_driver_txn`] allocate the authoritative one — see
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

#[cfg(test)]
mod tests {
    use super::{JournalEntry, JournalSnapshot, read_journal_entries};
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
}
