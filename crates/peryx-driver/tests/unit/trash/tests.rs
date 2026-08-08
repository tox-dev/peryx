use super::{DEFAULT_LIMIT, TrashItem, TrashQuery, TrashQueryError, TrashRef, paginate};
use peryx_core::{Ecosystem, TRASH_GRACE_SECS, TrashRecord, TrashState};

fn record(repository: &str, name: &str, deleted_at_unix: i64, retained: bool) -> TrashRecord {
    TrashRecord {
        ecosystem: Ecosystem::new("example"),
        repository: repository.to_owned(),
        name: name.to_owned(),
        reference: Some(format!("{name}.bin")),
        digest: Some("sha256:abc".to_owned()),
        reason: None,
        actor: Some("alice".to_owned()),
        deleted_at_unix,
        retained,
    }
}

fn query(limit: usize) -> TrashQuery {
    TrashQuery {
        limit,
        ..TrashQuery::default()
    }
}

#[test]
fn test_default_query_carries_the_default_page_size() {
    assert_eq!(TrashQuery::default().limit, DEFAULT_LIMIT);
}

#[test]
fn test_validate_rejects_bad_limit_cursor_and_repository() {
    assert_eq!(query(0).validate(), Err(TrashQueryError::InvalidLimit));
    assert_eq!(query(101).validate(), Err(TrashQueryError::InvalidLimit));
    assert_eq!(
        TrashQuery {
            cursor: Some(String::new()),
            ..query(25)
        }
        .validate(),
        Err(TrashQueryError::InvalidCursor)
    );
    assert_eq!(
        TrashQuery {
            cursor: Some("x".repeat(1_025)),
            ..query(25)
        }
        .validate(),
        Err(TrashQueryError::InvalidCursor)
    );
    assert_eq!(
        TrashQuery {
            repository: Some("r".repeat(513)),
            ..query(25)
        }
        .validate(),
        Err(TrashQueryError::RepositoryTooLong)
    );
    assert_eq!(query(25).validate(), Ok(()));
}

#[test]
fn test_error_messages_are_actionable() {
    assert_eq!(
        TrashQueryError::InvalidLimit.to_string(),
        "limit must be between 1 and 100"
    );
    assert_eq!(TrashQueryError::InvalidCursor.to_string(), "invalid trash cursor");
    assert_eq!(
        TrashQueryError::RepositoryTooLong.to_string(),
        "repository filter exceeds 512 bytes"
    );
    assert_eq!(TrashQueryError::Store("boom".to_owned()).to_string(), "boom");
}

#[test]
fn test_paginate_orders_newest_first_and_derives_state() {
    let records = vec![
        record("hosted", "old", 1_000, true),
        record("hosted", "new", 2_000, true),
    ];

    let page = paginate(records, &query(25), 2_000);

    assert_eq!(page.next_cursor, None);
    let names: Vec<&str> = page.items.iter().map(|item| item.record.name.as_str()).collect();
    assert_eq!(names, vec!["new", "old"], "newest deletion leads");
    assert!(page.items[0].restorable);
    assert_eq!(page.items[0].state, TrashState::Restorable);
    assert_eq!(page.items[0].deadline_unix, 2_000 + TRASH_GRACE_SECS);
}

#[test]
fn test_paginate_cursor_resumes_after_the_last_row_and_stays_stable() {
    let records = vec![
        record("hosted", "a", 3_000, true),
        record("hosted", "b", 2_000, true),
        record("hosted", "c", 1_000, true),
    ];

    let first = paginate(records.clone(), &query(2), 3_000);
    assert_eq!(
        first
            .items
            .iter()
            .map(|item| item.record.name.clone())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    let cursor = first.next_cursor.expect("a third row remains");

    // A newer artifact entering trash does not shift the boundary the cursor names.
    let mut grown = records;
    grown.push(record("hosted", "z", 4_000, true));
    let second = paginate(
        grown,
        &TrashQuery {
            cursor: Some(cursor),
            ..query(2)
        },
        4_000,
    );

    assert_eq!(
        second
            .items
            .iter()
            .map(|item| item.record.name.clone())
            .collect::<Vec<_>>(),
        vec!["c"],
        "the resumed page holds despite the insertion"
    );
    assert_eq!(second.next_cursor, None);
}

#[test]
fn test_paginate_filters_by_repository_ecosystem_state_and_deadline() {
    let records = vec![
        record("hosted", "keep", 1_000, true),
        record("other", "wrong-repo", 1_000, true),
        TrashRecord {
            ecosystem: Ecosystem::new("other"),
            ..record("hosted", "wrong-eco", 1_000, true)
        },
        record("hosted", "expired", 1_000, false),
    ];

    let restorable = paginate(
        records.clone(),
        &TrashQuery {
            repository: Some("hosted".to_owned()),
            ecosystem: Some(Ecosystem::new("example")),
            state: Some(TrashState::Restorable),
            ..query(25)
        },
        1_000,
    );
    assert_eq!(
        restorable
            .items
            .iter()
            .map(|item| item.record.name.clone())
            .collect::<Vec<_>>(),
        vec!["keep"]
    );

    let expiring = paginate(
        records,
        &TrashQuery {
            deadline_before_unix: Some(1_000 + TRASH_GRACE_SECS),
            ..query(25)
        },
        1_000,
    );
    assert_eq!(expiring.items.len(), 4, "every record's deadline is at the cutoff");
}

#[test]
fn test_trash_ref_matches_only_the_named_record() {
    let record = record("hosted", "flask", 1_000, true);
    let reference = TrashRef {
        ecosystem: Ecosystem::new("example"),
        repository: "hosted".to_owned(),
        name: "flask".to_owned(),
        reference: Some("flask.bin".to_owned()),
        digest: Some("sha256:abc".to_owned()),
    };
    assert!(reference.matches(&record));
    assert!(
        !TrashRef {
            name: "other".to_owned(),
            ..reference
        }
        .matches(&record)
    );
}

#[test]
fn test_trash_item_new_derives_expired_for_unretained_content() {
    let item = TrashItem::new(record("hosted", "flask", 1_000, false), 1_000);
    assert!(!item.restorable);
    assert_eq!(item.state, TrashState::Expired);
}
