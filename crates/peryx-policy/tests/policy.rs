use peryx_policy::{FallbackMode, Policy, PolicyAction, PolicyConfig};

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
