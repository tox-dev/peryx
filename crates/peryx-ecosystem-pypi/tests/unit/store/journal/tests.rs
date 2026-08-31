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
                vec![br#"{"serial":0,"action":"add-file","project":"old","version":null,"filename":null}"#.to_vec()],
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

/// peryx writes its own changes to the same log, so the changelog reports the `PyPI` events around
/// them instead of failing on a payload that was never a `PyPI` entry.
#[test]
fn test_read_journal_entries_skips_a_core_entry() {
    let (_dir, store) = store();
    store
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![value("first")])))
        .unwrap();
    store
        .put_digest_revocation(
            &peryx_identity::ArtifactDigest::from_sha256(format!("{:064x}", 1)).unwrap(),
            &peryx_identity::RevocationReason::new("incident").unwrap(),
            &peryx_identity::UserId::random(),
            10,
        )
        .unwrap();
    store
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![value("second")])))
        .unwrap();

    assert_eq!(
        read_journal_entries(&store, 0, 10).unwrap(),
        JournalSnapshot {
            current_serial: 3,
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
                    serial: 3,
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
fn test_read_changelog_page_resumes_past_a_trailing_core_entry() {
    let (_dir, store) = store();
    store
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![value("first")])))
        .unwrap();
    store
        .put_digest_revocation(
            &peryx_identity::ArtifactDigest::from_sha256(format!("{:064x}", 2)).unwrap(),
            &peryx_identity::RevocationReason::new("incident").unwrap(),
            &peryx_identity::UserId::random(),
            10,
        )
        .unwrap();

    let page = read_changelog_page(&store, 1, 10).unwrap();

    assert_eq!((page.entries().len(), page.resume_serial()), (0, 2));
}
