use crate::{
    RetentionCandidate, RetentionClass, RetentionConfig, RetentionDecision, RetentionFrontier, RetentionOutcome,
    RetentionPolicy, RetentionSelector, RetentionSummary, RetentionVisibility,
};
use rstest::rstest;

#[test]
fn an_empty_policy_retains_every_candidate_with_no_rule() {
    let policy = RetentionPolicy::compile(&RetentionConfig::default());
    assert!(policy.is_empty());

    let decisions = policy.plan_resource(None, vec![candidate("resource", "group-a", 0)]);

    assert_eq!(
        decisions,
        [decision("resource", "group-a", RetentionOutcome::Retain, None, &[],)]
    );
}

#[test]
fn a_populated_policy_is_not_empty() {
    assert!(!expiring(RetentionSelector::Cached).is_empty());
}

#[rstest]
#[case::age(RetentionSelector::Age { older_than_seconds: 1 }, "age")]
#[case::source(RetentionSelector::Source { name: "alpha".to_owned() }, "source")]
#[case::resource_prefix(
    RetentionSelector::ResourcePrefix { prefix: "team-".to_owned() },
    "resource-prefix"
)]
#[case::keep_latest(
    RetentionSelector::KeepLatestGroups { count: 2 },
    "keep-latest-groups"
)]
#[case::cached(RetentionSelector::Cached, "cached")]
#[case::trash(RetentionSelector::Trash, "trash")]
#[case::orphan(RetentionSelector::Orphan, "orphan")]
#[case::visibility(
    RetentionSelector::Visibility { state: RetentionVisibility::Withdrawn },
    "visibility"
)]
fn each_selector_reports_its_stable_rule_name(#[case] selector: RetentionSelector, #[case] name: &str) {
    assert_eq!(selector.name(), name);
}

#[test]
fn keep_latest_protects_the_newest_groups_and_expires_the_rest() {
    let policy = RetentionPolicy::compile(&RetentionConfig {
        keep: vec![RetentionSelector::KeepLatestGroups { count: 2 }],
        expire: vec![RetentionSelector::ResourcePrefix { prefix: String::new() }],
    });

    let decisions = policy.plan_resource(
        None,
        vec![
            candidate("resource", "group-a", 2),
            candidate("resource", "group-c", 0),
            candidate("resource", "group-b", 1),
        ],
    );

    assert_eq!(
        decisions,
        vec![
            decision(
                "resource",
                "group-c",
                RetentionOutcome::Retain,
                Some("keep-latest-groups"),
                &[],
            ),
            decision(
                "resource",
                "group-b",
                RetentionOutcome::Retain,
                Some("keep-latest-groups"),
                &[],
            ),
            decision(
                "resource",
                "group-a",
                RetentionOutcome::Remove,
                Some("resource-prefix"),
                &["group-b", "group-c"],
            ),
        ]
    );
}

#[test]
fn a_keep_rule_wins_over_a_matching_expire_rule() {
    let policy = RetentionPolicy::compile(&RetentionConfig {
        keep: vec![RetentionSelector::KeepLatestGroups { count: 1 }],
        expire: vec![RetentionSelector::Cached],
    });
    let mut cached = candidate("resource", "group-a", 0);
    cached.class = RetentionClass::Cached;

    let decisions = policy.plan_resource(None, vec![cached]);

    assert_eq!(
        decisions,
        [RetentionDecision {
            class: RetentionClass::Cached,
            ..decision(
                "resource",
                "group-a",
                RetentionOutcome::Retain,
                Some("keep-latest-groups"),
                &[],
            )
        }]
    );
}

#[test]
fn a_removed_artifact_reports_its_retained_sibling_group() {
    let policy = expiring(RetentionSelector::Trash);
    let retained = candidate("resource", "group-a", 0);
    let mut removed = candidate("resource", "group-a", 0);
    removed.artifact = "resource:group-a:source".to_owned();
    removed.digest = "sha256:source".to_owned();
    removed.class = RetentionClass::Trash;

    let decisions = policy.plan_resource(None, vec![removed, retained]);

    assert_eq!(
        decisions
            .iter()
            .find(|decision| decision.outcome == RetentionOutcome::Remove)
            .unwrap()
            .retained_groups,
        ["group-a"]
    );
}

#[test]
fn a_group_is_absent_when_every_artifact_is_removed() {
    let policy = expiring(RetentionSelector::Trash);
    let mut wheel = candidate("resource", "group-a", 0);
    wheel.class = RetentionClass::Trash;
    let mut source = candidate("resource", "group-a", 0);
    source.artifact = "resource:group-a:source".to_owned();
    source.digest = "sha256:source".to_owned();
    source.class = RetentionClass::Trash;

    let decisions = policy.plan_resource(None, vec![wheel, source]);

    assert!(decisions.iter().all(|decision| decision.retained_groups.is_empty()));
}

#[test]
fn retained_groups_are_deduplicated_and_ordered() {
    let policy = expiring(RetentionSelector::Trash);
    let retained = candidate("resource", "group-z", 0);
    let mut sibling = candidate("resource", "group-z", 0);
    sibling.artifact = "resource:group-z:source".to_owned();
    sibling.digest = "sha256:source".to_owned();
    let other = candidate("resource", "group-a", 0);
    let mut removed = candidate("resource", "group-m", 0);
    removed.class = RetentionClass::Trash;

    let decisions = policy.plan_resource(None, vec![retained, sibling, other, removed]);

    assert_eq!(
        decisions
            .iter()
            .find(|decision| decision.outcome == RetentionOutcome::Remove)
            .unwrap()
            .retained_groups,
        ["group-a", "group-z"]
    );
}

#[test]
fn an_age_rule_expires_only_candidates_older_than_its_bound() {
    let policy = expiring(RetentionSelector::Age {
        older_than_seconds: 100,
    });
    let mut old = candidate("resource", "group-a", 0);
    old.upload_time_unix = Some(0);
    let mut fresh = candidate("resource", "group-b", 1);
    fresh.upload_time_unix = Some(950);

    let decisions = policy.plan_resource(Some(1_000), vec![old, fresh]);

    assert_eq!(
        decisions,
        vec![
            decision(
                "resource",
                "group-a",
                RetentionOutcome::Remove,
                Some("age"),
                &["group-b"],
            ),
            decision("resource", "group-b", RetentionOutcome::Retain, None, &[]),
        ]
    );
}

#[test]
fn an_age_rule_ages_nothing_without_a_clock_or_a_publish_time() {
    let policy = expiring(RetentionSelector::Age { older_than_seconds: 1 });
    let mut dated = candidate("resource", "group-a", 0);
    dated.upload_time_unix = Some(0);

    assert_eq!(
        policy.plan_resource(None, vec![dated]),
        [decision("resource", "group-a", RetentionOutcome::Retain, None, &[],)]
    );

    let undated = candidate("resource", "group-b", 0);
    assert_eq!(
        policy.plan_resource(Some(10_000), vec![undated]),
        [decision("resource", "group-b", RetentionOutcome::Retain, None, &[],)]
    );
}

#[test]
fn an_age_rule_does_not_expire_a_future_upload() {
    let policy = expiring(RetentionSelector::Age { older_than_seconds: 1 });
    let mut future = candidate("resource", "group-a", 0);
    future.upload_time_unix = Some(i64::MAX);

    assert_eq!(
        policy.plan_resource(Some(i64::MIN), vec![future]),
        [decision("resource", "group-a", RetentionOutcome::Retain, None, &[],)]
    );
}

#[test]
fn a_source_rule_matches_the_named_routed_source() {
    let policy = expiring(RetentionSelector::Source {
        name: "upstream".to_owned(),
    });
    let mut routed = candidate("resource", "group-a", 0);
    routed.source = Some("upstream".to_owned());
    let mut other = candidate("resource", "group-b", 1);
    other.source = Some("mirror".to_owned());

    let decisions = policy.plan_resource(None, vec![routed, other]);

    assert_eq!(
        decisions,
        vec![
            RetentionDecision {
                source: Some("upstream".to_owned()),
                ..decision(
                    "resource",
                    "group-a",
                    RetentionOutcome::Remove,
                    Some("source"),
                    &["group-b"],
                )
            },
            RetentionDecision {
                source: Some("mirror".to_owned()),
                ..decision("resource", "group-b", RetentionOutcome::Retain, None, &[])
            },
        ]
    );
}

#[test]
fn a_resource_prefix_rule_matches_by_name() {
    let policy = expiring(RetentionSelector::ResourcePrefix {
        prefix: "team-".to_owned(),
    });

    let decisions = policy.plan_resource(
        None,
        vec![
            candidate("team-tool", "group-a", 0),
            candidate("resource", "group-a", 0),
        ],
    );

    assert_eq!(
        decisions,
        vec![
            decision("resource", "group-a", RetentionOutcome::Retain, None, &[]),
            decision(
                "team-tool",
                "group-a",
                RetentionOutcome::Remove,
                Some("resource-prefix"),
                &["group-a"],
            ),
        ]
    );
}

#[test]
fn a_trash_rule_matches_soft_deleted_candidates() {
    let policy = expiring(RetentionSelector::Trash);
    let mut trashed = candidate("resource", "group-a", 0);
    trashed.class = RetentionClass::Trash;

    assert_eq!(
        policy.plan_resource(None, vec![trashed]),
        [RetentionDecision {
            class: RetentionClass::Trash,
            ..decision("resource", "group-a", RetentionOutcome::Remove, Some("trash"), &[],)
        }]
    );
    assert_eq!(
        policy.plan_resource(None, vec![candidate("resource", "group-a", 0)]),
        [decision("resource", "group-a", RetentionOutcome::Retain, None, &[],)]
    );
}

#[test]
fn an_orphan_rule_matches_unreferenced_candidates() {
    let policy = expiring(RetentionSelector::Orphan);
    let mut orphan = candidate("resource", "group-a", 0);
    orphan.orphan = true;

    assert_eq!(
        policy.plan_resource(None, vec![orphan]),
        [decision(
            "resource",
            "group-a",
            RetentionOutcome::Remove,
            Some("orphan"),
            &[],
        )]
    );
    assert_eq!(
        policy.plan_resource(None, vec![candidate("resource", "group-a", 0)]),
        [decision("resource", "group-a", RetentionOutcome::Retain, None, &[],)]
    );
}

#[test]
fn a_visibility_rule_matches_only_candidates_in_the_named_state() {
    let policy = expiring(RetentionSelector::Visibility {
        state: RetentionVisibility::Withdrawn,
    });
    let mut withdrawn = candidate("resource", "group-a", 0);
    withdrawn.visibility = RetentionVisibility::Withdrawn;

    let decisions = policy.plan_resource(None, vec![withdrawn, candidate("resource", "group-b", 1)]);

    assert_eq!(
        decisions,
        vec![
            RetentionDecision {
                visibility: RetentionVisibility::Withdrawn,
                ..decision(
                    "resource",
                    "group-a",
                    RetentionOutcome::Remove,
                    Some("visibility"),
                    &["group-b"],
                )
            },
            decision("resource", "group-b", RetentionOutcome::Retain, None, &[]),
        ]
    );
}

#[test]
fn a_visibility_keep_rule_protects_hidden_candidates_from_an_expire_sweep() {
    let policy = RetentionPolicy::compile(&RetentionConfig {
        keep: vec![RetentionSelector::Visibility {
            state: RetentionVisibility::Hidden,
        }],
        expire: vec![RetentionSelector::ResourcePrefix { prefix: String::new() }],
    });
    let mut hidden = candidate("resource", "group-a", 0);
    hidden.visibility = RetentionVisibility::Hidden;

    let decisions = policy.plan_resource(None, vec![hidden]);

    assert_eq!(
        decisions,
        [RetentionDecision {
            visibility: RetentionVisibility::Hidden,
            ..decision("resource", "group-a", RetentionOutcome::Retain, Some("visibility"), &[],)
        }]
    );
}

#[test]
fn a_cached_keep_rule_protects_cached_candidates() {
    let policy = keeping(RetentionSelector::Cached);
    let mut cached = candidate("resource", "group-a", 0);
    cached.class = RetentionClass::Cached;

    let decisions = policy.plan_resource(None, vec![cached]);

    assert_eq!(
        decisions,
        [RetentionDecision {
            class: RetentionClass::Cached,
            ..decision("resource", "group-a", RetentionOutcome::Retain, Some("cached"), &[],)
        }]
    );
    assert_eq!(serde_json::to_value(&decisions[0]).unwrap()["class"], "cached");
}

#[test]
fn decisions_order_by_rank_then_artifact_then_digest() {
    let policy = RetentionPolicy::compile(&RetentionConfig::default());
    let mut tie_a = candidate("resource", "group-a", 0);
    tie_a.artifact = "resource:group-a:variant".to_owned();
    tie_a.digest = "sha256:aaa".to_owned();
    let mut tie_b = candidate("resource", "group-a", 0);
    tie_b.artifact = "resource:group-a:variant".to_owned();
    tie_b.digest = "sha256:bbb".to_owned();

    let decisions = policy.plan_resource(None, vec![tie_b, candidate("resource", "group-b", 1), tie_a]);

    assert_eq!(
        decisions,
        vec![
            RetentionDecision {
                artifact: "resource:group-a:variant".to_owned(),
                digest: "sha256:aaa".to_owned(),
                ..decision("resource", "group-a", RetentionOutcome::Retain, None, &[])
            },
            RetentionDecision {
                artifact: "resource:group-a:variant".to_owned(),
                digest: "sha256:bbb".to_owned(),
                ..decision("resource", "group-a", RetentionOutcome::Retain, None, &[])
            },
            decision("resource", "group-b", RetentionOutcome::Retain, None, &[]),
        ]
    );
}

#[test]
fn repeating_a_plan_produces_byte_identical_output() {
    let policy = expiring(RetentionSelector::Trash);
    let mut trashed = candidate("resource", "group-a", 1);
    trashed.class = RetentionClass::Trash;
    let build = || policy.plan_resource(None, vec![candidate("resource", "group-b", 0), trashed.clone()]);

    let expected = concat!(
        r#"[{"resource":"resource","group":"group-b","artifact":"resource:group-b","digest":"sha256:resourcegroup-b","class":"hosted","visibility":"active","bytes":10,"outcome":"retain"},"#,
        r#"{"resource":"resource","group":"group-a","artifact":"resource:group-a","digest":"sha256:resourcegroup-a","class":"trash","visibility":"active","bytes":10,"outcome":"remove","rule":"trash","retained_groups":["group-b"]}]"#,
    );

    assert_eq!(
        (
            serde_json::to_string(&build()).unwrap(),
            serde_json::to_string(&build()).unwrap(),
        ),
        (expected.to_owned(), expected.to_owned())
    );
}

#[test]
fn a_removal_decision_serializes_every_recorded_field() {
    let policy = expiring(RetentionSelector::Trash);
    let mut trashed = candidate("resource", "group-a", 1);
    trashed.class = RetentionClass::Trash;
    trashed.visibility = RetentionVisibility::Withdrawn;
    trashed.source = Some("upstream".to_owned());

    let decisions = policy.plan_resource(None, vec![candidate("resource", "group-b", 0), trashed]);
    let removed = decisions
        .iter()
        .find(|decision| decision.outcome == RetentionOutcome::Remove)
        .unwrap();

    let json = serde_json::to_value(removed).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "resource": "resource",
            "group": "group-a",
            "artifact": "resource:group-a",
            "digest": "sha256:resourcegroup-a",
            "class": "trash",
            "visibility": "withdrawn",
            "source": "upstream",
            "bytes": 10,
            "outcome": "remove",
            "rule": "trash",
            "retained_groups": ["group-b"],
        })
    );
}

#[test]
fn a_hidden_generated_candidate_serializes_its_class_and_visibility() {
    let policy = RetentionPolicy::compile(&RetentionConfig::default());
    let mut generated = candidate("resource", "group-a", 0);
    generated.class = RetentionClass::Generated;
    generated.visibility = RetentionVisibility::Hidden;

    let json = serde_json::to_value(&policy.plan_resource(None, vec![generated])[0]).unwrap();

    assert_eq!(
        json,
        serde_json::json!({
            "resource": "resource",
            "group": "group-a",
            "artifact": "resource:group-a",
            "digest": "sha256:resourcegroup-a",
            "class": "generated",
            "visibility": "hidden",
            "bytes": 10,
            "outcome": "retain",
        })
    );
}

#[test]
fn equal_rules_compile_to_one_version_and_distinct_rules_diverge() {
    let all = RetentionConfig {
        keep: vec![
            RetentionSelector::Age { older_than_seconds: 30 },
            RetentionSelector::Source {
                name: "alpha".to_owned(),
            },
            RetentionSelector::ResourcePrefix {
                prefix: "team-".to_owned(),
            },
            RetentionSelector::KeepLatestGroups { count: 5 },
            RetentionSelector::Cached,
            RetentionSelector::Trash,
            RetentionSelector::Orphan,
            RetentionSelector::Visibility {
                state: RetentionVisibility::Withdrawn,
            },
        ],
        expire: vec![RetentionSelector::Orphan],
    };

    assert_eq!(
        RetentionPolicy::compile(&all).version(),
        RetentionPolicy::compile(&all).version()
    );
    assert_ne!(
        RetentionPolicy::compile(&all).version(),
        RetentionPolicy::compile(&RetentionConfig::default()).version()
    );
    assert_ne!(
        keeping(RetentionSelector::Orphan).version(),
        expiring(RetentionSelector::Orphan).version()
    );
    assert_ne!(
        keeping(RetentionSelector::Visibility {
            state: RetentionVisibility::Withdrawn,
        })
        .version(),
        keeping(RetentionSelector::Visibility {
            state: RetentionVisibility::Hidden,
        })
        .version()
    );
    assert_ne!(
        keeping(RetentionSelector::Source {
            name: "a|source:b".to_owned(),
        })
        .version(),
        RetentionPolicy::compile(&RetentionConfig {
            keep: vec![
                RetentionSelector::Source { name: "a".to_owned() },
                RetentionSelector::Source { name: "b".to_owned() },
            ],
            expire: Vec::new(),
        })
        .version()
    );
}

#[test]
fn policy_versions_are_stable() {
    assert_eq!(keeping(RetentionSelector::Cached).version(), 0xd77a_0944_ef4d_2cfd);
}

#[test]
fn a_config_deserializes_every_selector_from_json() {
    let config: RetentionConfig = serde_json::from_str(
        r#"{
            "keep": [
                {"selector": "age", "older_than_seconds": 86400},
                {"selector": "source", "name": "alpha"},
                {"selector": "resource-prefix", "prefix": "acme-"},
                {"selector": "keep-latest-groups", "count": 5},
                {"selector": "cached"}
            ],
            "expire": [
                {"selector": "trash"},
                {"selector": "orphan"},
                {"selector": "visibility", "state": "withdrawn"}
            ]
        }"#,
    )
    .unwrap();

    assert_eq!(
        config,
        RetentionConfig {
            keep: vec![
                RetentionSelector::Age {
                    older_than_seconds: 86_400
                },
                RetentionSelector::Source {
                    name: "alpha".to_owned()
                },
                RetentionSelector::ResourcePrefix {
                    prefix: "acme-".to_owned()
                },
                RetentionSelector::KeepLatestGroups { count: 5 },
                RetentionSelector::Cached,
            ],
            expire: vec![
                RetentionSelector::Trash,
                RetentionSelector::Orphan,
                RetentionSelector::Visibility {
                    state: RetentionVisibility::Withdrawn,
                },
            ],
        }
    );
}

#[rstest]
#[case::section(r#"{"keeep":[{"selector":"cached"}],"expire":[{"selector":"trash"}]}"#, "keeep")]
#[case::selector_property(
    r#"{"keep":[{"selector":"age","older_than_seconds":86400,"older_than_second":1}],"expire":[{"selector":"trash"}]}"#,
    "older_than_second"
)]
fn a_config_rejects_an_unknown_key(#[case] input: &str, #[case] key: &str) {
    assert!(
        serde_json::from_str::<RetentionConfig>(input)
            .unwrap_err()
            .to_string()
            .contains(&format!("unknown field `{key}`"))
    );
}

#[test]
fn a_config_rejects_a_negative_age() {
    assert!(
        serde_json::from_str::<RetentionConfig>(r#"{"expire":[{"selector":"age","older_than_seconds":-1}]}"#).is_err()
    );
}

#[test]
fn a_summary_serializes_the_policy_version_and_metadata_frontier() {
    let summary = RetentionSummary {
        policy_version: keeping(RetentionSelector::Cached).version(),
        frontier: RetentionFrontier {
            repository: 7,
            catalog: 3,
            policy: 2,
        },
    };

    assert_eq!(
        serde_json::to_value(summary).unwrap(),
        serde_json::json!({
            "policy_version": summary.policy_version,
            "frontier": {"repository": 7, "catalog": 3, "policy": 2},
        })
    );
}

fn candidate(resource: &str, group: &str, rank: u64) -> RetentionCandidate {
    RetentionCandidate {
        resource: resource.to_owned(),
        group: Some(group.to_owned()),
        artifact: format!("{resource}:{group}"),
        digest: format!("sha256:{resource}{group}"),
        class: RetentionClass::Hosted,
        visibility: RetentionVisibility::Active,
        source: None,
        bytes: 10,
        upload_time_unix: None,
        rank,
        orphan: false,
    }
}

fn decision(
    resource: &str,
    group: &str,
    outcome: RetentionOutcome,
    rule: Option<&'static str>,
    retained_groups: &[&str],
) -> RetentionDecision {
    RetentionDecision {
        resource: resource.to_owned(),
        group: Some(group.to_owned()),
        artifact: format!("{resource}:{group}"),
        digest: format!("sha256:{resource}{group}"),
        class: RetentionClass::Hosted,
        visibility: RetentionVisibility::Active,
        source: None,
        bytes: 10,
        outcome,
        rule,
        retained_groups: retained_groups.iter().map(|group| (*group).to_owned()).collect(),
    }
}

fn keeping(selector: RetentionSelector) -> RetentionPolicy {
    RetentionPolicy::compile(&RetentionConfig {
        keep: vec![selector],
        expire: Vec::new(),
    })
}

fn expiring(selector: RetentionSelector) -> RetentionPolicy {
    RetentionPolicy::compile(&RetentionConfig {
        keep: Vec::new(),
        expire: vec![selector],
    })
}
