use super::{TRASH_GRACE_SECS, TrashRecord, TrashState, UnknownTrashState};
use crate::Ecosystem;

fn record() -> TrashRecord {
    TrashRecord {
        ecosystem: Ecosystem::new("example"),
        repository: "hosted".to_owned(),
        name: "flask".to_owned(),
        reference: Some("flask-1.0.bin".to_owned()),
        digest: Some("sha256:abc".to_owned()),
        reason: Some("bad build".to_owned()),
        actor: Some("alice".to_owned()),
        deleted_at_unix: 1_000,
        retained: true,
    }
}

#[test]
fn test_trash_state_string_forms_and_parsing() {
    assert_eq!(TrashState::Restorable.as_str(), "restorable");
    assert_eq!(TrashState::Expired.to_string(), "expired");
    assert_eq!("restorable".parse::<TrashState>().unwrap(), TrashState::Restorable);
    assert_eq!("expired".parse::<TrashState>().unwrap(), TrashState::Expired);
}

#[test]
fn test_trash_state_rejects_an_unknown_value() {
    let err = "gone".parse::<TrashState>().unwrap_err();
    assert_eq!(err, UnknownTrashState("gone".to_owned()));
    assert_eq!(err.to_string(), "unknown trash state: gone");
}

#[test]
fn test_deadline_follows_the_grace_window() {
    assert_eq!(record().deadline_unix(), 1_000 + TRASH_GRACE_SECS);
}

#[test]
fn test_restorable_within_the_window_with_retained_content() {
    let record = record();
    assert!(record.restorable(1_000));
    assert_eq!(record.state(1_000), TrashState::Restorable);
}

#[test]
fn test_expired_after_the_window_even_when_retained() {
    let record = record();
    let past = record.deadline_unix() + 1;
    assert!(!record.restorable(past));
    assert_eq!(record.state(past), TrashState::Expired);
}

#[test]
fn test_expired_when_content_is_not_retained() {
    let record = TrashRecord {
        retained: false,
        ..record()
    };
    assert!(!record.restorable(1_000));
    assert_eq!(record.state(1_000), TrashState::Expired);
}

#[test]
fn test_cursor_orders_newest_first_then_by_identity() {
    let newest = TrashRecord {
        deleted_at_unix: 2_000,
        ..record()
    };
    let older = record();
    assert!(newest.cursor() < older.cursor(), "a later deletion sorts first");

    let other_repo = TrashRecord {
        repository: "other".to_owned(),
        ..record()
    };
    assert!(older.cursor() < other_repo.cursor(), "same time breaks by repository");
}

#[test]
fn test_cursor_handles_absent_reference_and_digest() {
    let record = TrashRecord {
        reference: None,
        digest: None,
        ..record()
    };
    assert!(
        record.cursor().ends_with('\u{1f}'),
        "empty identity fields still serialize"
    );
}
