use rstest::rstest;

use crate::meta::{
    MetaStore, OperationClaim, OperationOutcomeError, OperationOutcomeHealth, OperationOutcomeQuery,
    OperationOutcomeQueryError, OperationOutcomeRecord, OperationOutcomeRow, OperationResult, OperationState,
};
use crate::tests::pagination::{Page, pages_after_exclusive_cursor, terminal_page};

use super::store;

fn pending(expiry_unix: Option<i64>, now: i64) -> OperationOutcomeRecord {
    OperationOutcomeRecord {
        state: OperationState::Pending,
        response: Vec::new(),
        expiry_unix,
        updated_at_unix: now,
    }
}

#[test]
fn test_claim_admits_a_fresh_operation() {
    let (_dir, store) = store();

    assert_eq!(
        store.claim_operation("op-1", Some(10), 1).unwrap(),
        OperationClaim::Admitted
    );
    assert_eq!(store.operation_outcome("op-1").unwrap(), Some(pending(Some(10), 1)));
}

#[test]
fn test_a_second_claim_returns_the_existing_record() {
    let (_dir, store) = store();
    store.claim_operation("op-1", Some(10), 1).unwrap();

    assert_eq!(
        store.claim_operation("op-1", Some(999), 2).unwrap(),
        OperationClaim::Existing(pending(Some(10), 1))
    );
}

#[test]
fn test_finalize_stamps_the_terminal_result_and_response() {
    let (_dir, store) = store();
    store.claim_operation("op-1", Some(10), 1).unwrap();

    let record = store
        .finalize_operation("op-1", OperationResult::Published, b"serial-7", 5)
        .unwrap();

    assert_eq!(
        record,
        OperationOutcomeRecord {
            state: OperationState::Published,
            response: b"serial-7".to_vec(),
            expiry_unix: Some(10),
            updated_at_unix: 5,
        }
    );
    assert_eq!(store.operation_outcome("op-1").unwrap(), Some(record));
}

#[test]
fn test_finalize_records_a_failure() {
    let (_dir, store) = store();
    store.claim_operation("op-1", None, 1).unwrap();

    let record = store
        .finalize_operation("op-1", OperationResult::Failed, b"quota exceeded", 5)
        .unwrap();

    assert_eq!(record.state, OperationState::Failed);
    assert_eq!(record.response, b"quota exceeded");
}

#[test]
fn test_retry_after_finalize_replays_the_original_result() {
    let (_dir, store) = store();
    store.claim_operation("op-1", Some(10), 1).unwrap();
    let finalized = store
        .finalize_operation("op-1", OperationResult::Published, b"serial-7", 5)
        .unwrap();

    assert_eq!(
        store.claim_operation("op-1", Some(10), 9).unwrap(),
        OperationClaim::Existing(finalized)
    );
}

#[test]
fn test_finalizing_an_unclaimed_operation_is_rejected() {
    let (_dir, store) = store();

    let error = store
        .finalize_operation("ghost", OperationResult::Published, b"", 1)
        .unwrap_err();

    assert!(matches!(error, OperationOutcomeError::NotAdmitted { operation } if operation == "ghost"));
}

#[test]
fn test_finalizing_a_terminal_operation_is_rejected() {
    let (_dir, store) = store();
    store.claim_operation("op-1", None, 1).unwrap();
    store
        .finalize_operation("op-1", OperationResult::Published, b"first", 5)
        .unwrap();

    let error = store
        .finalize_operation("op-1", OperationResult::Failed, b"second", 6)
        .unwrap_err();

    assert!(matches!(error, OperationOutcomeError::AlreadyFinal { operation } if operation == "op-1"));
}

#[test]
fn test_outcome_is_none_for_an_unknown_operation() {
    let (_dir, store) = store();
    assert_eq!(store.operation_outcome("unknown").unwrap(), None);
}

#[test]
fn test_error_messages_name_the_operation() {
    assert_eq!(
        OperationOutcomeError::NotAdmitted {
            operation: "op-1".to_owned()
        }
        .to_string(),
        "operation op-1 was never admitted"
    );
    assert_eq!(
        OperationOutcomeError::AlreadyFinal {
            operation: "op-1".to_owned()
        }
        .to_string(),
        "operation op-1 is already finalized"
    );
}

#[test]
fn test_state_reports_whether_it_is_terminal() {
    assert!(!OperationState::Pending.is_terminal());
    assert!(OperationState::Published.is_terminal());
    assert!(OperationState::Failed.is_terminal());
}

#[test]
fn test_prune_removes_only_expired_terminal_records() {
    let (_dir, store) = store();
    store.claim_operation("pending", Some(10), 1).unwrap();
    store.claim_operation("unexpired", Some(100), 1).unwrap();
    store
        .finalize_operation("unexpired", OperationResult::Published, b"", 2)
        .unwrap();
    store.claim_operation("no-expiry", None, 1).unwrap();
    store
        .finalize_operation("no-expiry", OperationResult::Published, b"", 2)
        .unwrap();
    store.claim_operation("expired", Some(10), 1).unwrap();
    store
        .finalize_operation("expired", OperationResult::Failed, b"", 2)
        .unwrap();

    let pruned = store.prune_operation_outcomes(50, 10).unwrap();

    assert_eq!(pruned, 1);
    assert_eq!(store.operation_outcome("expired").unwrap(), None);
    assert!(store.operation_outcome("pending").unwrap().is_some());
    assert!(store.operation_outcome("unexpired").unwrap().is_some());
    assert!(store.operation_outcome("no-expiry").unwrap().is_some());
}

#[test]
fn test_prune_honors_the_limit() {
    let (_dir, store) = store();
    for id in ["a", "b"] {
        store.claim_operation(id, Some(10), 1).unwrap();
        store
            .finalize_operation(id, OperationResult::Published, b"", 2)
            .unwrap();
    }

    let pruned = store.prune_operation_outcomes(50, 1).unwrap();

    assert_eq!(pruned, 1);
    let remaining = ["a", "b"]
        .iter()
        .filter(|id| store.operation_outcome(id).unwrap().is_some())
        .count();
    assert_eq!(remaining, 1);
}

fn row(operation: &str, state: OperationState, expiry_unix: Option<i64>, updated_at_unix: i64) -> OperationOutcomeRow {
    OperationOutcomeRow {
        operation: operation.to_owned(),
        state,
        expiry_unix,
        updated_at_unix,
    }
}

#[test]
fn test_list_on_an_empty_ledger_returns_no_rows() {
    let (_dir, store) = store();

    let page = store
        .list_operation_outcomes(&OperationOutcomeQuery::default())
        .unwrap();

    assert!(page.rows.is_empty());
    assert_eq!(page.next_cursor, None);
}

#[test]
fn test_list_returns_rows_in_operation_id_order() {
    let (_dir, store) = store();
    store.claim_operation("op-b", Some(10), 1).unwrap();
    store.claim_operation("op-a", None, 2).unwrap();
    store
        .finalize_operation("op-a", OperationResult::Published, b"", 3)
        .unwrap();

    let page = store
        .list_operation_outcomes(&OperationOutcomeQuery::default())
        .unwrap();

    assert_eq!(
        page.rows,
        vec![
            row("op-a", OperationState::Published, None, 3),
            row("op-b", OperationState::Pending, Some(10), 1),
        ],
    );
    assert_eq!(page.next_cursor, None);
}

#[test]
fn test_list_paginates_after_an_exclusive_cursor() {
    let (_dir, store) = store();
    assert_eq!(
        pages_after_exclusive_cursor(
            &["op-a", "op-b", "op-c"],
            |operation| seed_operation(&store, operation),
            |cursor, limit| operation_page(&store, cursor, limit),
            1,
        )
        .unwrap(),
        (
            Page {
                keys: vec!["op-a".to_owned()],
                next_cursor: Some("op-a".to_owned()),
            },
            Page {
                keys: vec!["op-b".to_owned()],
                next_cursor: Some("op-b".to_owned()),
            },
        )
    );
}

#[test]
fn test_list_page_that_exactly_fills_carries_no_next_cursor() {
    let (_dir, store) = store();
    assert_eq!(
        terminal_page(
            &["op-a", "op-b"],
            |operation| seed_operation(&store, operation),
            |cursor, limit| operation_page(&store, cursor, limit),
        )
        .unwrap(),
        Page {
            keys: vec!["op-a".to_owned(), "op-b".to_owned()],
            next_cursor: None,
        }
    );
}

#[rstest]
#[case(0)]
#[case(101)]
fn test_list_rejects_an_out_of_range_limit(#[case] limit: usize) {
    let (_dir, store) = store();

    let result = store.list_operation_outcomes(&OperationOutcomeQuery { cursor: None, limit });

    assert!(matches!(result, Err(OperationOutcomeQueryError::InvalidLimit)));
}

fn seed_operation(store: &MetaStore, operation: &str) {
    store.claim_operation(operation, None, 1).unwrap();
}

fn operation_page(store: &MetaStore, cursor: Option<String>, limit: usize) -> Result<Page, OperationOutcomeQueryError> {
    let page = store.list_operation_outcomes(&OperationOutcomeQuery { cursor, limit })?;
    Ok(Page {
        keys: page.rows.into_iter().map(|row| row.operation).collect(),
        next_cursor: page.next_cursor,
    })
}

#[test]
fn test_health_buckets_by_client_facing_status_at_the_clock() {
    let (_dir, store) = store();
    store.claim_operation("op-live", Some(100), 1).unwrap();
    store.claim_operation("op-stale", Some(10), 1).unwrap();
    store.claim_operation("op-done", None, 1).unwrap();
    store
        .finalize_operation("op-done", OperationResult::Published, b"", 2)
        .unwrap();
    store.claim_operation("op-gone", None, 1).unwrap();
    store
        .finalize_operation("op-gone", OperationResult::Failed, b"", 2)
        .unwrap();

    let health = store.operation_outcome_health(50).unwrap();

    assert_eq!(
        health,
        OperationOutcomeHealth {
            pending: 1,
            published: 1,
            failed: 1,
            expired: 1,
        }
    );
    assert_eq!(health.total(), 4);
}

#[test]
fn test_health_reads_a_pending_write_within_its_deadline_as_pending() {
    let (_dir, store) = store();
    store.claim_operation("op-live", Some(100), 1).unwrap();

    assert_eq!(store.operation_outcome_health(99).unwrap().pending, 1);
    assert_eq!(store.operation_outcome_health(100).unwrap().expired, 1);
}
