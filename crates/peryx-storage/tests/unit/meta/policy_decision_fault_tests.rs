use std::sync::Arc;

use redb::backends::InMemoryBackend;
use rstest::rstest;

use crate::meta::fault::{self, Fault};

use super::*;

fn decision(state: PolicyDecisionState, evaluated_at_unix: i64) -> NewPolicyDecision<'static> {
    NewPolicyDecision {
        repository: "private",
        project: "package",
        version: Some("1.0"),
        filename: Some("package-1.0.bin"),
        source: Some("alpha"),
        action: PolicyAction::Serve,
        state,
        rule: None,
        reason: None,
        evaluated_at_unix,
        next_eligible_at_unix: None,
    }
}

fn subject() -> NewPolicyDecision<'static> {
    decision(PolicyDecisionState::Allow, 0)
}

fn query() -> PolicyDecisionQuery {
    PolicyDecisionQuery {
        limit: 10,
        ..PolicyDecisionQuery::default()
    }
}

fn open_tables(write: &redb::WriteTransaction) -> Result<(), redb::TableError> {
    write.open_table(SERIAL)?;
    write.open_table(POLICY_INPUT_GENERATION)?;
    write.open_table(POLICY_DECISION)?;
    write.open_table(POLICY_DECISION_CURRENT)?;
    write.open_table(POLICY_DECISION_CURRENT_ID)?;
    Ok(())
}

fn seeded_store() -> (MetaStore, Arc<InMemoryBackend>, Arc<Fault>) {
    let (inner, fault) = fault::backend();
    let store = fault::create(&inner, &fault, open_tables);
    store
        .record_policy_decision(decision(PolicyDecisionState::Allow, 10))
        .unwrap();
    (store, inner, fault)
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    Advance,
    Record,
    InputGen,
    Current,
    Query,
}

fn invoke(store: &MetaStore, operation: Operation) -> bool {
    match operation {
        Operation::Advance => store.advance_policy_generation("private").is_err(),
        Operation::Record => store
            .record_policy_decision(decision(PolicyDecisionState::Deny, 20))
            .is_err(),
        Operation::InputGen => store.policy_input_generation("private").is_err(),
        Operation::Current => store.current_policy_decision(subject()).is_err(),
        Operation::Query => store.query_policy_decisions(&query()).is_err(),
    }
}

fn assert_readable(store: &MetaStore) {
    // A fault leaves no torn row behind: every projection still reads, decodes, and stays consistent
    // with the one seeded decision plus at most the swept write.
    let page = store.query_policy_decisions(&query()).unwrap();
    assert!(
        (1..=2).contains(&page.decisions.len()),
        "unexpected history size: {}",
        page.decisions.len()
    );
    store.current_policy_decision(subject()).unwrap();
    store.policy_input_generation("private").unwrap();
}

#[rstest]
#[case::advance(Operation::Advance)]
#[case::record(Operation::Record)]
#[case::input_gen(Operation::InputGen)]
#[case::current(Operation::Current)]
#[case::query(Operation::Query)]
fn test_policy_decisions_survive_backend_failures(#[case] operation: Operation) {
    let mut failures = 0;
    for fail_after in 0..96 {
        let (store, inner, fault) = seeded_store();
        drop(store);
        let store = fault::reopen(&inner, &fault);
        fault.arm(fail_after);
        if invoke(&store, operation) {
            failures += 1;
            fault.disable();
            drop(store);
            assert_readable(&fault::reopen(&inner, &fault));
        }
    }
    assert!(failures > 0, "no backend failure surfaced for {operation:?}");
}

#[rstest]
#[case::advance(Operation::Advance)]
#[case::record(Operation::Record)]
#[case::input_gen(Operation::InputGen)]
#[case::current(Operation::Current)]
#[case::query(Operation::Query)]
fn test_policy_decisions_reject_a_poisoned_backend(#[case] operation: Operation) {
    let (store, inner, fault) = seeded_store();
    drop(store);
    let store = fault::reopen(&inner, &fault);
    fault.arm(0);
    assert!(store.policy_input_generation("private").is_err());
    fault.disable();

    assert!(invoke(&store, operation));
}

#[rstest]
#[case::input_gen(Operation::InputGen)]
#[case::advance(Operation::Advance)]
#[case::current(Operation::Current)]
#[case::query(Operation::Query)]
#[case::record(Operation::Record)]
fn test_policy_decisions_reject_a_malformed_generation(#[case] operation: Operation) {
    let (inner, fault) = fault::backend();
    let store = fault::create(&inner, &fault, open_tables);
    store
        .record_policy_decision(decision(PolicyDecisionState::Allow, 10))
        .unwrap();
    fault::corrupt(&store, POLICY_INPUT_GENERATION, "private", b"not json");

    let decodes = match operation {
        Operation::InputGen => matches!(store.policy_input_generation("private"), Err(MetaError::Decode(_))),
        Operation::Advance => matches!(store.advance_policy_generation("private"), Err(MetaError::Decode(_))),
        Operation::Current => matches!(
            store.current_policy_decision(subject()),
            Err(PolicyDecisionStoreError::Store(MetaError::Decode(_)))
        ),
        Operation::Query => matches!(
            store.query_policy_decisions(&query()),
            Err(PolicyDecisionQueryError::Store(MetaError::Decode(_)))
        ),
        Operation::Record => matches!(
            store.record_policy_decision(decision(PolicyDecisionState::Deny, 20)),
            Err(PolicyDecisionStoreError::Store(MetaError::Decode(_)))
        ),
    };
    assert!(decodes, "{operation:?} did not surface the decode failure");
}
