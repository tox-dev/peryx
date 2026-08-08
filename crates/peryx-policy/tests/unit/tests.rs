use std::collections::BTreeSet;
use std::sync::Arc;

use mockall::mock;

use super::{
    ArtifactFacts, ArtifactRule, FallbackMode, Policy, PolicyAction, PolicyConfig, PolicyDecisionRecorder,
    PolicyDecisionState, PolicyDenial, PolicyEvaluation, RemoteMetadataMode, retain_versions,
};

#[derive(Debug)]
struct DefaultRule;

impl ArtifactRule for DefaultRule {
    fn check(&self, _action: PolicyAction, _facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        Ok(())
    }
}

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

    fn filters_artifacts(&self) -> bool {
        false
    }

    fn fallback_mode(&self) -> Option<FallbackMode> {
        Some(FallbackMode::PrivateFirst)
    }

    fn remote_metadata_mode(&self) -> Option<RemoteMetadataMode> {
        Some(RemoteMetadataMode::Cache)
    }
}

mock! {
    #[derive(Debug)]
    Recorder {}

    impl PolicyDecisionRecorder for Recorder {
        fn record<'a>(&self, evaluation: PolicyEvaluation<'a>);
    }
}

#[test]
fn check_size_allows_the_configured_limit() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_file_size_bytes: Some(4),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    assert_eq!(policy.check_size(PolicyAction::Upload, "project", 4), Ok(()));
}

#[test]
fn check_size_rejects_blocked_projects() {
    let policy = Policy::compile(
        &PolicyConfig {
            block_projects: vec!["blocked".to_owned()],
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    let denial = policy
        .check_size(PolicyAction::Upload, "blocked", 1)
        .expect_err("a blocked project should be denied");

    assert_eq!(denial.rule, "project-block-list");
}

#[test]
fn check_size_rejects_projects_outside_the_allow_list() {
    let policy = Policy::compile(
        &PolicyConfig {
            allow_projects: vec!["allowed".to_owned()],
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    let denial = policy
        .check_size(PolicyAction::Upload, "other", 1)
        .expect_err("a project outside the allow list should be denied");

    assert_eq!(denial.rule, "project-allow-list");
}

#[test]
fn check_size_rejects_values_above_the_limit() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_file_size_bytes: Some(10),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    let denial = policy
        .check_size(PolicyAction::Upload, "demo", 11)
        .expect_err("a file above the configured limit should be denied");

    assert_eq!(denial.reason.as_ref(), "file size 11 exceeds limit 10");
}

fn protecting(names: &[&str]) -> Policy {
    Policy::compile(
        &PolicyConfig {
            protected_names: names.iter().map(|&name| name.to_owned()).collect(),
            ..PolicyConfig::default()
        },
        |name| name.replace(['_', '.'], "-").to_lowercase(),
    )
}

#[test]
fn a_protected_name_is_active() {
    assert!(protecting(&["acme-secrets"]).active());
    assert!(!Policy::default().active());
}

#[test]
fn an_exact_protected_name_cannot_fall_back_upstream() {
    let denial = protecting(&["acme-secrets"])
        .check_project(PolicyAction::Cached, "acme-secrets")
        .unwrap_err();

    assert_eq!(denial.rule, "protected-name");
    assert_eq!(denial.action, PolicyAction::Cached);
    assert_eq!(
        &*denial.reason,
        "project \"acme-secrets\" is protected from upstream fallback by rule \"acme-secrets\""
    );
}

#[test]
fn a_prefix_rule_protects_a_whole_namespace_upstream() {
    let denial = protecting(&["acme-*"])
        .check_project(PolicyAction::Cached, "acme-widgets")
        .unwrap_err();

    assert_eq!(denial.rule, "protected-name");
    assert_eq!(
        &*denial.reason,
        "project \"acme-widgets\" is protected from upstream fallback by rule \"acme-*\""
    );
}

#[test]
fn a_name_outside_every_rule_still_falls_back_upstream() {
    assert_eq!(
        protecting(&["acme-secrets", "acme-*"]).check_project(PolicyAction::Cached, "requests"),
        Ok(())
    );
}

#[test]
fn a_protected_name_is_served_and_uploaded_from_hosted_members() {
    let policy = protecting(&["acme-*"]);

    assert_eq!(policy.check_project(PolicyAction::Serve, "acme-widgets"), Ok(()));
    assert_eq!(policy.check_project(PolicyAction::Upload, "acme-widgets"), Ok(()));
}

#[test]
fn protection_matches_after_normalization() {
    let policy = protecting(&["Acme_Secrets", "Team.*"]);

    assert!(policy.check_project(PolicyAction::Cached, "acme-secrets").is_err());
    assert!(policy.check_project(PolicyAction::Cached, "team-alpha").is_err());
}

#[test]
fn quota_limits_read_back_and_activate_the_policy() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_accounted_bytes: Some(1024),
            max_projects: Some(8),
            max_versions_per_project: Some(16),
            quota_audit: true,
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    assert_eq!(
        (
            policy.max_accounted_bytes(),
            policy.max_projects(),
            policy.max_versions_per_project(),
            policy.quota_audit(),
            policy.enforces_quota(),
            policy.active(),
        ),
        (Some(1024), Some(8), Some(16), true, true, true)
    );
}

#[test]
fn a_per_file_size_limit_alone_does_not_enforce_a_quota() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_file_size_bytes: Some(64),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    // The byte stream enforces the per-file limit directly, so it does not switch repository
    // accounting on, yet the policy is still active for that limit.
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
fn a_fallback_mode_renders_its_configured_wire_name() {
    for (mode, name) in [
        (FallbackMode::Fallback, "fallback"),
        (FallbackMode::PrivateFirst, "private-first"),
        (FallbackMode::NoFallback, "no-fallback"),
    ] {
        assert_eq!(mode.as_str(), name);
        assert_eq!(mode.to_string(), name);
    }
}

#[test]
fn artifact_facts_find_named_attributes() {
    let facts = ArtifactFacts {
        attributes: vec![("kind", "wheel".to_owned())],
        ..ArtifactFacts::default()
    };

    assert_eq!(facts.attribute("kind"), Some("wheel"));
    assert_eq!(facts.attribute("missing"), None);
}

#[test]
fn artifact_facts_build_contextual_denials() {
    let facts = ArtifactFacts {
        project: "demo".to_owned(),
        filename: Some("demo.whl".to_owned()),
        version: Some("1.0".to_owned()),
        ..ArtifactFacts::default()
    };

    let denial = facts.denial(
        PolicyAction::Serve,
        "blocked-kind",
        "kind",
        "artifact is blocked".to_owned(),
    );

    assert_eq!(denial.action, PolicyAction::Serve);
    assert_eq!(denial.project.as_ref(), "demo");
    assert_eq!(denial.filename.as_deref(), Some("demo.whl"));
    assert_eq!(denial.version.as_deref(), Some("1.0"));
    assert_eq!(denial.rule, "blocked-kind");
    assert_eq!(denial.field, "kind");
    assert_eq!(denial.reason.as_ref(), "artifact is blocked");
}

#[test]
fn artifact_rule_defaults_preserve_neutral_policy() {
    let rule = DefaultRule;

    assert!(rule.filters_artifacts());
    assert_eq!(rule.fallback_mode(), None);
    assert_eq!(rule.remote_metadata_mode(), None);
}

#[test]
fn attached_filtering_rules_activate_policy() {
    let policy = Policy::compile(&PolicyConfig::default(), str::to_owned).with_rules(vec![Arc::new(DefaultRule)]);

    assert!(policy.active());
}

#[test]
fn repository_mode_rules_do_not_activate_filtering() {
    let policy =
        Policy::compile(&PolicyConfig::default(), str::to_owned).with_rules(vec![Arc::new(RepositoryModeRule)]);

    assert!(!policy.active());
    assert_eq!(policy.fallback_mode(), FallbackMode::PrivateFirst);
    assert_eq!(policy.remote_metadata_mode(), RemoteMetadataMode::Cache);
}

#[test]
fn policy_modes_default_without_rules() {
    let policy = Policy::compile(&PolicyConfig::default(), str::to_owned);

    assert_eq!(policy.fallback_mode(), FallbackMode::Fallback);
    assert_eq!(policy.remote_metadata_mode(), RemoteMetadataMode::Direct);
}

#[test]
fn fact_checks_apply_attached_rules() {
    let policy =
        Policy::compile(&PolicyConfig::default(), str::to_owned).with_rules(vec![Arc::new(RepositoryModeRule)]);
    let facts = ArtifactFacts {
        project: "demo".to_owned(),
        filename: Some("demo.whl".to_owned()),
        version: Some("1.0".to_owned()),
        source: Some("upstream".to_owned()),
        attributes: vec![("allowed", "yes".to_owned())],
        ..ArtifactFacts::default()
    };

    assert_eq!(policy.check_facts(PolicyAction::Serve, &facts), Ok(()));
}

#[test]
fn fact_checks_return_rule_denials() {
    let policy =
        Policy::compile(&PolicyConfig::default(), str::to_owned).with_rules(vec![Arc::new(RepositoryModeRule)]);
    let facts = ArtifactFacts {
        project: "demo".to_owned(),
        ..ArtifactFacts::default()
    };

    let denial = policy
        .check_facts(PolicyAction::Serve, &facts)
        .expect_err("the rule should deny an artifact without its required attribute");

    assert_eq!(denial.rule, "repository-mode");
}

#[test]
fn fact_checks_record_allowed_decisions() {
    let mut recorder = MockRecorder::new();
    recorder
        .expect_record()
        .once()
        .withf(|evaluation| {
            evaluation.action == PolicyAction::Serve
                && evaluation.project == "demo"
                && evaluation.filename == Some("demo.whl")
                && evaluation.version == Some("1.0")
                && evaluation.source == Some("upstream")
                && evaluation.state == PolicyDecisionState::Allow
                && evaluation.rule.is_none()
                && evaluation.reason.is_none()
                && evaluation.next_eligible_at_unix.is_none()
        })
        .return_const(());
    let policy = Policy::compile(&PolicyConfig::default(), str::to_owned).with_decision_recorder(Arc::new(recorder));
    let facts = ArtifactFacts {
        project: "demo".to_owned(),
        filename: Some("demo.whl".to_owned()),
        version: Some("1.0".to_owned()),
        source: Some("upstream".to_owned()),
        ..ArtifactFacts::default()
    };

    assert_eq!(policy.check_facts(PolicyAction::Serve, &facts), Ok(()));
}

#[test]
fn fact_checks_record_denied_decisions() {
    let mut recorder = MockRecorder::new();
    recorder
        .expect_record()
        .once()
        .withf(|evaluation| {
            evaluation.state == PolicyDecisionState::Deny
                && evaluation.rule == Some("max-file-size")
                && evaluation.reason == Some("file size is unknown")
        })
        .return_const(());
    let policy = Policy::compile(
        &PolicyConfig {
            max_file_size_bytes: Some(10),
            ..PolicyConfig::default()
        },
        str::to_owned,
    )
    .with_decision_recorder(Arc::new(recorder));

    let denial = policy
        .check_facts(
            PolicyAction::Upload,
            &ArtifactFacts {
                project: "demo".to_owned(),
                ..ArtifactFacts::default()
            },
        )
        .expect_err("a configured file limit requires a known size");

    assert_eq!(denial.reason.as_ref(), "file size is unknown");
}

#[test]
fn fact_size_checks_allow_the_limit() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_file_size_bytes: Some(10),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );
    let facts = ArtifactFacts {
        project: "demo".to_owned(),
        size: Some(10),
        ..ArtifactFacts::default()
    };

    assert_eq!(policy.check_facts(PolicyAction::Upload, &facts), Ok(()));
}

#[test]
fn fact_size_checks_reject_values_above_the_limit() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_file_size_bytes: Some(10),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );
    let facts = ArtifactFacts {
        project: "demo".to_owned(),
        size: Some(11),
        ..ArtifactFacts::default()
    };

    let denial = policy
        .check_facts(PolicyAction::Upload, &facts)
        .expect_err("a file above the configured limit should be denied");

    assert_eq!(denial.reason.as_ref(), "file size 11 exceeds limit 10");
}

#[test]
fn policy_reports_file_and_project_limits() {
    let policy = Policy::compile(
        &PolicyConfig {
            max_file_size_bytes: Some(10),
            max_project_size_bytes: Some(20),
            ..PolicyConfig::default()
        },
        str::to_owned,
    );

    assert_eq!(policy.max_file_size(), Some(10));
    assert!(policy.has_project_size_limit());
    assert_eq!(policy.max_project_size(), Some(20));
}

#[test]
fn policy_reports_missing_project_limit() {
    let policy = Policy::compile(&PolicyConfig::default(), str::to_owned);

    assert!(!policy.has_project_size_limit());
    assert_eq!(policy.max_project_size(), None);
}

#[test]
fn policy_actions_render_wire_names() {
    assert_eq!(PolicyAction::Upload.to_string(), "upload");
    assert_eq!(PolicyAction::Cached.to_string(), "cached");
    assert_eq!(PolicyAction::Serve.to_string(), "serve");
}

#[test]
fn policy_denials_render_their_reason() {
    let denial = PolicyDenial::new(
        PolicyAction::Upload,
        "demo",
        None,
        None,
        "blocked",
        "project",
        "project is blocked".to_owned(),
    );

    assert_eq!(denial.to_string(), "project is blocked");
}

#[test]
fn retaining_no_versions_clears_the_input() {
    let mut versions = vec!["1.0".to_owned()];

    retain_versions(&mut versions, BTreeSet::new());

    assert!(versions.is_empty());
}

#[test]
fn retaining_versions_filters_and_appends() {
    let mut versions = vec!["1.0".to_owned(), "2.0".to_owned()];

    retain_versions(&mut versions, BTreeSet::from(["2.0".to_owned(), "3.0".to_owned()]));

    assert_eq!(versions, ["2.0", "3.0"]);
}
