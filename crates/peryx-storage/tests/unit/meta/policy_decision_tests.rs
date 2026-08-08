use peryx_policy::{PolicyAction, PolicyDecisionState};

use super::*;

fn decision(project: &str, evaluated_at_unix: i64) -> NewPolicyDecision<'_> {
    NewPolicyDecision {
        repository: "private",
        project,
        version: Some("1.0"),
        filename: Some("package-1.0.bin"),
        source: Some("alpha"),
        action: PolicyAction::Serve,
        state: PolicyDecisionState::Allow,
        rule: None,
        reason: None,
        evaluated_at_unix,
        next_eligible_at_unix: None,
    }
}

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

#[test]
fn test_history_limit_removes_old_subjects() {
    let (_dir, store) = store();
    for evaluated_at_unix in 0..20 {
        store
            .record_policy_decision_with_history_limit(
                decision(&format!("package-{evaluated_at_unix}"), evaluated_at_unix),
                16,
            )
            .unwrap();
    }

    assert_eq!(
        store
            .query_policy_decisions(&PolicyDecisionQuery {
                limit: 100,
                ..PolicyDecisionQuery::default()
            })
            .unwrap()
            .decisions
            .len(),
        16
    );
    assert!(
        store
            .current_policy_decision(decision("package-0", 0))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .current_policy_decision(decision("package-4", 0))
            .unwrap()
            .is_some()
    );
}

#[test]
fn test_history_limit_preserves_the_current_replacement() {
    let (_dir, store) = store();
    for evaluated_at_unix in 0..20 {
        store
            .record_policy_decision_with_history_limit(decision("package", evaluated_at_unix), 16)
            .unwrap();
    }

    assert_eq!(
        store
            .current_policy_decision(decision("package", 0))
            .unwrap()
            .unwrap()
            .evaluated_at_unix,
        19
    );
}
