use super::store;
use crate::meta::JournalEntry;

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
