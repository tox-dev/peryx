use std::sync::Arc;

use peryx_policy::{PolicyAction, PolicyDecisionState};

use crate::meta::{NewPolicyDecision, PolicyDecisionQuery, PolicyDecisionQueryError, PolicyDecisionStoreError};

use super::store;

fn decision(project: &str, state: PolicyDecisionState, evaluated_at_unix: i64) -> NewPolicyDecision<'_> {
    NewPolicyDecision {
        repository: "private",
        project,
        version: Some("1.0"),
        filename: Some("package-1.0.bin"),
        source: Some("alpha"),
        action: PolicyAction::Serve,
        state,
        rule: (state == PolicyDecisionState::Deny).then_some("blocked-project"),
        reason: (state == PolicyDecisionState::Deny).then_some("project is blocked"),
        evaluated_at_unix,
        next_eligible_at_unix: (state == PolicyDecisionState::Wait).then_some(evaluated_at_unix + 60),
    }
}

fn publish_catalog(meta: &crate::meta::MetaStore, generation: u64) {
    meta.commit_driver_txn_with_catalog_generation("private", generation, |_txn| {
        Ok::<_, crate::meta::MetaError>(((), Vec::new()))
    })
    .unwrap();
}

#[test]
fn test_policy_decision_replaces_current_and_retains_history() {
    let (_dir, meta) = store();
    publish_catalog(&meta, 8);
    meta.advance_policy_generation("private").unwrap();
    let denied = meta
        .record_policy_decision(decision("package", PolicyDecisionState::Deny, 10))
        .unwrap();
    let allowed = meta
        .record_policy_decision(decision("package", PolicyDecisionState::Allow, 11))
        .unwrap();

    assert_eq!(
        (
            meta.current_policy_decision(decision("package", PolicyDecisionState::Allow, 0))
                .unwrap()
                .unwrap(),
            meta.query_policy_decisions(&PolicyDecisionQuery {
                limit: 10,
                ..PolicyDecisionQuery::default()
            })
            .unwrap()
            .decisions
            .into_iter()
            .map(|item| item.record)
            .collect::<Vec<_>>(),
        ),
        (allowed.clone(), vec![allowed, denied])
    );
}

#[test]
fn test_policy_decision_repository_change_makes_current_stale() {
    let (_dir, meta) = store();
    meta.advance_policy_generation("private").unwrap();
    meta.record_policy_decision(decision("package", PolicyDecisionState::Allow, 10))
        .unwrap();
    meta.next_serial().unwrap();

    assert_eq!(
        (
            meta.current_policy_decision(decision("package", PolicyDecisionState::Allow, 0))
                .unwrap(),
            meta.query_policy_decisions(&PolicyDecisionQuery {
                limit: 1,
                ..PolicyDecisionQuery::default()
            })
            .unwrap()
            .decisions[0]
                .fresh,
        ),
        (None, false)
    );
}

#[test]
fn test_policy_decision_catalog_change_makes_current_stale() {
    let (_dir, meta) = store();
    publish_catalog(&meta, 1);
    meta.advance_policy_generation("private").unwrap();
    meta.record_policy_decision(decision("package", PolicyDecisionState::Allow, 10))
        .unwrap();
    publish_catalog(&meta, 2);

    assert_eq!(
        meta.current_policy_decision(decision("package", PolicyDecisionState::Allow, 0))
            .unwrap(),
        None
    );
}

#[test]
fn test_policy_decision_policy_change_makes_current_stale() {
    let (_dir, meta) = store();
    meta.advance_policy_generation("private").unwrap();
    meta.record_policy_decision(decision("package", PolicyDecisionState::Allow, 10))
        .unwrap();
    meta.advance_policy_generation("private").unwrap();

    assert_eq!(
        meta.current_policy_decision(decision("package", PolicyDecisionState::Allow, 0))
            .unwrap(),
        None
    );
}

#[test]
fn test_policy_decision_catalog_publication_is_atomic_with_driver_rows() {
    let (_dir, meta) = store();
    publish_catalog(&meta, 1);
    meta.advance_policy_generation("private").unwrap();
    meta.record_policy_decision(decision("package", PolicyDecisionState::Allow, 10))
        .unwrap();

    meta.commit_driver_txn_with_catalog_generation("private", 2, |txn| {
        txn.put_local("catalog/private", b"2")?;
        Ok::<_, crate::meta::MetaError>(((), Vec::new()))
    })
    .unwrap();

    assert_eq!(
        (
            meta.get_driver_value("catalog/private").unwrap(),
            meta.policy_input_generation("private").unwrap().catalog,
            meta.current_policy_decision(decision("package", PolicyDecisionState::Allow, 0))
                .unwrap(),
        ),
        (Some(b"2".to_vec()), 2, None)
    );
}

#[test]
fn test_policy_decision_failed_catalog_publication_rolls_back_generation() {
    let (_dir, meta) = store();
    publish_catalog(&meta, 1);
    meta.advance_policy_generation("private").unwrap();

    let result = meta.commit_driver_txn_with_catalog_generation("private", 2, |txn| {
        txn.put_local("catalog/private", b"2")?;
        Err::<((), Vec<Vec<u8>>), _>(crate::meta::MetaError::DriverPrecondition("failed".to_owned()))
    });

    assert_eq!(
        (
            result.is_err(),
            meta.get_driver_value("catalog/private").unwrap(),
            meta.policy_input_generation("private").unwrap().catalog,
        ),
        (true, None, 1)
    );
}

#[test]
fn test_policy_catalog_generation_captures_repository_serial() {
    let (_dir, meta) = store();
    meta.next_serial().unwrap();
    publish_catalog(&meta, 1);

    assert_eq!(
        meta.policy_input_generation("private").unwrap(),
        crate::meta::PolicyInputGeneration {
            repository: 1,
            catalog: 1,
            policy: 0,
        }
    );
}

#[test]
fn test_policy_decision_query_filters_and_paginates() {
    let (_dir, meta) = store();
    for (project, state, evaluated_at_unix) in [
        ("alpha", PolicyDecisionState::Allow, 10),
        ("beta", PolicyDecisionState::Deny, 20),
        ("gamma", PolicyDecisionState::Deny, 30),
    ] {
        meta.record_policy_decision(decision(project, state, evaluated_at_unix))
            .unwrap();
    }
    let first = meta
        .query_policy_decisions(&PolicyDecisionQuery {
            state: Some(PolicyDecisionState::Deny),
            source: Some("alpha".to_owned()),
            evaluated_from_unix: Some(15),
            limit: 1,
            ..PolicyDecisionQuery::default()
        })
        .unwrap();
    let second_query = PolicyDecisionQuery {
        state: Some(PolicyDecisionState::Deny),
        source: Some("alpha".to_owned()),
        evaluated_from_unix: Some(15),
        cursor: first.next_cursor.clone(),
        limit: 1,
        ..PolicyDecisionQuery::default()
    };
    second_query.validate().unwrap();
    let second = meta.query_policy_decisions(&second_query).unwrap();

    assert_eq!(
        (
            first
                .decisions
                .iter()
                .map(|item| item.record.project.as_str())
                .collect::<Vec<_>>(),
            first.next_cursor.is_some(),
            second
                .decisions
                .iter()
                .map(|item| item.record.project.as_str())
                .collect::<Vec<_>>(),
            second.next_cursor,
        ),
        (vec!["gamma"], true, vec!["beta"], None)
    );
}

#[test]
fn test_policy_decision_query_scopes_to_one_project() {
    let (_dir, meta) = store();
    for (project, state, evaluated_at_unix) in [
        ("alpha", PolicyDecisionState::Deny, 10),
        ("beta", PolicyDecisionState::Deny, 20),
        ("alpha", PolicyDecisionState::Allow, 30),
    ] {
        meta.record_policy_decision(decision(project, state, evaluated_at_unix))
            .unwrap();
    }

    let scoped = meta
        .query_policy_decisions(&PolicyDecisionQuery {
            project: Some("alpha".to_owned()),
            limit: 10,
            ..PolicyDecisionQuery::default()
        })
        .unwrap();

    assert_eq!(
        scoped
            .decisions
            .iter()
            .map(|item| (item.record.project.as_str(), item.record.state))
            .collect::<Vec<_>>(),
        vec![
            ("alpha", PolicyDecisionState::Allow),
            ("alpha", PolicyDecisionState::Deny)
        ]
    );
}

#[test]
fn test_policy_decision_rejects_zero_limit() {
    let (_dir, meta) = store();
    let query = PolicyDecisionQuery {
        limit: 0,
        ..PolicyDecisionQuery::default()
    };

    assert!(matches!(
        (query.validate(), meta.query_policy_decisions(&query)),
        (
            Err(PolicyDecisionQueryError::InvalidLimit),
            Err(PolicyDecisionQueryError::InvalidLimit)
        )
    ));
}

#[test]
fn test_policy_decision_rejects_invalid_cursor() {
    let (_dir, meta) = store();
    let query = PolicyDecisionQuery {
        cursor: Some("bad".to_owned()),
        ..PolicyDecisionQuery::default()
    };

    assert!(matches!(
        (query.validate(), meta.query_policy_decisions(&query)),
        (
            Err(PolicyDecisionQueryError::InvalidCursor),
            Err(PolicyDecisionQueryError::InvalidCursor)
        )
    ));
}

#[test]
fn test_policy_decision_query_bounds_text_filters() {
    let (_dir, meta) = store();
    let bounded = "x".repeat(512);
    let oversized = "x".repeat(513);
    let mut candidate = decision("bounded", PolicyDecisionState::Allow, 10);
    candidate.repository = &bounded;
    candidate.project = &bounded;
    candidate.rule = Some(&bounded);
    candidate.source = Some(&bounded);
    let expected = meta.record_policy_decision(candidate).unwrap();

    for query in [
        PolicyDecisionQuery {
            repository: Some(bounded.clone()),
            ..PolicyDecisionQuery::default()
        },
        PolicyDecisionQuery {
            project: Some(bounded.clone()),
            ..PolicyDecisionQuery::default()
        },
        PolicyDecisionQuery {
            rule: Some(bounded.clone()),
            ..PolicyDecisionQuery::default()
        },
        PolicyDecisionQuery {
            source: Some(bounded),
            ..PolicyDecisionQuery::default()
        },
    ] {
        query.validate().unwrap();
        assert_eq!(
            meta.query_policy_decisions(&query).unwrap().decisions[0].record,
            expected
        );
    }

    for (field, query) in [
        (
            "repository",
            PolicyDecisionQuery {
                repository: Some(oversized.clone()),
                ..PolicyDecisionQuery::default()
            },
        ),
        (
            "project",
            PolicyDecisionQuery {
                project: Some(oversized.clone()),
                ..PolicyDecisionQuery::default()
            },
        ),
        (
            "rule",
            PolicyDecisionQuery {
                rule: Some(oversized.clone()),
                ..PolicyDecisionQuery::default()
            },
        ),
        (
            "source",
            PolicyDecisionQuery {
                source: Some(oversized),
                ..PolicyDecisionQuery::default()
            },
        ),
    ] {
        assert!(matches!(
            (query.validate(), meta.query_policy_decisions(&query)),
            (
                Err(PolicyDecisionQueryError::FilterTooLong { field: actual, max: 512 }),
                Err(PolicyDecisionQueryError::FilterTooLong { .. })
            ) if actual == field
        ));
    }
}

#[test]
fn test_policy_decision_validation_rolls_back() {
    let (_dir, meta) = store();
    let reason = "x".repeat(2_049);
    let mut oversized = decision("package", PolicyDecisionState::Deny, 10);
    oversized.reason = Some(&reason);

    assert!(matches!(
        (
            meta.record_policy_decision(oversized),
            meta.query_policy_decisions(&PolicyDecisionQuery {
                limit: 1,
                ..PolicyDecisionQuery::default()
            })
            .unwrap()
            .decisions,
        ),
        (
            Err(PolicyDecisionStoreError::FieldTooLong { field: "reason", .. }),
            decisions
        ) if decisions.is_empty()
    ));
}

#[test]
fn test_policy_decision_validation_bounds_subject_fields() {
    enum Field {
        Repository,
        Project,
        Version,
        Filename,
        Source,
        Rule,
    }

    let (_dir, meta) = store();
    let oversized = "x".repeat(513);
    for (field, name) in [
        (Field::Repository, "repository"),
        (Field::Project, "project"),
        (Field::Version, "version"),
        (Field::Filename, "filename"),
        (Field::Source, "source"),
        (Field::Rule, "rule"),
    ] {
        let mut candidate = decision("package", PolicyDecisionState::Allow, 10);
        match field {
            Field::Repository => candidate.repository = &oversized,
            Field::Project => candidate.project = &oversized,
            Field::Version => candidate.version = Some(&oversized),
            Field::Filename => candidate.filename = Some(&oversized),
            Field::Source => candidate.source = Some(&oversized),
            Field::Rule => candidate.rule = Some(&oversized),
        }
        assert!(matches!(
            meta.record_policy_decision(candidate),
            Err(PolicyDecisionStoreError::FieldTooLong { field: actual, .. }) if actual == name
        ));
    }
}

#[test]
fn test_policy_generation_initializes_an_unknown_repository() {
    let (_dir, meta) = store();

    assert_eq!(
        (
            meta.advance_policy_generation("private").unwrap(),
            meta.policy_input_generation("unknown").unwrap(),
        ),
        (
            crate::meta::PolicyInputGeneration {
                repository: 0,
                catalog: 0,
                policy: 1,
            },
            crate::meta::PolicyInputGeneration::default(),
        )
    );
}

#[test]
fn test_policy_generation_captures_repository_serial() {
    let (_dir, meta) = store();
    meta.next_serial().unwrap();

    assert_eq!(
        meta.advance_policy_generation("private").unwrap(),
        crate::meta::PolicyInputGeneration {
            repository: 1,
            catalog: 0,
            policy: 1,
        }
    );
}

#[test]
fn test_policy_decision_survives_restart() {
    let (dir, meta) = store();
    let expected = meta
        .record_policy_decision(decision("package", PolicyDecisionState::Wait, 10))
        .unwrap();
    drop(meta);

    assert_eq!(
        crate::meta::MetaStore::open_existing(dir.path().join("peryx.redb"))
            .unwrap()
            .current_policy_decision(decision("package", PolicyDecisionState::Wait, 0))
            .unwrap(),
        Some(expected)
    );
}

#[test]
fn test_policy_decision_concurrent_writes_keep_one_current_record() {
    let (_dir, meta) = store();
    let meta = Arc::new(meta);
    let threads: [_; 8] = std::array::from_fn(|evaluated_at_unix| {
        let meta = Arc::clone(&meta);
        std::thread::spawn(move || {
            meta.record_policy_decision(decision(
                "package",
                PolicyDecisionState::Allow,
                i64::try_from(evaluated_at_unix).unwrap(),
            ))
            .unwrap()
        })
    });
    let records = threads.map(|thread| thread.join().unwrap());
    let current = meta
        .current_policy_decision(decision("package", PolicyDecisionState::Allow, 0))
        .unwrap()
        .unwrap();

    assert!(records.iter().any(|record| record == &current));
}

#[test]
fn test_policy_decision_history_is_bounded() {
    let (_dir, meta) = store();
    for evaluated_at_unix in 0..20 {
        meta.record_policy_decision(decision(
            &format!("package-{evaluated_at_unix}"),
            PolicyDecisionState::Allow,
            evaluated_at_unix,
        ))
        .unwrap();
    }

    assert_eq!(
        (
            meta.query_policy_decisions(&PolicyDecisionQuery {
                limit: 100,
                ..PolicyDecisionQuery::default()
            })
            .unwrap()
            .decisions
            .len(),
            meta.current_policy_decision(decision("package-0", PolicyDecisionState::Allow, 0))
                .unwrap(),
            meta.current_policy_decision(decision("package-4", PolicyDecisionState::Allow, 0))
                .unwrap()
                .is_some(),
        ),
        (16, None, true)
    );
}

#[test]
fn test_policy_decision_retention_preserves_replaced_subject_current_record() {
    let (_dir, meta) = store();
    for evaluated_at_unix in 0..20 {
        meta.record_policy_decision(decision("package", PolicyDecisionState::Allow, evaluated_at_unix))
            .unwrap();
    }

    assert_eq!(
        meta.current_policy_decision(decision("package", PolicyDecisionState::Allow, 0))
            .unwrap()
            .unwrap()
            .evaluated_at_unix,
        19
    );
}
