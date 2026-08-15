use peryx_policy::{Policy, PolicyAction, PolicyConfig, PolicyDenial};

use super::{FallbackMode, PackageType, PypiPolicyConfig, PypiPolicyError, RemoteMetadataMode, compile_capabilities};

#[test]
fn test_package_type_parse_rejects_an_unknown_value() {
    assert_eq!(PackageType::parse("wheel"), Some(PackageType::Wheel));
    assert_eq!(PackageType::parse("sdist"), Some(PackageType::Sdist));
    assert_eq!(PackageType::parse("egg"), None);
}

#[test]
fn test_compile_capabilities_preserves_the_invalid_version_specifier() {
    let config = PypiPolicyConfig {
        allow_versions: Some("not a specifier".to_owned()),
        ..PypiPolicyConfig::default()
    };

    assert!(matches!(
        compile_capabilities(&config),
        Err(PypiPolicyError::VersionSpecifiers(value)) if value == "not a specifier"
    ));
}

#[test]
fn test_project_rules_normalize_blocked_and_protected_names() {
    let policy = policy(&PypiPolicyConfig {
        block_projects: vec!["Blocked.Project".to_owned()],
        protected_names: vec!["Exact.Name".to_owned(), "Internal_*".to_owned()],
        ..PypiPolicyConfig::default()
    });

    assert_eq!(
        [
            policy.check_resource(PolicyAction::Cached, "exact-name").unwrap_err(),
            policy
                .check_resource(PolicyAction::Cached, "internal-package")
                .unwrap_err(),
            policy
                .check_resource(PolicyAction::Serve, "blocked-project")
                .unwrap_err(),
        ],
        [
            PolicyDenial::new(
                PolicyAction::Cached,
                "exact-name",
                None,
                None,
                "protected-name",
                "project",
                "project \"exact-name\" is protected from upstream fallback".to_owned(),
            ),
            PolicyDenial::new(
                PolicyAction::Cached,
                "internal-package",
                None,
                None,
                "protected-name",
                "project",
                "project \"internal-package\" is protected from upstream fallback".to_owned(),
            ),
            PolicyDenial::new(
                PolicyAction::Serve,
                "blocked-project",
                None,
                None,
                "project-block-list",
                "project",
                "project \"blocked-project\" is blocked".to_owned(),
            ),
        ]
    );
}

#[test]
fn test_protected_names_do_not_block_serving() {
    let policy = policy(&PypiPolicyConfig {
        protected_names: vec!["Exact.Name".to_owned()],
        ..PypiPolicyConfig::default()
    });

    assert_eq!(policy.check_resource(PolicyAction::Serve, "exact-name"), Ok(()));
}

#[test]
fn test_policy_settings_ignore_unowned_keys() {
    let policy = policy(&PypiPolicyConfig {
        fallback_mode: FallbackMode::PrivateFirst,
        ..PypiPolicyConfig::default()
    });

    assert_eq!(
        (
            policy.owner_setting("pypi.fallback-mode"),
            policy.owner_setting("other.setting"),
        ),
        (Some("private-first"), None)
    );
}

#[test]
fn test_remote_metadata_modes_have_stable_config_values() {
    for (mode, expected) in [
        (RemoteMetadataMode::Direct, "direct"),
        (RemoteMetadataMode::Proxy, "proxy"),
        (RemoteMetadataMode::Cache, "cache"),
    ] {
        assert_eq!(mode.as_str(), expected);
    }
}

fn policy(config: &PypiPolicyConfig) -> Policy {
    Policy::compile(&PolicyConfig::default(), crate::normalize_name)
        .with_capabilities(compile_capabilities(config).unwrap())
}
