use std::collections::BTreeMap;

use velodex_ecosystem_pypi::{
    CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Provenance, Yanked,
};

use crate::{PackageType, Policy, PolicyAction, PolicyConfig, PolicyConfigError};

#[test]
fn test_apply_list_filters_project_rules() {
    let policy = policy(|config| {
        config.block_projects = vec!["bad-pkg".to_owned()];
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
    let policy = policy(|config| {
        config.max_project_size_bytes = Some(10);
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
    let policy = policy(|config| {
        config.allow_projects = vec!["flask".to_owned()];
    });

    let denial = policy.check_project(PolicyAction::Serve, "django").unwrap_err();

    assert_eq!(denial.rule, "project-allow-list");
    assert_eq!(denial.field, "project");
    assert_eq!(denial.reason.as_ref(), "project \"django\" is not in the allow list");
}

#[test]
fn test_check_download_denies_unknown_version_when_versions_are_limited() {
    let policy = policy(|config| {
        config.allow_versions = Some(">=1".to_owned());
    });

    let denial = policy
        .check_download(PolicyAction::Serve, "not-a-dist.whl", Some(1))
        .unwrap_err();

    assert_eq!(denial.rule, "version-specifier");
    assert_eq!(denial.field, "version");
    assert_eq!(denial.reason.as_ref(), "file version is unknown");
}

#[test]
fn test_check_download_denies_unknown_package_type_when_types_are_limited() {
    let policy = policy(|config| {
        config.allow_package_types = vec![PackageType::Wheel];
    });

    let denial = policy
        .check_download(PolicyAction::Serve, "not-a-dist.whl", Some(1))
        .unwrap_err();

    assert_eq!(denial.rule, "package-type-allow-list");
    assert_eq!(denial.field, "package_type");
    assert_eq!(denial.reason.as_ref(), "package type is unknown");
}

#[test]
fn test_check_file_denies_blocked_wheel_package_type() {
    let policy = policy(|config| {
        config.block_package_types = vec![PackageType::Wheel];
    });

    let denial = policy
        .check_file(PolicyAction::Serve, "demo", &file("demo-1.0-py3-none-any.whl", Some(1)))
        .unwrap_err();

    assert_eq!(denial.rule, "package-type-block-list");
    assert_eq!(denial.field, "package_type");
    assert_eq!(denial.reason.as_ref(), "package type wheel is blocked");
}

#[test]
fn test_check_file_accepts_wheel_tag_allow_and_block_rules() {
    let policy = policy(|config| {
        config.allow_wheel_pythons = vec!["py3".to_owned()];
        config.block_wheel_pythons = vec!["cp39".to_owned()];
        config.allow_wheel_platforms = vec!["any".to_owned()];
        config.block_wheel_platforms = vec!["manylinux_2_28_x86_64".to_owned()];
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
fn test_check_file_denies_wheel_python_allow_list() {
    let policy = policy(|config| {
        config.allow_wheel_pythons = vec!["cp39".to_owned()];
    });

    let denial = policy
        .check_file(PolicyAction::Serve, "demo", &file("demo-1.0-py3-none-any.whl", Some(1)))
        .unwrap_err();

    assert_eq!(denial.rule, "wheel-python-allow-list");
    assert_eq!(denial.field, "wheel_python");
}

#[test]
fn test_check_file_denies_wheel_platform_block_list() {
    let policy = policy(|config| {
        config.block_wheel_platforms = vec!["any".to_owned()];
    });

    let denial = policy
        .check_file(PolicyAction::Serve, "demo", &file("demo-1.0-py3-none-any.whl", Some(1)))
        .unwrap_err();

    assert_eq!(denial.rule, "wheel-platform-block-list");
    assert_eq!(denial.field, "wheel_platform");
}

#[test]
fn test_policy_action_display_formats_mirror() {
    assert_eq!(PolicyAction::Proxy.to_string(), "proxy");
}

#[test]
fn test_apply_detail_accepts_project_size_under_limit() {
    let policy = policy(|config| {
        config.max_project_size_bytes = Some(10);
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
    let policy = policy(|config| {
        config.max_project_size_bytes = Some(10);
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
    let policy = policy(|config| {
        config.block_projects = vec!["blocked".to_owned()];
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
    let policy = policy(|config| {
        config.block_projects = vec!["blocked".to_owned()];
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
    let policy = policy(|config| {
        config.block_package_types = vec![PackageType::Sdist];
        config.max_project_size_bytes = Some(5);
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
    let config = PolicyConfig {
        allow_wheel_pythons: vec![String::new()],
        ..PolicyConfig::default()
    };

    assert!(matches!(
        Policy::compile(&config),
        Err(PolicyConfigError::EmptyTag(value)) if value.is_empty()
    ));
}

fn policy(configure: impl FnOnce(&mut PolicyConfig)) -> Policy {
    let mut config = PolicyConfig::default();
    configure(&mut config);
    Policy::compile(&config).unwrap()
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
