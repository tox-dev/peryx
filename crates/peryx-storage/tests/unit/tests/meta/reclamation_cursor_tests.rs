use crate::meta::{MetaStore, ReclamationPhase};

use super::store;

#[test]
fn test_a_phase_starts_without_a_cursor() {
    let (_directory, store) = store();

    assert_eq!(store.reclamation_cursor(ReclamationPhase::Selection).unwrap(), None);
}

#[test]
fn test_a_cursor_round_trips_where_the_scan_stopped() {
    let (_directory, store) = store();

    store
        .set_reclamation_cursor(ReclamationPhase::Selection, Some("0f1e"))
        .unwrap();

    assert_eq!(
        store.reclamation_cursor(ReclamationPhase::Selection).unwrap(),
        Some("0f1e".to_owned())
    );
}

#[test]
fn test_the_two_phases_advance_independently() {
    let (_directory, store) = store();
    store
        .set_reclamation_cursor(ReclamationPhase::Selection, Some("0f1e"))
        .unwrap();

    store
        .set_reclamation_cursor(ReclamationPhase::Finalize, Some("sha256:aa01"))
        .unwrap();

    assert_eq!(
        store.reclamation_cursor(ReclamationPhase::Selection).unwrap(),
        Some("0f1e".to_owned())
    );
}

#[test]
fn test_a_later_pass_overwrites_the_recorded_cursor() {
    let (_directory, store) = store();
    store
        .set_reclamation_cursor(ReclamationPhase::Finalize, Some("sha256:0f1e"))
        .unwrap();

    store
        .set_reclamation_cursor(ReclamationPhase::Finalize, Some("sha256:aa01"))
        .unwrap();

    assert_eq!(
        store.reclamation_cursor(ReclamationPhase::Finalize).unwrap(),
        Some("sha256:aa01".to_owned())
    );
}

#[test]
fn test_clearing_the_cursor_wraps_the_next_pass_to_the_first_row() {
    let (_directory, store) = store();
    store
        .set_reclamation_cursor(ReclamationPhase::Selection, Some("0f1e"))
        .unwrap();

    store.set_reclamation_cursor(ReclamationPhase::Selection, None).unwrap();

    assert_eq!(store.reclamation_cursor(ReclamationPhase::Selection).unwrap(), None);
}

#[test]
fn test_a_recorded_cursor_survives_a_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    MetaStore::open(&path)
        .unwrap()
        .set_reclamation_cursor(ReclamationPhase::Selection, Some("0f1e"))
        .unwrap();

    let reopened = MetaStore::open(&path).unwrap();

    assert_eq!(
        reopened.reclamation_cursor(ReclamationPhase::Selection).unwrap(),
        Some("0f1e".to_owned())
    );
}
