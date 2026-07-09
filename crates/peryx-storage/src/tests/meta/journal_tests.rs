use super::store;
use crate::meta::{JournalEntry, MetadataSibling, PublishedFile};

#[test]
fn test_serial_starts_at_zero_and_increments() {
    let (_dir, store) = store();
    assert_eq!(store.current_serial().unwrap(), 0);
    assert_eq!(store.next_serial().unwrap(), 1);
    assert_eq!(store.next_serial().unwrap(), 2);
    assert_eq!(store.current_serial().unwrap(), 2);
}

#[test]
fn test_journal_appends_entries_and_reads_the_changelog() {
    let (_dir, store) = store();
    assert_eq!(
        store
            .append_journal("add-file", "flask", Some("1.0"), Some("flask-1.0.whl"))
            .unwrap(),
        1
    );
    assert_eq!(store.append_journal("promote", "flask", None, None).unwrap(), 2);
    assert_eq!(store.current_serial().unwrap(), 2);

    let all = store.journal_since(0).unwrap();
    assert_eq!(
        all,
        vec![
            JournalEntry {
                serial: 1,
                action: "add-file".to_owned(),
                project: "flask".to_owned(),
                version: Some("1.0".to_owned()),
                filename: Some("flask-1.0.whl".to_owned()),
            },
            JournalEntry {
                serial: 2,
                action: "promote".to_owned(),
                project: "flask".to_owned(),
                version: None,
                filename: None,
            },
        ]
    );

    let tail = store.journal_since(1).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].serial, 2);
}

#[test]
fn test_publish_file_commits_every_row_with_its_journal_entry() {
    let (_dir, store) = store();
    let serial = store
        .publish_file(&PublishedFile {
            index: "private",
            normalized: "flask",
            display: "Flask",
            filename: "flask-1.0.whl",
            record: b"{}",
            version: "1.0",
            metadata: Some(MetadataSibling {
                artifact_sha256: "a".repeat(64).as_str(),
                url: "uploaded",
                metadata_sha256: "b".repeat(64).as_str(),
                source: "private",
            }),
        })
        .unwrap();

    assert_eq!(serial, 1);
    assert_eq!(
        store.get_upload("private", "flask", "flask-1.0.whl").unwrap(),
        Some(b"{}".to_vec())
    );
    assert_eq!(store.get_project("private", "flask").unwrap(), Some("Flask".to_owned()));
    assert!(store.get_metadata(&"a".repeat(64)).unwrap().is_some());
    assert_eq!(
        store.journal_since(0).unwrap(),
        vec![JournalEntry {
            serial: 1,
            action: "add-file".to_owned(),
            project: "flask".to_owned(),
            version: Some("1.0".to_owned()),
            filename: Some("flask-1.0.whl".to_owned()),
        }]
    );
}

#[test]
fn test_publish_file_journals_the_project_not_the_index() {
    let (_dir, store) = store();
    store
        .publish_file(&PublishedFile {
            index: "private",
            normalized: "flask",
            display: "Flask",
            filename: "flask-1.0.whl",
            record: b"{}",
            version: "1.0",
            metadata: None,
        })
        .unwrap();

    // A replica replays the changelog by project name; recording the index here made every uploaded
    // file appear under a project called "private".
    assert_eq!(store.journal_since(0).unwrap()[0].project, "flask");
}
