use std::collections::HashMap;
use std::sync::Arc;

use crate::meta::{
    NewPolicyDecision, PolicyDecisionItem, PolicyDecisionQuery, PolicyDecisionQueryError, PolicyDecisionStoreError,
};
use peryx_policy::{PolicyAction, PolicyDecisionState};
use rstest::rstest;

use super::store;

fn decision(resource: &str, state: PolicyDecisionState, evaluated_at_unix: i64) -> NewPolicyDecision<'_> {
    NewPolicyDecision {
        repository: "private",
        resource,
        group: Some("1.0"),
        artifact: Some("artifact-1.0.bin"),
        source: Some("alpha"),
        action: PolicyAction::Serve,
        state,
        rule: (state == PolicyDecisionState::Deny).then_some("blocked-resource"),
        reason: (state == PolicyDecisionState::Deny).then_some("resource is blocked"),
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

fn write_row(meta: &crate::meta::MetaStore, repository: &str, key: &str, value: &[u8]) {
    meta.commit_driver_txn(|txn| {
        txn.touch_policy_inputs(repository);
        txn.put(key, value)?;
        Ok::<_, crate::meta::MetaError>(((), vec![b"replicated".to_vec()]))
    })
    .unwrap();
}

#[test]
fn test_policy_decision_replaces_current_and_retains_history() {
    let (_dir, meta) = store();
    publish_catalog(&meta, 8);
    meta.advance_policy_generation("private").unwrap();
    let denied = meta
        .record_policy_decision(decision("resource", PolicyDecisionState::Deny, 10))
        .unwrap();
    let allowed = meta
        .record_policy_decision(decision("resource", PolicyDecisionState::Allow, 11))
        .unwrap();

    assert_eq!(
        (
            meta.current_policy_decision(decision("resource", PolicyDecisionState::Allow, 0))
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
    write_row(&meta, "private", "pypi/private/one", b"one");
    meta.record_policy_decision(decision("resource", PolicyDecisionState::Allow, 10))
        .unwrap();
    write_row(&meta, "private", "pypi/private/two", b"two");

    assert_eq!(
        (
            meta.current_policy_decision(decision("resource", PolicyDecisionState::Allow, 0))
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
    meta.record_policy_decision(decision("resource", PolicyDecisionState::Allow, 10))
        .unwrap();
    publish_catalog(&meta, 2);

    assert_eq!(
        meta.current_policy_decision(decision("resource", PolicyDecisionState::Allow, 0))
            .unwrap(),
        None
    );
}

#[test]
fn test_policy_decision_policy_change_makes_current_stale() {
    let (_dir, meta) = store();
    meta.advance_policy_generation("private").unwrap();
    meta.record_policy_decision(decision("resource", PolicyDecisionState::Allow, 10))
        .unwrap();
    meta.advance_policy_generation("private").unwrap();

    assert_eq!(
        meta.current_policy_decision(decision("resource", PolicyDecisionState::Allow, 0))
            .unwrap(),
        None
    );
}

#[test]
fn test_policy_decision_catalog_publication_is_atomic_with_driver_rows() {
    let (_dir, meta) = store();
    publish_catalog(&meta, 1);
    meta.advance_policy_generation("private").unwrap();
    meta.record_policy_decision(decision("resource", PolicyDecisionState::Allow, 10))
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
            meta.current_policy_decision(decision("resource", PolicyDecisionState::Allow, 0))
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
fn test_policy_catalog_publication_leaves_the_repository_revision_alone() {
    let (_dir, meta) = store();
    write_row(&meta, "private", "pypi/private/one", b"one");
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
fn test_policy_decision_stays_fresh_when_another_repository_changes() {
    let (_dir, meta) = store();
    meta.advance_policy_generation("private").unwrap();
    let recorded = meta
        .record_policy_decision(decision("resource", PolicyDecisionState::Allow, 10))
        .unwrap();

    write_row(&meta, "other", "pypi/other/one", b"one");

    assert_eq!(
        (
            meta.current_policy_decision(decision("resource", PolicyDecisionState::Allow, 0))
                .unwrap(),
            meta.policy_input_generation("private").unwrap(),
            meta.policy_input_generation("other").unwrap(),
        ),
        (
            Some(recorded),
            crate::meta::PolicyInputGeneration {
                repository: 0,
                catalog: 0,
                policy: 1,
            },
            crate::meta::PolicyInputGeneration {
                repository: 1,
                catalog: 0,
                policy: 0,
            },
        )
    );
}

#[test]
fn test_policy_decision_history_marks_only_the_changed_repository_stale() {
    let (_dir, meta) = store();
    meta.record_policy_decision(decision("resource", PolicyDecisionState::Allow, 10))
        .unwrap();
    meta.record_policy_decision(NewPolicyDecision {
        repository: "other",
        ..decision("resource", PolicyDecisionState::Allow, 11)
    })
    .unwrap();

    write_row(&meta, "other", "pypi/other/one", b"one");

    assert_eq!(
        meta.query_policy_decisions(&PolicyDecisionQuery {
            limit: 10,
            ..PolicyDecisionQuery::default()
        })
        .unwrap()
        .decisions
        .into_iter()
        .map(|item| (item.record.repository, item.fresh))
        .collect::<Vec<_>>(),
        vec![("other".to_owned(), false), ("private".to_owned(), true)]
    );
}

#[test]
fn test_policy_decision_artifact_lookup_ignores_another_repository_change() {
    let (_dir, meta) = store();
    let recorded = meta
        .record_policy_decision(decision("resource", PolicyDecisionState::Allow, 10))
        .unwrap();

    write_row(&meta, "other", "pypi/other/one", b"one");

    assert_eq!(
        meta.current_policy_decisions_for_artifacts("private", "resource", &["artifact-1.0.bin"])
            .unwrap(),
        HashMap::from([(
            "artifact-1.0.bin".to_owned(),
            PolicyDecisionItem {
                record: recorded,
                fresh: true
            }
        )])
    );
}

#[test]
fn test_policy_repository_revision_rolls_back_with_its_transaction() {
    let (_dir, meta) = store();
    write_row(&meta, "private", "pypi/private/one", b"one");

    let result = meta.commit_driver_txn(|txn| {
        txn.touch_policy_inputs("private");
        txn.put("pypi/private/two", b"two")?;
        Err::<((), Vec<Vec<u8>>), _>(crate::meta::MetaError::DriverPrecondition("failed".to_owned()))
    });

    assert_eq!(
        (
            result.is_err(),
            meta.get_driver_value("pypi/private/two").unwrap(),
            meta.policy_input_generation("private").unwrap().repository,
        ),
        (true, None, 1)
    );
}

#[test]
fn test_policy_repository_revisions_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = crate::meta::MetaStore::open(&path).unwrap();
    write_row(&meta, "private", "pypi/private/one", b"one");
    write_row(&meta, "other", "pypi/other/one", b"one");
    write_row(&meta, "other", "pypi/other/two", b"two");
    meta.commit_driver_txn(|txn| {
        txn.touch_policy_inputs("private");
        Err::<((), Vec<Vec<u8>>), _>(crate::meta::MetaError::DriverPrecondition("interrupted".to_owned()))
    })
    .unwrap_err();
    drop(meta);

    let meta = crate::meta::MetaStore::open(&path).unwrap();

    assert_eq!(
        (
            meta.policy_input_generation("private").unwrap().repository,
            meta.policy_input_generation("other").unwrap().repository,
        ),
        (1, 2)
    );
}

#[test]
fn test_advance_repository_generations_moves_only_the_named_repositories() {
    let (_dir, meta) = store();
    write_row(&meta, "private", "pypi/private/one", b"one");
    write_row(&meta, "other", "pypi/other/one", b"one");

    meta.advance_repository_generations(&std::collections::BTreeSet::from(["other".to_owned()]))
        .unwrap();

    assert_eq!(
        (
            meta.policy_input_generation("private").unwrap().repository,
            meta.policy_input_generation("other").unwrap().repository,
        ),
        (1, 2)
    );
}

#[test]
fn test_advance_repository_generations_accepts_an_empty_set() {
    let (_dir, meta) = store();
    write_row(&meta, "private", "pypi/private/one", b"one");

    meta.advance_repository_generations(&std::collections::BTreeSet::new())
        .unwrap();

    assert_eq!(meta.policy_input_generation("private").unwrap().repository, 1);
}

#[test]
fn test_policy_decision_query_filters_and_paginates() {
    let (_dir, meta) = store();
    for (resource, state, evaluated_at_unix) in [
        ("alpha", PolicyDecisionState::Allow, 10),
        ("beta", PolicyDecisionState::Deny, 20),
        ("gamma", PolicyDecisionState::Deny, 30),
    ] {
        meta.record_policy_decision(decision(resource, state, evaluated_at_unix))
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
                .map(|item| item.record.resource.as_str())
                .collect::<Vec<_>>(),
            first.next_cursor.is_some(),
            second
                .decisions
                .iter()
                .map(|item| item.record.resource.as_str())
                .collect::<Vec<_>>(),
            second.next_cursor,
        ),
        (vec!["gamma"], true, vec!["beta"], None)
    );
}

#[test]
fn test_policy_decision_query_applies_the_inclusive_upper_time_bound() {
    let (_dir, meta) = store();
    for evaluated_at_unix in [10, 20, 30] {
        meta.record_policy_decision(decision(
            &format!("resource-{evaluated_at_unix}"),
            PolicyDecisionState::Allow,
            evaluated_at_unix,
        ))
        .unwrap();
    }
    assert_eq!(
        meta.query_policy_decisions(&PolicyDecisionQuery {
            evaluated_to_unix: Some(20),
            limit: 10,
            ..PolicyDecisionQuery::default()
        })
        .unwrap()
        .decisions
        .into_iter()
        .map(|decision| decision.record.evaluated_at_unix)
        .collect::<Vec<_>>(),
        vec![20, 10]
    );
}

#[test]
fn test_policy_decision_query_scopes_to_one_resource() {
    let (_dir, meta) = store();
    for (resource, state, evaluated_at_unix) in [
        ("alpha", PolicyDecisionState::Deny, 10),
        ("beta", PolicyDecisionState::Deny, 20),
        ("alpha", PolicyDecisionState::Allow, 30),
    ] {
        meta.record_policy_decision(decision(resource, state, evaluated_at_unix))
            .unwrap();
    }

    let scoped = meta
        .query_policy_decisions(&PolicyDecisionQuery {
            resource: Some("alpha".to_owned()),
            limit: 10,
            ..PolicyDecisionQuery::default()
        })
        .unwrap();

    assert_eq!(
        scoped
            .decisions
            .iter()
            .map(|item| (item.record.resource.as_str(), item.record.state))
            .collect::<Vec<_>>(),
        vec![
            ("alpha", PolicyDecisionState::Allow),
            ("alpha", PolicyDecisionState::Deny)
        ]
    );
}

#[test]
fn test_policy_decision_artifact_batch_uses_current_records() {
    let (_dir, meta) = store();
    let mut wanted = decision("project", PolicyDecisionState::Deny, 10);
    wanted.artifact = Some("wanted.whl");
    meta.record_policy_decision(wanted).unwrap();
    let mut cached = decision("project", PolicyDecisionState::Allow, 11);
    cached.artifact = Some("wanted.whl");
    cached.action = PolicyAction::Cached;
    let expected = meta.record_policy_decision(cached).unwrap();
    for index in 0..101 {
        let artifact = format!("unrelated-{index}.whl");
        let mut unrelated = decision("project", PolicyDecisionState::Allow, 20 + index);
        unrelated.artifact = Some(&artifact);
        meta.record_policy_decision(unrelated).unwrap();
    }
    let mut upload = decision("project", PolicyDecisionState::Allow, 200);
    upload.artifact = Some("upload-only.whl");
    upload.action = PolicyAction::Upload;
    meta.record_policy_decision(upload).unwrap();

    assert_eq!(
        meta.current_policy_decisions_for_artifacts(
            "private",
            "project",
            &["wanted.whl", "upload-only.whl", "missing.whl"],
        )
        .unwrap(),
        HashMap::from([(
            "wanted.whl".to_owned(),
            PolicyDecisionItem {
                record: expected,
                fresh: true,
            },
        )])
    );
}

#[test]
fn test_policy_decision_artifact_batch_keeps_stale_records() {
    let (_dir, meta) = store();
    let mut candidate = decision("project", PolicyDecisionState::Allow, 10);
    candidate.artifact = Some("stale.whl");
    let expected = meta.record_policy_decision(candidate).unwrap();
    write_row(&meta, "private", "pypi/private/one", b"one");

    assert_eq!(
        meta.current_policy_decisions_for_artifacts("private", "project", &["stale.whl"])
            .unwrap(),
        HashMap::from([(
            "stale.whl".to_owned(),
            PolicyDecisionItem {
                record: expected,
                fresh: false,
            },
        )])
    );
}

#[rstest]
#[case::repository("x".repeat(513), "project".to_owned(), vec!["artifact.whl".to_owned()], "repository")]
#[case::resource("private".to_owned(), "x".repeat(513), vec!["artifact.whl".to_owned()], "resource")]
#[case::artifact("private".to_owned(), "project".to_owned(), vec!["x".repeat(513)], "artifact")]
fn test_policy_decision_artifact_batch_bounds_subjects(
    #[case] repository: String,
    #[case] resource: String,
    #[case] artifacts: Vec<String>,
    #[case] field: &str,
) {
    let (_dir, meta) = store();
    let artifacts = artifacts.iter().map(String::as_str).collect::<Vec<_>>();

    assert!(matches!(
        meta.current_policy_decisions_for_artifacts(&repository, &resource, &artifacts),
        Err(PolicyDecisionStoreError::FieldTooLong { field: actual, max: 512 }) if actual == field
    ));
}

#[test]
fn test_policy_decision_artifact_batch_bounds_count() {
    let (_dir, meta) = store();

    assert!(matches!(
        meta.current_policy_decisions_for_artifacts("private", "project", &["artifact.whl"; 101]),
        Err(PolicyDecisionStoreError::TooManyArtifacts { max: 100 })
    ));
}

#[test]
fn test_policy_decision_artifact_batch_accepts_an_empty_set() {
    let (_dir, meta) = store();

    assert_eq!(
        meta.current_policy_decisions_for_artifacts("private", "project", &[])
            .unwrap(),
        HashMap::new()
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
    candidate.resource = &bounded;
    candidate.rule = Some(&bounded);
    candidate.source = Some(&bounded);
    let expected = meta.record_policy_decision(candidate).unwrap();

    for query in [
        PolicyDecisionQuery {
            repository: Some(bounded.clone()),
            ..PolicyDecisionQuery::default()
        },
        PolicyDecisionQuery {
            resource: Some(bounded.clone()),
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
            "resource",
            PolicyDecisionQuery {
                resource: Some(oversized.clone()),
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
    let mut oversized = decision("resource", PolicyDecisionState::Deny, 10);
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
        Resource,
        Group,
        Artifact,
        Source,
        Rule,
    }

    let (_dir, meta) = store();
    let oversized = "x".repeat(513);
    for (field, name) in [
        (Field::Repository, "repository"),
        (Field::Resource, "resource"),
        (Field::Group, "group"),
        (Field::Artifact, "artifact"),
        (Field::Source, "source"),
        (Field::Rule, "rule"),
    ] {
        let mut candidate = decision("resource", PolicyDecisionState::Allow, 10);
        match field {
            Field::Repository => candidate.repository = &oversized,
            Field::Resource => candidate.resource = &oversized,
            Field::Group => candidate.group = Some(&oversized),
            Field::Artifact => candidate.artifact = Some(&oversized),
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
fn test_policy_generation_keeps_the_repository_revision() {
    let (_dir, meta) = store();
    write_row(&meta, "private", "pypi/private/one", b"one");

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
        .record_policy_decision(decision("resource", PolicyDecisionState::Wait, 10))
        .unwrap();
    let persisted = serde_json::to_value(&expected).unwrap();
    drop(meta);

    assert_eq!(
        (
            persisted
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            persisted.get("resource").and_then(serde_json::Value::as_str),
            persisted.get("group").and_then(serde_json::Value::as_str),
            persisted.get("artifact").and_then(serde_json::Value::as_str),
            crate::meta::MetaStore::open_existing(dir.path().join("peryx.redb"))
                .unwrap()
                .current_policy_decision(decision("resource", PolicyDecisionState::Wait, 0))
                .unwrap(),
        ),
        (
            std::collections::BTreeSet::from([
                "action",
                "artifact",
                "evaluated_at_unix",
                "group",
                "id",
                "input_generation",
                "next_eligible_at_unix",
                "reason",
                "repository",
                "resource",
                "rule",
                "source",
                "state",
            ]),
            Some("resource"),
            Some("1.0"),
            Some("artifact-1.0.bin"),
            Some(expected),
        )
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
                "resource",
                PolicyDecisionState::Allow,
                i64::try_from(evaluated_at_unix).unwrap(),
            ))
            .unwrap()
        })
    });
    let records = threads.map(|thread| thread.join().unwrap());
    let current = meta
        .current_policy_decision(decision("resource", PolicyDecisionState::Allow, 0))
        .unwrap()
        .unwrap();

    assert!(records.iter().any(|record| record == &current));
}
