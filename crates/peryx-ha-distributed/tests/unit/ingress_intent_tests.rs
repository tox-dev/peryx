use std::num::NonZeroUsize;

use peryx_core::Ecosystem;

use crate::ingress_intent::{IngressIntent, IntentKey, IntentLedger, IntentState, StageOutcome, TransitionOutcome};

fn intent(idempotency_key: &str, digest: &str, size: u64) -> IngressIntent {
    IngressIntent {
        key: IntentKey {
            tenant: "acme".to_owned(),
            authority_key: "root/alpha/resource-a".to_owned(),
            idempotency_key: idempotency_key.to_owned(),
        },
        ecosystem: Ecosystem::new("example"),
        digest: digest.to_owned(),
        size,
        ingress_dc: "dc-west".to_owned(),
        operation_id: format!("op-{idempotency_key}"),
    }
}

fn limit(count: usize) -> NonZeroUsize {
    NonZeroUsize::new(count).unwrap()
}

#[test]
fn test_a_new_ledger_is_empty() {
    let ledger = IntentLedger::new(limit(4));

    assert!(ledger.is_empty());
    assert_eq!(ledger.len(), 0);
    assert_eq!(ledger.state(&intent("k1", "sha256:aa", 1).key), None);
    assert_eq!(ledger.get(&intent("k1", "sha256:aa", 1).key), None);
}

#[test]
fn test_restaging_the_same_key_and_content_is_an_idempotent_duplicate() {
    let mut ledger = IntentLedger::new(limit(4));
    let write = intent("k1", "sha256:aa", 10);

    assert_eq!(ledger.stage(write.clone()), StageOutcome::Admitted);
    assert_eq!(ledger.stage(write.clone()), StageOutcome::Duplicate);

    assert_eq!(ledger.len(), 1, "a duplicate must not retain a second intent");
    assert_eq!(ledger.get(&write.key), Some(&write));
    assert_eq!(ledger.state(&write.key), Some(IntentState::Pending));
}

#[test]
fn test_reusing_a_key_for_a_different_digest_conflicts_without_overwriting() {
    let mut ledger = IntentLedger::new(limit(4));
    let first = intent("k1", "sha256:aa", 10);
    assert_eq!(ledger.stage(first.clone()), StageOutcome::Admitted);

    assert_eq!(ledger.stage(intent("k1", "sha256:bb", 10)), StageOutcome::Conflict);

    assert_eq!(
        ledger.get(&first.key),
        Some(&first),
        "a conflicting restage must not overwrite the admitted content",
    );
    assert_eq!(ledger.len(), 1);
}

#[test]
fn test_reusing_a_key_for_a_different_size_conflicts_without_overwriting() {
    let mut ledger = IntentLedger::new(limit(4));
    let first = intent("k1", "sha256:aa", 10);
    assert_eq!(ledger.stage(first.clone()), StageOutcome::Admitted);

    assert_eq!(ledger.stage(intent("k1", "sha256:aa", 20)), StageOutcome::Conflict);

    assert_eq!(
        ledger.get(&first.key),
        Some(&first),
        "a matching digest with a different size is still a conflict, not an overwrite",
    );
    assert_eq!(ledger.len(), 1);
}

#[test]
fn test_admits_up_to_the_limit_then_rejects_the_next() {
    let mut ledger = IntentLedger::new(limit(2));

    assert_eq!(ledger.stage(intent("k1", "sha256:a1", 1)), StageOutcome::Admitted);
    assert_eq!(ledger.stage(intent("k2", "sha256:a2", 2)), StageOutcome::Admitted);
    assert_eq!(
        ledger.stage(intent("k3", "sha256:a3", 3)),
        StageOutcome::RejectedOverLimit
    );

    assert_eq!(ledger.len(), 2, "a rejected intent must not be retained");
    assert_eq!(ledger.state(&intent("k3", "sha256:a3", 3).key), None);
}

#[test]
fn test_an_existing_key_resolves_before_the_limit_even_at_capacity() {
    let mut ledger = IntentLedger::new(limit(2));
    let first = intent("k1", "sha256:a1", 1);
    assert_eq!(ledger.stage(first.clone()), StageOutcome::Admitted);
    assert_eq!(ledger.stage(intent("k2", "sha256:a2", 2)), StageOutcome::Admitted);

    assert_eq!(
        ledger.stage(first.clone()),
        StageOutcome::Duplicate,
        "a full ledger still deduplicates a known key rather than refusing it over the limit",
    );
    assert_eq!(
        ledger.stage(intent("k1", "sha256:bb", 9)),
        StageOutcome::Conflict,
        "a full ledger still conflicts a known key rather than refusing it over the limit",
    );

    assert_eq!(ledger.get(&first.key), Some(&first));
    assert_eq!(ledger.len(), 2);
}

#[test]
fn test_lifecycle_advances_forward_and_ignores_a_move_backward() {
    let mut ledger = IntentLedger::new(limit(4));
    let write = intent("k1", "sha256:aa", 10);
    ledger.stage(write.clone());
    assert_eq!(ledger.state(&write.key), Some(IntentState::Pending));

    assert_eq!(
        ledger.advance(&write.key, IntentState::Admitted),
        TransitionOutcome::Advanced
    );
    assert_eq!(ledger.state(&write.key), Some(IntentState::Admitted));

    assert_eq!(
        ledger.advance(&write.key, IntentState::Pending),
        TransitionOutcome::Ignored,
        "a stale transition must not move a settled intent backward",
    );
    assert_eq!(ledger.state(&write.key), Some(IntentState::Admitted));

    assert_eq!(
        ledger.advance(&write.key, IntentState::Expired),
        TransitionOutcome::Advanced
    );
    assert_eq!(ledger.state(&write.key), Some(IntentState::Expired));
}

#[test]
fn test_advance_ignores_a_duplicate_state_and_an_unknown_intent() {
    let mut ledger = IntentLedger::new(limit(4));
    let write = intent("k1", "sha256:aa", 10);
    ledger.stage(write.clone());

    assert_eq!(
        ledger.advance(&write.key, IntentState::Pending),
        TransitionOutcome::Ignored,
        "re-advancing to the current state is a no-op",
    );
    assert_eq!(ledger.state(&write.key), Some(IntentState::Pending));

    let unknown = intent("nope", "sha256:zz", 1).key;
    assert_eq!(
        ledger.advance(&unknown, IntentState::Admitted),
        TransitionOutcome::Ignored
    );
    assert_eq!(ledger.get(&unknown), None);
}
