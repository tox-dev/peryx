use super::*;

fn decision_in<'a>(repository: &'a str, resource: &'a str, evaluated_at_unix: i64) -> NewPolicyDecision<'a> {
    NewPolicyDecision {
        repository,
        resource,
        group: Some("1.0"),
        artifact: Some("artifact-1.0.bin"),
        source: Some("alpha"),
        action: PolicyAction::Serve,
        state: PolicyDecisionState::Allow,
        rule: None,
        reason: None,
        evaluated_at_unix,
        next_eligible_at_unix: None,
    }
}

fn decision(resource: &str, evaluated_at_unix: i64) -> NewPolicyDecision<'_> {
    decision_in("private", resource, evaluated_at_unix)
}

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn audit(store: &MetaStore, repository: Option<&str>) -> Vec<PolicyDecisionRecord> {
    store
        .query_policy_decisions(&PolicyDecisionQuery {
            repository: repository.map(str::to_owned),
            limit: 100,
            ..PolicyDecisionQuery::default()
        })
        .unwrap()
        .decisions
        .into_iter()
        .map(|item| item.record)
        .collect()
}

fn churn(store: &MetaStore, repository: &str, evaluations: i64, history_limit: usize) {
    for evaluated_at_unix in 0..evaluations {
        store
            .record_policy_decision_with_history_limit(
                decision_in(repository, &format!("resource-{evaluated_at_unix}"), evaluated_at_unix),
                history_limit,
            )
            .unwrap();
    }
}

#[test]
fn test_history_limit_bounds_the_audit_log() {
    let (_dir, store) = store();
    churn(&store, "private", 20, 16);

    assert_eq!(audit(&store, None).len(), 16);
}

#[test]
fn test_history_limit_keeps_the_current_decision_of_an_evicted_subject() {
    let (_dir, store) = store();
    let evicted = store
        .record_policy_decision_with_history_limit(decision("resource-first", 0), 16)
        .unwrap();
    churn(&store, "private", 20, 16);

    assert_eq!(
        (
            store.current_policy_decision(decision("resource-first", 0)).unwrap(),
            audit(&store, None).contains(&evicted),
        ),
        (Some(evicted), false)
    );
}

#[test]
fn test_history_limit_keeps_the_current_decision_of_another_repository() {
    let (_dir, store) = store();
    let flask = store
        .record_policy_decision_with_history_limit(decision_in("tenant-a", "flask", 0), 16)
        .unwrap();
    churn(&store, "tenant-b", 20, 16);

    assert_eq!(
        (
            store
                .current_policy_decision(decision_in("tenant-a", "flask", 0))
                .unwrap(),
            audit(&store, Some("tenant-a")),
        ),
        (Some(flask), Vec::new())
    );
}

#[test]
fn test_history_limit_keeps_a_current_decision_that_is_the_oldest_audit_row() {
    let (_dir, store) = store();
    let oldest = store
        .record_policy_decision_with_history_limit(decision_in("tenant-a", "flask", 0), 4)
        .unwrap();
    churn(&store, "tenant-b", 4, 4);

    assert_eq!(
        store
            .current_policy_decision(decision_in("tenant-a", "flask", 0))
            .unwrap(),
        Some(oldest)
    );
}

#[test]
fn test_history_limit_preserves_the_current_replacement() {
    let (_dir, store) = store();
    for evaluated_at_unix in 0..20 {
        store
            .record_policy_decision_with_history_limit(decision("resource", evaluated_at_unix), 16)
            .unwrap();
    }

    assert_eq!(
        store
            .current_policy_decision(decision("resource", 0))
            .unwrap()
            .unwrap()
            .evaluated_at_unix,
        19
    );
}

#[test]
fn test_a_preserved_current_decision_still_goes_stale_on_an_input_revision() {
    let (_dir, store) = store();
    let flask = store
        .record_policy_decision_with_history_limit(decision_in("tenant-a", "flask", 0), 16)
        .unwrap();
    churn(&store, "tenant-b", 20, 16);
    let preserved = store
        .current_policy_decision(decision_in("tenant-a", "flask", 0))
        .unwrap();
    store.advance_policy_generation("tenant-a").unwrap();

    assert_eq!(
        (
            preserved,
            store
                .current_policy_decision(decision_in("tenant-a", "flask", 0))
                .unwrap(),
        ),
        (Some(flask), None)
    );
}

#[test]
fn test_history_eviction_leaves_the_artifact_scan_on_current_decisions() {
    let (_dir, store) = store();
    let flask = store
        .record_policy_decision_with_history_limit(decision_in("tenant-a", "flask", 0), 16)
        .unwrap();
    churn(&store, "tenant-b", 20, 16);

    assert_eq!(
        store
            .current_policy_decisions_for_artifacts("tenant-a", "flask", &["artifact-1.0.bin"])
            .unwrap(),
        HashMap::from([(
            "artifact-1.0.bin".to_owned(),
            PolicyDecisionItem {
                record: flask,
                fresh: true
            }
        )])
    );
}
