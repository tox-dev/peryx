use super::{TRASH_GRACE_SECS, TrashRecord, TrashState, UnknownTrashState};
use crate::Ecosystem;
use rstest::rstest;

fn record() -> TrashRecord {
    TrashRecord {
        ecosystem: Ecosystem::new("example"),
        repository: "hosted".into(),
        resource: "resource-a".into(),
        artifact: Some("artifact-a.bin".into()),
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

#[rstest]
#[case::restorable(true, 1_000, TrashState::Restorable)]
#[case::past_deadline(true, 1_000 + TRASH_GRACE_SECS + 1, TrashState::Expired)]
#[case::content_removed(false, 1_000, TrashState::Expired)]
fn test_state(#[case] retained: bool, #[case] now: i64, #[case] expected: TrashState) {
    let record = TrashRecord { retained, ..record() };

    assert_eq!(
        (record.restorable(now), record.state(now)),
        (expected == TrashState::Restorable, expected)
    );
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
        repository: "other".to_owned().into(),
        ..record()
    };
    assert!(older.cursor() < other_repo.cursor(), "same time breaks by repository");
}

#[test]
fn test_cursor_handles_absent_artifact_and_digest() {
    let record = TrashRecord {
        artifact: None,
        digest: None,
        ..record()
    };
    assert!(
        record.cursor().ends_with('\u{1f}'),
        "empty identity fields still serialize"
    );
}
