use peryx_ecosystem_oci::OciMutation;
use peryx_storage::meta::{MetaError, MetaStore};
use rstest::rstest;

use super::{
    ChangelogReadError, JournalEntry, JournalSnapshot, PYPI_OP_TAG, read_changelog_page, read_journal_entries,
};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

/// A real mutation from another driver rather than bytes shaped like one, so this keeps describing a
/// mixed node if either payload changes shape.
fn foreign_value(repo: &str) -> Vec<u8> {
    OciMutation::MountBlob {
        index: "images".to_owned(),
        repo: repo.to_owned(),
        digest: format!("sha256:{:064x}", 7),
    }
    .encode()
}

fn value(project: &str) -> Vec<u8> {
    serde_json::to_vec(&JournalEntry {
        serial: 999,
        submitted_at_unix: 123,
        action: "add-file".to_owned(),
        project: project.to_owned(),
        version: Some("1.0".to_owned()),
        filename: Some(format!("{project}-1.0.whl")),
        python: None,
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
                    python: None,
                },
                JournalEntry {
                    serial: 2,
                    submitted_at_unix: 123,
                    action: "add-file".to_owned(),
                    project: "second".to_owned(),
                    version: Some("1.0".to_owned()),
                    filename: Some("second-1.0.whl".to_owned()),
                    python: None,
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
                    br#"{"pypi-op":"journal-entry","serial":0,"action":"add-file","project":"old","version":null,"filename":null}"#
                        .to_vec(),
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
            python: None,
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
                    python: None,
                },
                JournalEntry {
                    serial: 3,
                    submitted_at_unix: 123,
                    action: "add-file".to_owned(),
                    project: "second".to_owned(),
                    version: Some("1.0".to_owned()),
                    filename: Some("second-1.0.whl".to_owned()),
                    python: None,
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

/// A push to another ecosystem lands between two `PyPI` publishes on the one shared journal, and the
/// changelog reports the publishes around it with the serials storage gave them.
#[test]
fn test_read_journal_entries_skips_a_foreign_ecosystem_record() {
    let (_dir, store) = store();
    store
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![value("first"), foreign_value("app"), value("second")])))
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
                    python: None,
                },
                JournalEntry {
                    serial: 3,
                    submitted_at_unix: 123,
                    action: "add-file".to_owned(),
                    project: "second".to_owned(),
                    version: Some("1.0".to_owned()),
                    filename: Some("second-1.0.whl".to_owned()),
                    python: None,
                },
            ],
        }
    );
}

/// `limit` bounds the entries a page returns, not the records it reads, so a run of foreign records
/// longer than the page cannot hide the publish behind it.
#[test]
fn test_read_journal_entries_walks_past_more_foreign_records_than_the_limit() {
    let (_dir, store) = store();
    store
        .commit_driver_txn(|_| {
            Ok::<_, MetaError>(((), vec![foreign_value("app"), foreign_value("api"), value("first")]))
        })
        .unwrap();

    let snapshot = read_journal_entries(&store, 0, 1).unwrap();

    assert_eq!(snapshot.current_serial, 3);
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.entries[0].project, "first");
    assert_eq!(snapshot.entries[0].serial, 3);
}

/// A caller that reaches a journal ending in foreign records resumes past them instead of asking for
/// the same records again.
#[test]
fn test_read_changelog_page_advances_past_records_it_all_skipped() {
    let (_dir, store) = store();
    store
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![foreign_value("app"), foreign_value("api")])))
        .unwrap();

    let page = read_changelog_page(&store, 0, 10).unwrap();

    assert_eq!((page.entries().len(), page.resume_serial()), (0, 2));
}

/// Claiming the tag and then failing to decode is corruption, not another driver's record: dropping it
/// would lose a mutation a mirror needs without saying so.
#[rstest]
#[case::missing_field(br#"{"pypi-op":"journal-entry","serial":0,"action":"add-file"}"#.to_vec())]
#[case::unknown_operation(br#"{"pypi-op":"revocation","serial":0}"#.to_vec())]
fn test_read_journal_entries_rejects_a_payload_claiming_the_pypi_tag(#[case] payload: Vec<u8>) {
    let (_dir, store) = store();
    store
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), vec![payload])))
        .unwrap();

    assert!(matches!(read_journal_entries(&store, 0, 10), Err(MetaError::Decode(_))));
}

#[test]
fn test_a_written_entry_carries_the_tag_the_reader_claims() {
    let payload = serde_json::from_slice::<serde_json::Value>(&value("first")).unwrap();

    assert_eq!(payload[PYPI_OP_TAG], serde_json::json!("journal-entry"));
}

/// The two vocabularies name themselves apart, so neither reader can claim the other's records.
#[test]
fn test_a_foreign_record_carries_no_pypi_tag() {
    let payload = serde_json::from_slice::<serde_json::Value>(&foreign_value("app")).unwrap();

    assert_eq!(payload.get(PYPI_OP_TAG), None);
}
