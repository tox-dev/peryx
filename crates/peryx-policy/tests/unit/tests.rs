use std::sync::{Arc, Mutex};

use super::{
    ArtifactFacts, ArtifactRule, Policy, PolicyAction, PolicyCapabilities, PolicyConfig, PolicyDecisionRecorder,
    PolicyDecisionState, PolicyDenial, PolicyEvaluation, PolicyLimits, ResourceRule,
};

#[derive(Debug)]
struct RepositoryModeRule;

impl ArtifactRule for RepositoryModeRule {
    fn check(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        (facts.attribute("allowed") == Some("yes"))
            .then_some(())
            .ok_or_else(|| {
                facts.denial(
                    action,
                    "repository-mode",
                    "allowed",
                    "artifact is not allowed".to_owned(),
                )
            })
    }
}

#[derive(Debug)]
struct BlockedResourceRule;

impl ResourceRule for BlockedResourceRule {
    fn check(&self, action: PolicyAction, resource: &str) -> Result<(), PolicyDenial> {
        (resource != "blocked").then_some(()).ok_or_else(|| {
            PolicyDenial::new(
                action,
                resource,
                None,
                None,
                "owner-resource-block",
                "resource",
                format!("resource {resource:?} is blocked by its owner"),
            )
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RecordedEvaluation {
    action: PolicyAction,
    resource: String,
    artifact: Option<String>,
    group: Option<String>,
    source: Option<String>,
    state: PolicyDecisionState,
    rule: Option<&'static str>,
    reason: Option<String>,
    next_eligible_at_unix: Option<i64>,
}

impl From<PolicyEvaluation<'_>> for RecordedEvaluation {
    fn from(evaluation: PolicyEvaluation<'_>) -> Self {
        Self {
            action: evaluation.action,
            resource: evaluation.resource.to_owned(),
            artifact: evaluation.artifact.map(str::to_owned),
            group: evaluation.group.map(str::to_owned),
            source: evaluation.source.map(str::to_owned),
            state: evaluation.state,
            rule: evaluation.rule,
            reason: evaluation.reason.map(str::to_owned),
            next_eligible_at_unix: evaluation.next_eligible_at_unix,
        }
    }
}

#[derive(Debug, Default)]
struct Recorder(Mutex<Vec<RecordedEvaluation>>);

impl PolicyDecisionRecorder for Recorder {
    fn record(&self, evaluation: PolicyEvaluation<'_>) {
        self.0.lock().unwrap().push(evaluation.into());
    }
}

#[test]
fn check_size_allows_the_configured_limit() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_artifact_size_bytes: Some(4),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    assert_eq!(policy.check_size(PolicyAction::Upload, "resource", 4), Ok(()));
}

#[test]
fn check_size_rejects_blocked_resources() {
    let policy = Policy::compile(
        &PolicyConfig {
            block_resources: vec!["blocked".to_owned()],
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    let denial = policy
        .check_size(PolicyAction::Upload, "blocked", 1)
        .expect_err("a blocked resource should be denied");

    assert_eq!(denial.rule, "resource-block-list");
}

#[test]
fn check_size_rejects_resources_outside_the_allow_list() {
    let policy = Policy::compile(
        &PolicyConfig {
            allow_resources: vec!["allowed".to_owned()],
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    let denial = policy
        .check_size(PolicyAction::Upload, "other", 1)
        .expect_err("a resource outside the allow list should be denied");

    assert_eq!(denial.rule, "resource-allow-list");
}

#[test]
fn check_size_rejects_values_above_the_limit() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_artifact_size_bytes: Some(10),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    let denial = policy
        .check_size(PolicyAction::Upload, "demo", 11)
        .expect_err("an artifact above the configured limit should be denied");

    assert_eq!(denial.reason.as_ref(), "artifact size 11 exceeds limit 10");
}

macro_rules! protecting {
    ($names:expr) => {
        Policy::compile(
            &PolicyConfig {
                protected_resources: $names.iter().map(|&name| name.to_owned()).collect(),
                ..PolicyConfig::default()
            },
            |name| name.replace(['_', '.'], "-").to_lowercase(),
        )
    };
}

#[test]
fn a_protected_name_is_active() {
    assert!(protecting!(&["acme-secrets"]).active());
    assert!(!Policy::default().active());
}

#[test]
fn an_exact_protected_name_cannot_fall_back_upstream() {
    let denial = protecting!(&["acme-secrets"])
        .check_resource(PolicyAction::Cached, "acme-secrets")
        .unwrap_err();

    assert_eq!(denial.rule, "protected-name");
    assert_eq!(denial.action, PolicyAction::Cached);
    assert_eq!(
        &*denial.reason,
        "resource \"acme-secrets\" is protected from upstream fallback by rule \"acme-secrets\""
    );
}

#[test]
fn a_prefix_rule_protects_a_whole_namespace_upstream() {
    let denial = protecting!(&["acme-*"])
        .check_resource(PolicyAction::Cached, "acme-widgets")
        .unwrap_err();

    assert_eq!(denial.rule, "protected-name");
    assert_eq!(
        &*denial.reason,
        "resource \"acme-widgets\" is protected from upstream fallback by rule \"acme-*\""
    );
}

#[test]
fn a_name_outside_every_rule_still_falls_back_upstream() {
    assert_eq!(
        protecting!(&["acme-secrets", "acme-*"]).check_resource(PolicyAction::Cached, "requests"),
        Ok(())
    );
}

#[test]
fn a_protected_name_is_served_and_uploaded_from_hosted_members() {
    let policy = protecting!(&["acme-*"]);

    assert_eq!(policy.check_resource(PolicyAction::Serve, "acme-widgets"), Ok(()));
    assert_eq!(policy.check_resource(PolicyAction::Upload, "acme-widgets"), Ok(()));
}

#[test]
fn protection_matches_after_normalization() {
    let policy = protecting!(&["Acme_Secrets", "Team.*"]);

    assert!(policy.check_resource(PolicyAction::Cached, "acme-secrets").is_err());
    assert!(policy.check_resource(PolicyAction::Cached, "team-alpha").is_err());
}

#[test]
fn quota_limits_read_back_and_activate_the_policy() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_accounted_bytes: Some(1024),
            max_resources: Some(8),
            quota_audit: true,
            ..PolicyConfig::default()
        },
        str::to_owned,
    )
    .with_capabilities(
        PolicyCapabilities::default()
            .with_artifact_rules(vec![Arc::new(RepositoryModeRule)])
            .with_limits(PolicyLimits {
                max_accounted_bytes: Some(512),
                max_groups_per_resource: Some(16),
                ..PolicyLimits::default()
            })
            .with_owner_setting("mode", "strict"),
    );

    assert_eq!(
        (
            policy.max_accounted_bytes(),
            policy.max_resources(),
            policy.max_groups_per_resource(),
            policy.owner_setting("mode"),
            policy.quota_audit(),
            policy.enforces_quota(),
            policy.active(),
        ),
        (Some(512), Some(8), Some(16), Some("strict"), true, true, true)
    );
}

#[test]
fn owner_activation_marks_capabilities_and_policy_active() {
    let capabilities = PolicyCapabilities::default().with_policy_activation();

    assert_eq!(
        (
            PolicyCapabilities::default().is_empty(),
            capabilities.is_empty(),
            Policy::default().with_capabilities(capabilities).active(),
        ),
        (true, false, true)
    );
}

#[test]
fn a_per_artifact_size_limit_alone_does_not_enforce_a_quota() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_artifact_size_bytes: Some(64),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    assert!(!policy.enforces_quota());
    assert!(policy.active());
}

#[test]
fn audit_mode_alone_does_not_enforce_a_quota() {
    let policy = Policy::compile(
        &PolicyConfig {
            quota_audit: true,
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    assert_eq!(
        (policy.quota_audit(), policy.enforces_quota(), policy.active()),
        (true, false, false)
    );
}

#[test]
fn artifact_facts_find_named_attributes() {
    let facts = ArtifactFacts {
        attributes: vec![("kind", "archive".to_owned())],
        ..ArtifactFacts::default()
    };

    assert_eq!(facts.attribute("kind"), Some("archive"));
    assert_eq!(facts.attribute("missing"), None);
}

#[test]
fn artifact_facts_build_contextual_denials() {
    let facts = ArtifactFacts {
        resource: "demo".to_owned(),
        artifact: Some("demo.bin".to_owned()),
        group: Some("1.0".to_owned()),
        ..ArtifactFacts::default()
    };

    let denial = facts.denial(
        PolicyAction::Serve,
        "blocked-kind",
        "kind",
        "artifact is blocked".to_owned(),
    );

    assert_eq!(
        denial,
        PolicyDenial::new(
            PolicyAction::Serve,
            "demo",
            Some("demo.bin"),
            Some("1.0".to_owned()),
            "blocked-kind",
            "kind",
            "artifact is blocked".to_owned(),
        )
    );
}

#[test]
fn attached_filtering_rules_activate_policy() {
    let policy = Policy::compile(&PolicyConfig::default(), str::to_owned)
        .with_rules(vec![Arc::new(RepositoryModeRule) as Arc<dyn ArtifactRule>]);
    let facts = ArtifactFacts {
        attributes: vec![("allowed", "yes".to_owned())],
        ..ArtifactFacts::default()
    };

    assert_eq!(
        (policy.active(), policy.check_facts(PolicyAction::Serve, &facts)),
        (true, Ok(()))
    );
}

#[test]
fn resource_checks_apply_attached_resource_rules() {
    let policy = Policy::compile(&PolicyConfig::default(), str::to_owned)
        .with_capabilities(PolicyCapabilities::default().with_resource_rules(vec![Arc::new(BlockedResourceRule)]));

    assert_eq!(policy.check_resource(PolicyAction::Serve, "allowed"), Ok(()));
    assert_eq!(
        policy.check_resource(PolicyAction::Serve, "blocked").unwrap_err().rule,
        "owner-resource-block"
    );
}

#[test]
fn fact_checks_apply_attached_rules() {
    let policy = Policy::compile(&PolicyConfig::default(), str::to_owned)
        .with_rules(vec![Arc::new(RepositoryModeRule) as Arc<dyn ArtifactRule>]);
    let facts = ArtifactFacts {
        resource: "demo".to_owned(),
        artifact: Some("artifact.bin".to_owned()),
        group: Some("1.0".to_owned()),
        source: Some("upstream".to_owned()),
        attributes: vec![("allowed", "yes".to_owned())],
        ..ArtifactFacts::default()
    };

    assert_eq!(policy.check_facts(PolicyAction::Serve, &facts), Ok(()));
}

#[test]
fn fact_checks_return_rule_denials() {
    let policy = Policy::compile(&PolicyConfig::default(), str::to_owned)
        .with_rules(vec![Arc::new(RepositoryModeRule) as Arc<dyn ArtifactRule>]);
    let facts = ArtifactFacts {
        resource: "demo".to_owned(),
        ..ArtifactFacts::default()
    };

    let denial = policy
        .check_facts(PolicyAction::Serve, &facts)
        .expect_err("the rule should deny an artifact without its required attribute");

    assert_eq!(denial.rule, "repository-mode");
}

#[test]
fn fact_checks_record_allowed_decisions() {
    let recorder = Arc::new(Recorder::default());
    let policy =
        Policy::compile(&PolicyConfig::default(), str::to_owned).with_decision_recorder(Arc::clone(&recorder) as _);
    let facts = ArtifactFacts {
        resource: "demo".to_owned(),
        artifact: Some("artifact.bin".to_owned()),
        group: Some("1.0".to_owned()),
        source: Some("upstream".to_owned()),
        ..ArtifactFacts::default()
    };

    assert_eq!(policy.check_facts(PolicyAction::Serve, &facts), Ok(()));
    assert_eq!(
        *recorder.0.lock().unwrap(),
        [RecordedEvaluation {
            action: PolicyAction::Serve,
            resource: "demo".to_owned(),
            artifact: Some("artifact.bin".to_owned()),
            group: Some("1.0".to_owned()),
            source: Some("upstream".to_owned()),
            state: PolicyDecisionState::Allow,
            rule: None,
            reason: None,
            next_eligible_at_unix: None,
        }]
    );
}

#[test]
fn fact_checks_record_denied_decisions() {
    let recorder = Arc::new(Recorder::default());
    let policy = Policy::compile(
        &PolicyConfig {
            max_artifact_size_bytes: Some(10),
            ..PolicyConfig::default()
        },
        str::to_owned,
    )
    .with_decision_recorder(Arc::clone(&recorder) as _);

    let denial = policy
        .check_facts(
            PolicyAction::Upload,
            &ArtifactFacts {
                resource: "demo".to_owned(),
                ..ArtifactFacts::default()
            },
        )
        .expect_err("a configured artifact limit requires a known size");

    assert_eq!(denial.reason.as_ref(), "artifact size is unknown");
    assert_eq!(
        *recorder.0.lock().unwrap(),
        [RecordedEvaluation {
            action: PolicyAction::Upload,
            resource: "demo".to_owned(),
            artifact: None,
            group: None,
            source: None,
            state: PolicyDecisionState::Deny,
            rule: Some("max-artifact-size"),
            reason: Some("artifact size is unknown".to_owned()),
            next_eligible_at_unix: None,
        }]
    );
}

#[test]
fn fact_size_checks_allow_the_limit() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_artifact_size_bytes: Some(10),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );
    let facts = ArtifactFacts {
        resource: "demo".to_owned(),
        size: Some(10),
        ..ArtifactFacts::default()
    };

    assert_eq!(policy.check_facts(PolicyAction::Upload, &facts), Ok(()));
}

#[test]
fn fact_size_checks_reject_values_above_the_limit() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_artifact_size_bytes: Some(10),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );
    let facts = ArtifactFacts {
        resource: "demo".to_owned(),
        size: Some(11),
        ..ArtifactFacts::default()
    };

    let denial = policy
        .check_facts(PolicyAction::Upload, &facts)
        .expect_err("an artifact above the configured limit should be denied");

    assert_eq!(denial.reason.as_ref(), "artifact size 11 exceeds limit 10");
}

#[test]
fn policy_reports_artifact_and_resource_limits() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_artifact_size_bytes: Some(10),
            max_resource_size_bytes: Some(20),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    assert_eq!(policy.max_artifact_size(), Some(10));
    assert!(policy.has_resource_size_limit());
    assert_eq!(policy.max_resource_size(), Some(20));
}

#[test]
fn policy_reports_missing_resource_limit() {
    let policy = Policy::compile(&PolicyConfig::default(), str::to_owned);

    assert!(!policy.has_resource_size_limit());
    assert_eq!(policy.max_resource_size(), None);
}

#[test]
fn policy_actions_render_wire_names() {
    for (action, name) in [
        (PolicyAction::Upload, "upload"),
        (PolicyAction::Cached, "cached"),
        (PolicyAction::Serve, "serve"),
    ] {
        assert_eq!(action.to_string(), name);
    }
}

#[test]
fn policy_denials_render_their_reason() {
    let denial = PolicyDenial::new(
        PolicyAction::Upload,
        "demo",
        None,
        None,
        "blocked",
        "resource",
        "resource is blocked".to_owned(),
    );

    assert_eq!(denial.to_string(), "resource is blocked");
}
