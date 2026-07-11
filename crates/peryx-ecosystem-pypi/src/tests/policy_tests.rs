use std::collections::BTreeMap;

use peryx_policy::{Policy, PolicyAction, PolicyConfig};

use crate::policy::{PackageType, PypiPolicy, PypiPolicyConfig, PypiPolicyError, compile_rules};
use crate::{CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Provenance, Yanked};

#[test]
fn test_apply_list_filters_project_rules() {
    let policy = policy(|neutral, _pypi| {
        neutral.block_projects = vec!["bad-pkg".to_owned()];
    });

    assert_eq!(
        policy.apply_list(ProjectList {
            meta: Meta::default(),
            projects: vec![
                ProjectListEntry {
                    name: "Flask".to_owned(),
                },
                ProjectListEntry {
                    name: "Bad_Pkg".to_owned(),
                },
            ],
        }),
        ProjectList {
            meta: Meta::default(),
            projects: vec![ProjectListEntry {
                name: "Flask".to_owned(),
            }],
        }
    );
}

#[test]
fn test_apply_detail_rejects_project_size_over_limit() {
    let policy = policy(|neutral, _pypi| {
        neutral.max_project_size_bytes = Some(10);
    });

    let denial = policy
        .apply_detail(
            PolicyAction::Serve,
            "demo",
            ProjectDetail {
                meta: Meta::default(),
                name: "demo".to_owned(),
                versions: vec!["1.0".to_owned(), "2.0".to_owned()],
                files: vec![
                    file("demo-1.0-py3-none-any.whl", Some(6)),
                    file("demo-2.0-py3-none-any.whl", Some(5)),
                ],
            },
        )
        .unwrap_err();

    assert_eq!(denial.rule, "max-project-size");
    assert_eq!(denial.field, "project_size");
    assert_eq!(denial.to_string(), "project size 11 exceeds limit 10");
}

#[test]
fn test_check_project_denies_project_outside_allow_list() {
    let policy = policy(|neutral, _pypi| {
        neutral.allow_projects = vec!["flask".to_owned()];
    });

    let denial = policy.check_project(PolicyAction::Serve, "django").unwrap_err();

    assert_eq!(denial.rule, "project-allow-list");
    assert_eq!(denial.field, "project");
    assert_eq!(denial.reason.as_ref(), "project \"django\" is not in the allow list");
}

#[test]
fn test_check_download_denies_unknown_file_attributes() {
    struct Case {
        label: &'static str,
        configure: fn(&mut PolicyConfig, &mut PypiPolicyConfig),
        rule: &'static str,
        field: &'static str,
        reason: &'static str,
    }
    let cases = [
        Case {
            label: "unknown version when versions are limited",
            configure: |_neutral, pypi| pypi.allow_versions = Some(">=1".to_owned()),
            rule: "version-specifier",
            field: "version",
            reason: "file version is unknown",
        },
        Case {
            label: "unknown package type when types are limited",
            configure: |_neutral, pypi| pypi.allow_package_types = vec![PackageType::Wheel],
            rule: "package-type-allow-list",
            field: "package_type",
            reason: "package type is unknown",
        },
    ];

    for case in cases {
        let policy = policy(case.configure);

        let denial = policy
            .check_download(PolicyAction::Serve, "not-a-dist.whl", Some(1))
            .unwrap_err();

        assert_eq!(denial.rule, case.rule, "{}", case.label);
        assert_eq!(denial.field, case.field, "{}", case.label);
        assert_eq!(denial.reason.as_ref(), case.reason, "{}", case.label);
    }
}

#[test]
fn test_check_file_denies_by_rule_and_field() {
    struct Case {
        label: &'static str,
        configure: fn(&mut PolicyConfig, &mut PypiPolicyConfig),
        rule: &'static str,
        field: &'static str,
        reason: Option<&'static str>,
    }
    let cases = [
        Case {
            label: "blocked wheel package type",
            configure: |_neutral, pypi| pypi.block_package_types = vec![PackageType::Wheel],
            rule: "package-type-block-list",
            field: "package_type",
            reason: Some("package type wheel is blocked"),
        },
        Case {
            label: "wheel python allow list",
            configure: |_neutral, pypi| pypi.allow_wheel_pythons = vec!["cp39".to_owned()],
            rule: "wheel-python-allow-list",
            field: "wheel_python",
            reason: None,
        },
        Case {
            label: "wheel platform block list",
            configure: |_neutral, pypi| pypi.block_wheel_platforms = vec!["any".to_owned()],
            rule: "wheel-platform-block-list",
            field: "wheel_platform",
            reason: None,
        },
    ];

    for case in cases {
        let policy = policy(case.configure);

        let denial = policy
            .check_file(PolicyAction::Serve, "demo", &file("demo-1.0-py3-none-any.whl", Some(1)))
            .unwrap_err();

        assert_eq!(denial.rule, case.rule, "{}", case.label);
        assert_eq!(denial.field, case.field, "{}", case.label);
        if let Some(reason) = case.reason {
            assert_eq!(denial.reason.as_ref(), reason, "{}", case.label);
        }
    }
}

#[test]
fn test_check_file_accepts_wheel_tag_allow_and_block_rules() {
    let policy = policy(|_neutral, pypi| {
        pypi.allow_wheel_pythons = vec!["py3".to_owned()];
        pypi.block_wheel_pythons = vec!["cp39".to_owned()];
        pypi.allow_wheel_platforms = vec!["any".to_owned()];
        pypi.block_wheel_platforms = vec!["manylinux_2_28_x86_64".to_owned()];
    });

    policy
        .check_file(
            PolicyAction::Serve,
            "demo",
            &file("demo-1.0-py2.py3-none-any.whl", Some(1)),
        )
        .unwrap();
}

#[test]
fn test_policy_action_display_formats_mirror() {
    assert_eq!(PolicyAction::Cached.to_string(), "cached");
}

#[test]
fn test_apply_detail_accepts_project_size_under_limit() {
    let policy = policy(|neutral, _pypi| {
        neutral.max_project_size_bytes = Some(10);
    });

    let detail = policy
        .apply_detail(
            PolicyAction::Serve,
            "demo",
            ProjectDetail {
                meta: Meta::default(),
                name: "demo".to_owned(),
                versions: vec!["1.0".to_owned(), "2.0".to_owned()],
                files: vec![
                    file("demo-1.0-py3-none-any.whl", Some(4)),
                    file("demo-2.0-py3-none-any.whl", Some(5)),
                ],
            },
        )
        .unwrap();

    assert_eq!(detail.files.len(), 2);
    assert_eq!(detail.versions, ["1.0", "2.0"]);
}

#[test]
fn test_apply_detail_rejects_project_size_without_file_size() {
    let policy = policy(|neutral, _pypi| {
        neutral.max_project_size_bytes = Some(10);
    });

    let denial = policy
        .apply_detail(
            PolicyAction::Serve,
            "demo",
            ProjectDetail {
                meta: Meta::default(),
                name: "demo".to_owned(),
                versions: vec!["1.0".to_owned()],
                files: vec![file("demo-1.0-py3-none-any.whl", None)],
            },
        )
        .unwrap_err();

    assert_eq!(denial.rule, "max-project-size");
    assert_eq!(denial.field, "size");
    assert_eq!(
        denial.reason.as_ref(),
        "project size is unknown because file \"demo-1.0-py3-none-any.whl\" has no declared size"
    );
}

#[test]
fn test_apply_detail_clears_versions_when_no_file_versions_remain() {
    let policy = policy(|neutral, _pypi| {
        neutral.block_projects = vec!["blocked".to_owned()];
    });

    let detail = policy
        .apply_detail(
            PolicyAction::Serve,
            "demo",
            ProjectDetail {
                meta: Meta::default(),
                name: "demo".to_owned(),
                versions: vec!["1.0".to_owned()],
                files: vec![file("not-a-dist.whl", Some(1))],
            },
        )
        .unwrap();

    assert!(detail.versions.is_empty());
}

#[test]
fn test_apply_detail_adds_missing_file_versions() {
    let policy = policy(|neutral, _pypi| {
        neutral.block_projects = vec!["blocked".to_owned()];
    });

    let detail = policy
        .apply_detail(
            PolicyAction::Serve,
            "demo",
            ProjectDetail {
                meta: Meta::default(),
                name: "demo".to_owned(),
                versions: Vec::new(),
                files: vec![file("demo-2.0-py3-none-any.whl", Some(1))],
            },
        )
        .unwrap();

    assert_eq!(detail.versions, ["2.0"]);
}

#[test]
fn test_preview_detail_reports_file_and_project_size_denials() {
    let policy = policy(|neutral, pypi| {
        pypi.block_package_types = vec![PackageType::Sdist];
        neutral.max_project_size_bytes = Some(5);
    });
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: "demo".to_owned(),
        versions: vec!["1.0".to_owned(), "2.0".to_owned()],
        files: vec![
            file("demo-1.0-py3-none-any.whl", Some(4)),
            file("demo-1.0.tar.gz", Some(1)),
            file("demo-2.0-py3-none-any.whl", Some(4)),
        ],
    };

    let denials = policy.preview_detail(PolicyAction::Serve, &detail);

    assert_eq!(denials.len(), 2);
    assert_eq!(denials[0].rule, "package-type-block-list");
    assert_eq!(denials[1].rule, "max-project-size");
}

#[test]
fn test_compile_rejects_empty_wheel_tag() {
    let config = PypiPolicyConfig {
        allow_wheel_pythons: vec![String::new()],
        ..PypiPolicyConfig::default()
    };

    assert!(matches!(
        compile_rules(&config),
        Err(PypiPolicyError::EmptyTag(value)) if value.is_empty()
    ));
}

fn policy(configure: impl FnOnce(&mut PolicyConfig, &mut PypiPolicyConfig)) -> Policy {
    let mut neutral = PolicyConfig::default();
    let mut pypi = PypiPolicyConfig::default();
    configure(&mut neutral, &mut pypi);
    Policy::compile(&neutral, crate::normalize_name).with_rules(compile_rules(&pypi).unwrap())
}

fn file(filename: &str, size: Option<u64>) -> File {
    File {
        filename: filename.to_owned(),
        url: format!("https://files.example/{filename}"),
        hashes: BTreeMap::new(),
        requires_python: None,
        size,
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Absent,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::Absent,
    }
}

#[test]
fn test_compile_rejects_empty_platform_tag() {
    let config = PypiPolicyConfig {
        allow_wheel_platforms: vec![String::new()],
        ..PypiPolicyConfig::default()
    };
    assert!(matches!(
        compile_rules(&config),
        Err(PypiPolicyError::EmptyTag(value)) if value.is_empty()
    ));
}

#[test]
fn test_wheel_tag_rule_ignores_non_wheel_files() {
    let policy = policy(|_neutral, pypi| {
        pypi.block_wheel_platforms = vec!["any".to_owned()];
    });
    // An sdist carries no wheel tags, so a wheel-tag rule does not apply to it.
    policy
        .check_file(PolicyAction::Serve, "demo", &file("demo-1.0.tar.gz", Some(1)))
        .unwrap();
}
