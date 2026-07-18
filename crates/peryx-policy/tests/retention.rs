use peryx_policy::{
    RetentionCandidate, RetentionClass, RetentionConfig, RetentionFrontier, RetentionOutcome, RetentionPolicy,
    RetentionSelector, RetentionSummary, RetentionVisibility,
};

fn candidate(project: &str, version: &str, rank: u64) -> RetentionCandidate {
    RetentionCandidate {
        project: project.to_owned(),
        version: Some(version.to_owned()),
        artifact: format!("{project}-{version}.whl"),
        digest: format!("sha256:{project}{version}"),
        class: RetentionClass::Hosted,
        visibility: RetentionVisibility::Active,
        source: None,
        bytes: 10,
        upload_time_unix: None,
        rank,
        orphan: false,
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

fn outcomes(decisions: &[peryx_policy::RetentionDecision]) -> Vec<(&str, RetentionOutcome, Option<&str>)> {
    decisions
        .iter()
        .map(|decision| (decision.artifact.as_str(), decision.outcome, decision.rule))
        .collect()
}

#[test]
fn an_empty_policy_retains_every_candidate_with_no_rule() {
    let policy = RetentionPolicy::compile(&RetentionConfig::default());
    assert!(policy.is_empty());

    let decisions = policy.plan_project(None, vec![candidate("flask", "1.0", 0)]);

    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].outcome, RetentionOutcome::Retain);
    assert_eq!(decisions[0].rule, None);
    assert!(decisions[0].retained_alternatives.is_empty());
}

#[test]
fn a_populated_policy_is_not_empty() {
    assert!(!expiring(RetentionSelector::Cached).is_empty());
}

#[test]
fn each_selector_reports_its_stable_rule_name() {
    for (selector, name) in [
        (RetentionSelector::Age { older_than_seconds: 1 }, "age"),
        (
            RetentionSelector::Source {
                name: "pypi".to_owned(),
            },
            "source",
        ),
        (
            RetentionSelector::ProjectPrefix {
                prefix: "acme-".to_owned(),
            },
            "project-prefix",
        ),
        (RetentionSelector::KeepLatest { count: 2 }, "keep-latest"),
        (RetentionSelector::Cached, "cached"),
        (RetentionSelector::Trash, "trash"),
        (RetentionSelector::Orphan, "orphan"),
    ] {
        assert_eq!(selector.name(), name);
    }
}

#[test]
fn keep_latest_protects_the_newest_versions_and_expires_the_rest() {
    let policy = RetentionPolicy::compile(&RetentionConfig {
        keep: vec![RetentionSelector::KeepLatest { count: 2 }],
        expire: vec![RetentionSelector::ProjectPrefix { prefix: String::new() }],
    });

    let decisions = policy.plan_project(
        None,
        vec![
            candidate("flask", "1.0", 2),
            candidate("flask", "3.0", 0),
            candidate("flask", "2.0", 1),
        ],
    );

    assert_eq!(
        outcomes(&decisions),
        vec![
            ("flask-3.0.whl", RetentionOutcome::Retain, Some("keep-latest")),
            ("flask-2.0.whl", RetentionOutcome::Retain, Some("keep-latest")),
            ("flask-1.0.whl", RetentionOutcome::Remove, Some("project-prefix")),
        ]
    );
}

#[test]
fn a_keep_rule_wins_over_a_matching_expire_rule() {
    let policy = RetentionPolicy::compile(&RetentionConfig {
        keep: vec![RetentionSelector::KeepLatest { count: 1 }],
        expire: vec![RetentionSelector::Cached],
    });
    let mut cached = candidate("flask", "1.0", 0);
    cached.class = RetentionClass::Cached;

    let decisions = policy.plan_project(None, vec![cached]);

    assert_eq!(decisions[0].outcome, RetentionOutcome::Retain);
    assert_eq!(decisions[0].rule, Some("keep-latest"));
}

#[test]
fn a_removal_lists_the_surviving_versions_of_its_project_as_alternatives() {
    let policy = RetentionPolicy::compile(&RetentionConfig {
        keep: vec![RetentionSelector::KeepLatest { count: 2 }],
        expire: vec![RetentionSelector::ProjectPrefix { prefix: String::new() }],
    });

    let decisions = policy.plan_project(
        None,
        vec![
            candidate("flask", "1.0", 2),
            candidate("flask", "3.0", 0),
            candidate("flask", "2.0", 1),
        ],
    );

    let removed = decisions
        .iter()
        .find(|decision| decision.outcome == RetentionOutcome::Remove)
        .unwrap();
    assert_eq!(removed.retained_alternatives, vec!["2.0".to_owned(), "3.0".to_owned()]);
}

#[test]
fn an_age_rule_expires_only_candidates_older_than_its_bound() {
    let policy = expiring(RetentionSelector::Age {
        older_than_seconds: 100,
    });
    let mut old = candidate("flask", "1.0", 0);
    old.upload_time_unix = Some(0);
    let mut fresh = candidate("flask", "2.0", 1);
    fresh.upload_time_unix = Some(950);

    let decisions = policy.plan_project(Some(1_000), vec![old, fresh]);

    assert_eq!(
        outcomes(&decisions),
        vec![
            ("flask-1.0.whl", RetentionOutcome::Remove, Some("age")),
            ("flask-2.0.whl", RetentionOutcome::Retain, None),
        ]
    );
}

#[test]
fn an_age_rule_ages_nothing_without_a_clock_or_a_publish_time() {
    let policy = expiring(RetentionSelector::Age { older_than_seconds: 1 });
    let mut dated = candidate("flask", "1.0", 0);
    dated.upload_time_unix = Some(0);

    let without_clock = policy.plan_project(None, vec![dated]);
    assert_eq!(without_clock[0].outcome, RetentionOutcome::Retain);

    let undated = candidate("flask", "2.0", 0);
    let without_time = policy.plan_project(Some(10_000), vec![undated]);
    assert_eq!(without_time[0].outcome, RetentionOutcome::Retain);
}

#[test]
fn a_source_rule_matches_the_named_routed_source() {
    let policy = expiring(RetentionSelector::Source {
        name: "upstream".to_owned(),
    });
    let mut routed = candidate("flask", "1.0", 0);
    routed.source = Some("upstream".to_owned());
    let mut other = candidate("flask", "2.0", 1);
    other.source = Some("mirror".to_owned());

    let decisions = policy.plan_project(None, vec![routed, other]);

    assert_eq!(
        outcomes(&decisions),
        vec![
            ("flask-1.0.whl", RetentionOutcome::Remove, Some("source")),
            ("flask-2.0.whl", RetentionOutcome::Retain, None),
        ]
    );
}

#[test]
fn a_project_prefix_rule_matches_by_name() {
    let policy = expiring(RetentionSelector::ProjectPrefix {
        prefix: "acme-".to_owned(),
    });

    let decisions = policy.plan_project(
        None,
        vec![candidate("acme-tool", "1.0", 0), candidate("flask", "1.0", 0)],
    );

    assert_eq!(
        outcomes(&decisions),
        vec![
            ("acme-tool-1.0.whl", RetentionOutcome::Remove, Some("project-prefix")),
            ("flask-1.0.whl", RetentionOutcome::Retain, None),
        ]
    );
}

#[test]
fn a_trash_rule_matches_soft_deleted_candidates() {
    let policy = expiring(RetentionSelector::Trash);
    let mut trashed = candidate("flask", "1.0", 0);
    trashed.class = RetentionClass::Trash;

    assert_eq!(
        policy.plan_project(None, vec![trashed])[0].outcome,
        RetentionOutcome::Remove
    );
    assert_eq!(
        policy.plan_project(None, vec![candidate("flask", "1.0", 0)])[0].outcome,
        RetentionOutcome::Retain
    );
}

#[test]
fn an_orphan_rule_matches_unreferenced_candidates() {
    let policy = expiring(RetentionSelector::Orphan);
    let mut orphan = candidate("flask", "1.0", 0);
    orphan.orphan = true;

    assert_eq!(
        policy.plan_project(None, vec![orphan])[0].outcome,
        RetentionOutcome::Remove
    );
    assert_eq!(
        policy.plan_project(None, vec![candidate("flask", "1.0", 0)])[0].outcome,
        RetentionOutcome::Retain
    );
}

#[test]
fn a_cached_keep_rule_protects_cached_candidates() {
    let policy = keeping(RetentionSelector::Cached);
    let mut cached = candidate("flask", "1.0", 0);
    cached.class = RetentionClass::Cached;

    let decisions = policy.plan_project(None, vec![cached]);

    assert_eq!(decisions[0].outcome, RetentionOutcome::Retain);
    assert_eq!(decisions[0].rule, Some("cached"));
    assert_eq!(serde_json::to_value(&decisions[0]).unwrap()["class"], "cached");
}

#[test]
fn decisions_order_by_rank_then_artifact_then_digest() {
    let policy = RetentionPolicy::compile(&RetentionConfig::default());
    let mut tie_a = candidate("flask", "1.0", 0);
    tie_a.artifact = "flask-1.0-py3.whl".to_owned();
    tie_a.digest = "sha256:aaa".to_owned();
    let mut tie_b = candidate("flask", "1.0", 0);
    tie_b.artifact = "flask-1.0-py3.whl".to_owned();
    tie_b.digest = "sha256:bbb".to_owned();

    let decisions = policy.plan_project(None, vec![tie_b, candidate("flask", "2.0", 1), tie_a]);

    assert_eq!(
        decisions
            .iter()
            .map(|decision| decision.digest.clone())
            .collect::<Vec<_>>(),
        vec!["sha256:aaa", "sha256:bbb", "sha256:flask2.0"]
    );
}

#[test]
fn repeating_a_plan_produces_byte_identical_output() {
    let policy = expiring(RetentionSelector::Trash);
    let mut trashed = candidate("flask", "1.0", 1);
    trashed.class = RetentionClass::Trash;
    let build = || policy.plan_project(None, vec![candidate("flask", "2.0", 0), trashed.clone()]);

    assert_eq!(
        serde_json::to_string(&build()).unwrap(),
        serde_json::to_string(&build()).unwrap()
    );
}

#[test]
fn a_removal_decision_serializes_every_recorded_field() {
    let policy = expiring(RetentionSelector::Trash);
    let mut trashed = candidate("flask", "1.0", 1);
    trashed.class = RetentionClass::Trash;
    trashed.visibility = RetentionVisibility::Yanked;
    trashed.source = Some("upstream".to_owned());

    let decisions = policy.plan_project(None, vec![candidate("flask", "2.0", 0), trashed]);
    let removed = decisions
        .iter()
        .find(|decision| decision.outcome == RetentionOutcome::Remove)
        .unwrap();

    let json = serde_json::to_value(removed).unwrap();
    assert_eq!(json["outcome"], "remove");
    assert_eq!(json["rule"], "trash");
    assert_eq!(json["class"], "trash");
    assert_eq!(json["visibility"], "yanked");
    assert_eq!(json["source"], "upstream");
    assert_eq!(json["bytes"], 10);
    assert_eq!(json["retained_alternatives"], serde_json::json!(["2.0"]));
}

#[test]
fn a_hidden_generated_candidate_serializes_its_class_and_visibility() {
    let policy = RetentionPolicy::compile(&RetentionConfig::default());
    let mut generated = candidate("flask", "1.0", 0);
    generated.class = RetentionClass::Generated;
    generated.visibility = RetentionVisibility::Hidden;

    let json = serde_json::to_value(&policy.plan_project(None, vec![generated])[0]).unwrap();

    assert_eq!(json["class"], "generated");
    assert_eq!(json["visibility"], "hidden");
    assert_eq!(json.get("rule"), None);
    assert_eq!(json.get("retained_alternatives"), None);
}

#[test]
fn equal_rules_compile_to_one_version_and_distinct_rules_diverge() {
    let all = RetentionConfig {
        keep: vec![
            RetentionSelector::Age { older_than_seconds: 30 },
            RetentionSelector::Source {
                name: "pypi".to_owned(),
            },
            RetentionSelector::ProjectPrefix {
                prefix: "acme-".to_owned(),
            },
            RetentionSelector::KeepLatest { count: 5 },
            RetentionSelector::Cached,
            RetentionSelector::Trash,
            RetentionSelector::Orphan,
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
}

#[test]
fn a_config_deserializes_every_selector_from_json() {
    let config: RetentionConfig = serde_json::from_str(
        r#"{
            "keep": [
                {"selector": "age", "older_than_seconds": 86400},
                {"selector": "source", "name": "pypi"},
                {"selector": "project-prefix", "prefix": "acme-"},
                {"selector": "keep-latest", "count": 5},
                {"selector": "cached"}
            ],
            "expire": [
                {"selector": "trash"},
                {"selector": "orphan"}
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
                    name: "pypi".to_owned()
                },
                RetentionSelector::ProjectPrefix {
                    prefix: "acme-".to_owned()
                },
                RetentionSelector::KeepLatest { count: 5 },
                RetentionSelector::Cached,
            ],
            expire: vec![RetentionSelector::Trash, RetentionSelector::Orphan],
        }
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

    let json = serde_json::to_value(summary).unwrap();
    assert_eq!(
        json["frontier"],
        serde_json::json!({"repository": 7, "catalog": 3, "policy": 2})
    );
    assert_eq!(json["policy_version"], serde_json::json!(summary.policy_version));
}
