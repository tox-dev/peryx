use std::num::NonZeroUsize;

use crate::envelope::{AuthorityEpoch, TraceError};
use crate::reconcile::{
    Cleanup, Disposition, OldEpochIdentity, OldEpochOp, ReconcileAction, ReplayCommand, classify, cleanup,
    prune_reconcile, reconcile,
};
use rstest::rstest;

fn op(durably_committed: bool, already_applied: bool, superseded: bool) -> OldEpochOp {
    OldEpochOp {
        durably_committed,
        already_applied,
        superseded,
    }
}

#[rstest]
#[case::uncommitted(false, false, false, Disposition::Failed)]
#[case::applied(true, true, false, Disposition::AlreadyApplied)]
#[case::superseded(true, false, true, Disposition::Superseded)]
#[case::replayable(true, false, false, Disposition::Replayable)]
#[case::applied_precedes_superseded(true, true, true, Disposition::AlreadyApplied)]
#[case::uncommitted_precedes_status(false, true, true, Disposition::Failed)]
fn test_classify(
    #[case] committed: bool,
    #[case] applied: bool,
    #[case] superseded: bool,
    #[case] expected: Disposition,
) {
    assert_eq!(classify(&op(committed, applied, superseded)), expected);
}

const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const SPAN: &str = "b7ad6b7169203331";
const CHILD: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-b7ad6b7169203331-01";

fn identity(traceparent: Option<&'static str>) -> OldEpochIdentity<'static> {
    OldEpochIdentity {
        source: "east",
        epoch: AuthorityEpoch(4),
        serial: 42,
        traceparent,
    }
}

#[test]
fn test_reconcile_replays_a_standing_operation_under_the_new_epoch() {
    let action = reconcile(&op(true, false, false), identity(Some(PARENT)), AuthorityEpoch(7), SPAN).unwrap();

    assert_eq!(
        action,
        ReconcileAction::Replay(ReplayCommand {
            source: "east".to_owned(),
            from_epoch: AuthorityEpoch(4),
            epoch: AuthorityEpoch(7),
            serial: 42,
            traceparent: Some(CHILD.to_owned()),
        })
    );
}

#[test]
fn test_reconcile_replay_without_a_trace_carries_no_traceparent() {
    let action = reconcile(&op(true, false, false), identity(None), AuthorityEpoch(7), SPAN).unwrap();

    assert_eq!(
        action,
        ReconcileAction::Replay(ReplayCommand {
            source: "east".to_owned(),
            from_epoch: AuthorityEpoch(4),
            epoch: AuthorityEpoch(7),
            serial: 42,
            traceparent: None,
        })
    );
}

#[test]
fn test_reconcile_settles_a_terminal_operation_with_nothing_to_replay() {
    for (case, expected) in [
        (op(true, true, false), Disposition::AlreadyApplied),
        (op(true, false, true), Disposition::Superseded),
        (op(false, false, false), Disposition::Failed),
    ] {
        let action = reconcile(&case, identity(Some(PARENT)), AuthorityEpoch(7), SPAN).unwrap();
        assert_eq!(action, ReconcileAction::Settle(expected));
    }
}

#[test]
fn test_reconcile_settles_without_deriving_a_trace() {
    let action = reconcile(
        &op(true, true, false),
        identity(Some("not-a-traceparent")),
        AuthorityEpoch(7),
        SPAN,
    )
    .unwrap();
    assert_eq!(action, ReconcileAction::Settle(Disposition::AlreadyApplied));
}

#[test]
fn test_reconcile_rejects_a_malformed_parent_trace() {
    let error = reconcile(
        &op(true, false, false),
        identity(Some("ff-bad")),
        AuthorityEpoch(7),
        SPAN,
    )
    .unwrap_err();
    assert!(matches!(error, TraceError::MalformedParent(_)));
}

#[test]
fn test_reconcile_rejects_an_invalid_span_id() {
    let error = reconcile(
        &op(true, false, false),
        identity(Some(PARENT)),
        AuthorityEpoch(7),
        "zzzz",
    )
    .unwrap_err();
    assert!(matches!(error, TraceError::InvalidSpanId(_)));
}

#[test]
fn test_cleanup_releases_only_once_both_frontiers_pass() {
    assert_eq!(cleanup(10, 9, 100), Cleanup::Retain);
    assert_eq!(cleanup(10, 100, 9), Cleanup::Retain);
    assert_eq!(cleanup(10, 10, 10), Cleanup::Release);
    assert_eq!(cleanup(10, 50, 50), Cleanup::Release);
}

#[test]
fn test_cleanup_is_release_reports_the_verdict() {
    assert!(cleanup(5, 5, 5).is_release());
    assert!(!cleanup(5, 4, 5).is_release());
}

use peryx_ha::NewReconcileEntry;
use peryx_storage::meta::MetaStore;

use crate::reconcile::{ReconcileDrain, drain_reconcile};

const DRAIN_NOW: i64 = 1_800_000_000;

fn backlog_entry(serial: u64, committed: bool, applied: bool, superseded: bool) -> NewReconcileEntry<'static> {
    NewReconcileEntry {
        source: "east",
        epoch: 4,
        serial,
        durably_committed: committed,
        already_applied: applied,
        superseded,
        traceparent: None,
    }
}

#[test]
fn test_disposition_code_names_each_outcome() {
    assert_eq!(Disposition::AlreadyApplied.code(), "already_applied");
    assert_eq!(Disposition::Replayable.code(), "replayable");
    assert_eq!(Disposition::Superseded.code(), "superseded");
    assert_eq!(Disposition::Failed.code(), "failed");
}

#[test]
fn test_drain_classifies_and_settles_the_backlog_by_disposition() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("meta.redb")).unwrap();
    meta.enqueue_reconcile(&backlog_entry(1, true, false, false), DRAIN_NOW)
        .unwrap();
    meta.enqueue_reconcile(&backlog_entry(2, true, true, false), DRAIN_NOW)
        .unwrap();
    meta.enqueue_reconcile(&backlog_entry(3, true, false, true), DRAIN_NOW)
        .unwrap();
    meta.enqueue_reconcile(&backlog_entry(4, false, false, false), DRAIN_NOW)
        .unwrap();

    let report = drain_reconcile(&meta, 100, DRAIN_NOW).unwrap();

    assert_eq!(
        report,
        ReconcileDrain {
            replayable: 1,
            already_applied: 1,
            superseded: 1,
            failed: 1,
        }
    );
    assert_eq!(report.settled(), 4);
    assert_eq!(
        meta.reconcile_entry("east:4:1").unwrap().unwrap().outcome.as_deref(),
        Some("replayable")
    );
    assert_eq!(
        meta.reconcile_entry("east:4:4").unwrap().unwrap().outcome.as_deref(),
        Some("failed")
    );
}

#[test]
fn test_drain_is_bounded_and_settles_each_operation_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("meta.redb")).unwrap();
    for serial in 1..=3 {
        meta.enqueue_reconcile(&backlog_entry(serial, true, false, false), DRAIN_NOW)
            .unwrap();
    }

    assert_eq!(drain_reconcile(&meta, 2, DRAIN_NOW).unwrap().settled(), 2);
    assert_eq!(drain_reconcile(&meta, 100, DRAIN_NOW).unwrap().settled(), 1);
    assert_eq!(
        drain_reconcile(&meta, 100, DRAIN_NOW).unwrap(),
        ReconcileDrain::default()
    );
    assert!(meta.pending_reconcile(100).unwrap().is_empty());
}

#[test]
fn test_prune_requires_settlement_and_both_frontiers() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("meta.redb")).unwrap();
    for serial in 1..=3 {
        meta.enqueue_reconcile(&backlog_entry(serial, true, false, false), DRAIN_NOW)
            .unwrap();
    }
    meta.settle_reconcile("east:4:1", "replayable", DRAIN_NOW).unwrap();
    meta.settle_reconcile("east:4:2", "superseded", DRAIN_NOW).unwrap();

    assert_eq!(
        prune_reconcile(&meta, 1, 10, NonZeroUsize::new(10).unwrap()).unwrap(),
        1
    );
    assert_eq!(
        prune_reconcile(&meta, 10, 1, NonZeroUsize::new(10).unwrap()).unwrap(),
        0
    );
    assert_eq!(
        prune_reconcile(&meta, 10, 10, NonZeroUsize::new(10).unwrap()).unwrap(),
        1
    );
    assert!(meta.reconcile_entry("east:4:3").unwrap().unwrap().is_pending());
}

#[test]
fn test_prune_bounds_each_batch() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("meta.redb")).unwrap();
    for serial in 1..=4 {
        meta.enqueue_reconcile(&backlog_entry(serial, true, false, false), DRAIN_NOW)
            .unwrap();
        meta.settle_reconcile(&format!("east:4:{serial}"), "replayable", DRAIN_NOW)
            .unwrap();
    }

    assert_eq!(
        prune_reconcile(&meta, 10, 10, NonZeroUsize::new(2).unwrap()).unwrap(),
        2
    );
    assert_eq!(meta.count_reconcile().unwrap(), 2);
}

#[test]
fn test_prune_scans_past_a_page_of_pending_entries() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("meta.redb")).unwrap();
    for serial in 1..=3 {
        meta.enqueue_reconcile(&backlog_entry(serial, true, false, false), DRAIN_NOW)
            .unwrap();
    }

    assert_eq!(
        prune_reconcile(&meta, 10, 10, NonZeroUsize::new(2).unwrap()).unwrap(),
        0
    );
    assert_eq!(meta.count_reconcile().unwrap(), 3);
}
