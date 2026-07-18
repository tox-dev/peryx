use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use peryx_policy::{
    ArtifactFacts, FallbackMode, Policy, PolicyAction, PolicyConfig, PolicyDecisionRecorder, PolicyDecisionState,
    PolicyEvaluation,
};
use rstest::rstest;

use crate::policy::{
    AttestationMode, PackageType, PypiPolicy, PypiPolicyConfig, PypiPolicyError, REQUIRED_ATTESTATION_AUDIT_RULE,
    REQUIRED_ATTESTATION_RULE, compile_rules,
};
use crate::{CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Provenance, Yanked};

#[test]
fn test_fallback_mode_defaults_to_inactive_filename_merge() {
    let policy = policy(|_neutral, _pypi| {});

    assert_eq!(policy.fallback_mode(), FallbackMode::Fallback);
    assert!(!policy.active());
}

#[rstest]
#[case(FallbackMode::PrivateFirst)]
#[case(FallbackMode::NoFallback)]
fn test_fallback_mode_activates_project_resolution(#[case] mode: FallbackMode) {
    let policy = policy(|_neutral, pypi| pypi.fallback_mode = mode);

    assert_eq!(policy.fallback_mode(), mode);
    assert!(policy.active());
}

#[rstest]
#[case("fallback", FallbackMode::Fallback)]
#[case("private-first", FallbackMode::PrivateFirst)]
#[case("no-fallback", FallbackMode::NoFallback)]
fn test_fallback_mode_deserializes_kebab_case(#[case] value: &str, #[case] expected: FallbackMode) {
    let config: PypiPolicyConfig = serde_json::from_str(&format!(r#"{{"fallback_mode":"{value}"}}"#)).unwrap();

    assert_eq!(config.fallback_mode, expected);
}

#[test]
fn test_fallback_mode_rejects_unknown_values() {
    serde_json::from_str::<PypiPolicyConfig>(r#"{"fallback_mode":"prefer-private"}"#).unwrap_err();
}

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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
fn test_protected_name_blocks_upstream_across_pep503_spellings() {
    let policy = policy(|neutral, _pypi| {
        neutral.protected_names = vec!["Acme.Secrets".to_owned(), "acme_internal_*".to_owned()];
    });

    for spelling in ["acme-secrets", "Acme_Secrets", "acme.secrets", "acme-internal-db"] {
        assert!(
            policy
                .check_project(PolicyAction::Cached, &crate::normalize_name(spelling))
                .is_err(),
            "{spelling} should not fall back upstream"
        );
    }
    assert_eq!(policy.check_project(PolicyAction::Serve, "acme-secrets"), Ok(()));
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

const WEEK_SECS: u64 = 604_800;
const NOW: i64 = 1_768_003_200; // 2026-01-10T00:00:00Z

fn delay_policy() -> Policy {
    policy(|_neutral, pypi| pypi.min_release_age_secs = Some(WEEK_SECS))
}

fn aged_facts(upload_time: Option<i64>, now: Option<i64>) -> ArtifactFacts {
    ArtifactFacts {
        project: "demo".to_owned(),
        filename: Some("demo-1.0-py3-none-any.whl".to_owned()),
        upload_time,
        now,
        ..ArtifactFacts::default()
    }
}

fn file_at(filename: &str, upload_time: &str) -> File {
    File {
        upload_time: Some(upload_time.to_owned()),
        ..file(filename, Some(1))
    }
}

#[test]
fn test_release_delay_denies_a_release_inside_the_window() {
    let denial = delay_policy()
        .check_facts(PolicyAction::Serve, &aged_facts(Some(NOW - 100), Some(NOW)))
        .unwrap_err();

    assert_eq!(denial.rule, "release-delay");
    assert_eq!(denial.field, "upload_time");
    assert_eq!(
        denial.to_string(),
        "release is 100s old, within the 604800s upstream delay"
    );
}

#[test]
fn test_release_delay_denies_a_missing_upload_time() {
    let denial = delay_policy()
        .check_facts(PolicyAction::Serve, &aged_facts(None, Some(NOW)))
        .unwrap_err();

    assert_eq!(denial.rule, "release-delay");
    assert_eq!(denial.to_string(), "release has no upstream upload time to age against");
}

#[test]
fn test_release_delay_verdict_by_age_and_clock() {
    struct Case {
        label: &'static str,
        upload_time: Option<i64>,
        now: Option<i64>,
        allowed: bool,
    }
    let cases = [
        Case {
            label: "aged past the window",
            upload_time: Some(NOW - 604_801),
            now: Some(NOW),
            allowed: true,
        },
        // Eligible the instant the delay elapses.
        Case {
            label: "exactly at the window",
            upload_time: Some(NOW - 604_800),
            now: Some(NOW),
            allowed: true,
        },
        Case {
            label: "one second inside the window",
            upload_time: Some(NOW - 604_799),
            now: Some(NOW),
            allowed: false,
        },
        // No serve clock: the delay cannot be evaluated, so the release passes.
        Case {
            label: "no serve clock",
            upload_time: Some(NOW - 1),
            now: None,
            allowed: true,
        },
    ];

    for case in cases {
        assert_eq!(
            delay_policy()
                .check_facts(PolicyAction::Serve, &aged_facts(case.upload_time, case.now))
                .is_ok(),
            case.allowed,
            "{}",
            case.label,
        );
    }
}

#[test]
fn test_release_delay_zero_age_stays_inactive() {
    let policy = policy(|_neutral, pypi| pypi.min_release_age_secs = Some(0));
    assert!(!policy.active());
}

#[test]
fn test_release_delay_clamps_an_out_of_range_age() {
    // A delay past i64 saturates to the max, so every real release stays quarantined.
    let policy = policy(|_neutral, pypi| pypi.min_release_age_secs = Some(u64::MAX));
    policy
        .check_facts(PolicyAction::Serve, &aged_facts(Some(NOW - 1), Some(NOW)))
        .unwrap_err();
}

#[test]
fn test_apply_detail_hides_a_young_release_and_keeps_an_aged_one() {
    let detail = delay_policy()
        .apply_detail(
            PolicyAction::Serve,
            "demo",
            ProjectDetail {
                meta: Meta::default(),
                name: "demo".to_owned(),
                versions: vec!["1.0".to_owned(), "2.0".to_owned()],
                files: vec![
                    file_at("demo-1.0-py3-none-any.whl", "2026-01-01T00:00:00Z"),
                    file_at("demo-2.0-py3-none-any.whl", "2026-01-08T00:00:00Z"),
                ],
            },
            Some(NOW),
        )
        .unwrap();

    let names: Vec<&str> = detail.files.iter().map(|file| file.filename.as_str()).collect();
    assert_eq!(names, ["demo-1.0-py3-none-any.whl"]);
    assert_eq!(detail.versions, ["1.0"]);
}

#[test]
fn test_apply_detail_hides_a_release_with_an_unparseable_upload_time() {
    let detail = delay_policy()
        .apply_detail(
            PolicyAction::Serve,
            "demo",
            ProjectDetail {
                meta: Meta::default(),
                name: "demo".to_owned(),
                versions: vec!["1.0".to_owned()],
                files: vec![file_at("demo-1.0-py3-none-any.whl", "not-a-timestamp")],
            },
            Some(NOW),
        )
        .unwrap();

    assert!(detail.files.is_empty());
}

#[test]
fn test_apply_detail_serves_a_young_release_without_a_clock() {
    let detail = delay_policy()
        .apply_detail(
            PolicyAction::Serve,
            "demo",
            ProjectDetail {
                meta: Meta::default(),
                name: "demo".to_owned(),
                versions: vec!["2.0".to_owned()],
                files: vec![file_at("demo-2.0-py3-none-any.whl", "2026-01-08T00:00:00Z")],
            },
            None,
        )
        .unwrap();

    assert_eq!(detail.files.len(), 1);
}

const PUBLISH_PREDICATE: &str = "https://docs.pypi.org/attestations/publish/v1";
const SLSA_PREDICATE: &str = "https://slsa.dev/provenance/v1";

fn attestation_policy(mode: AttestationMode, required: &[&str]) -> Policy {
    policy(|_neutral, pypi| {
        pypi.attestation_mode = mode;
        pypi.required_attestations = required.iter().map(|value| (*value).to_owned()).collect();
    })
}

fn predicate_types(types: &[&str]) -> BTreeSet<String> {
    types.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn test_required_attestation_allows_an_upload_carrying_every_predicate_type() {
    attestation_policy(AttestationMode::Enforce, &[PUBLISH_PREDICATE, SLSA_PREDICATE])
        .check_upload(
            PolicyAction::Upload,
            "demo",
            &file("demo-1.0-py3-none-any.whl", Some(1)),
            &predicate_types(&[
                PUBLISH_PREDICATE,
                SLSA_PREDICATE,
                "https://slsa.dev/verification_summary/v1",
            ]),
        )
        .unwrap();
}

#[test]
fn test_required_attestation_denies_an_upload_missing_a_predicate_type() {
    let denial = attestation_policy(AttestationMode::Enforce, &[PUBLISH_PREDICATE, SLSA_PREDICATE])
        .check_upload(
            PolicyAction::Upload,
            "demo",
            &file("demo-1.0-py3-none-any.whl", Some(1)),
            &predicate_types(&[PUBLISH_PREDICATE]),
        )
        .unwrap_err();

    assert_eq!(denial.rule, REQUIRED_ATTESTATION_RULE);
    assert_eq!(denial.field, "attestations");
    assert_eq!(
        denial.reason.as_ref(),
        format!("upload is missing a required attestation predicate type: {SLSA_PREDICATE}")
    );
}

#[test]
fn test_required_attestation_denies_an_upload_with_no_attestations() {
    let denial = attestation_policy(AttestationMode::Enforce, &[PUBLISH_PREDICATE])
        .check_upload(
            PolicyAction::Upload,
            "demo",
            &file("demo-1.0-py3-none-any.whl", Some(1)),
            &BTreeSet::new(),
        )
        .unwrap_err();

    assert_eq!(denial.rule, REQUIRED_ATTESTATION_RULE);
    assert_eq!(
        denial.reason.as_ref(),
        format!("upload is missing a required attestation predicate type: {PUBLISH_PREDICATE}")
    );
}

#[test]
fn test_required_attestation_audit_mode_names_the_audit_rule() {
    let denial = attestation_policy(AttestationMode::Audit, &[PUBLISH_PREDICATE])
        .check_upload(
            PolicyAction::Upload,
            "demo",
            &file("demo-1.0-py3-none-any.whl", Some(1)),
            &BTreeSet::new(),
        )
        .unwrap_err();

    assert_eq!(denial.rule, REQUIRED_ATTESTATION_AUDIT_RULE);
}

#[test]
fn test_required_attestation_ignores_a_serve_fact_that_carries_no_attestations() {
    // A serve or catalog path builds no attestation attribute, so the requirement cannot judge it and
    // the file passes; the rule only speaks at the upload boundary.
    attestation_policy(AttestationMode::Enforce, &[PUBLISH_PREDICATE])
        .check_file(PolicyAction::Serve, "demo", &file("demo-1.0-py3-none-any.whl", Some(1)))
        .unwrap();
}

#[test]
fn test_required_attestation_reports_a_structural_denial_before_itself() {
    // The attestation rule is compiled last, so a blocked package type is what an upload missing both
    // the type allowance and its attestations hears.
    let policy = policy(|_neutral, pypi| {
        pypi.block_package_types = vec![PackageType::Wheel];
        pypi.required_attestations = vec![PUBLISH_PREDICATE.to_owned()];
    });

    let denial = policy
        .check_upload(
            PolicyAction::Upload,
            "demo",
            &file("demo-1.0-py3-none-any.whl", Some(1)),
            &BTreeSet::new(),
        )
        .unwrap_err();

    assert_eq!(denial.rule, "package-type-block-list");
}

#[test]
fn test_compile_rejects_an_empty_predicate_type() {
    let config = PypiPolicyConfig {
        required_attestations: vec![String::new()],
        ..PypiPolicyConfig::default()
    };

    assert!(matches!(
        compile_rules(&config),
        Err(PypiPolicyError::EmptyPredicateType)
    ));
}

#[rstest]
#[case("enforce", AttestationMode::Enforce)]
#[case("audit", AttestationMode::Audit)]
fn test_attestation_mode_deserializes_kebab_case(#[case] value: &str, #[case] expected: AttestationMode) {
    let config: PypiPolicyConfig = serde_json::from_str(&format!(r#"{{"attestation_mode":"{value}"}}"#)).unwrap();

    assert_eq!(config.attestation_mode, expected);
}

#[test]
fn test_attestation_mode_defaults_to_enforce() {
    assert_eq!(PypiPolicyConfig::default().attestation_mode, AttestationMode::Enforce);
}

#[derive(Debug, Default)]
struct CaptureRecorder(Mutex<Vec<(PolicyDecisionState, Option<String>)>>);

impl PolicyDecisionRecorder for CaptureRecorder {
    fn record(&self, evaluation: PolicyEvaluation<'_>) {
        self.0
            .lock()
            .unwrap()
            .push((evaluation.state, evaluation.rule.map(str::to_owned)));
    }
}

#[rstest]
#[case(AttestationMode::Enforce, REQUIRED_ATTESTATION_RULE)]
#[case(AttestationMode::Audit, REQUIRED_ATTESTATION_AUDIT_RULE)]
fn test_required_attestation_persists_the_unmet_decision(#[case] mode: AttestationMode, #[case] rule: &str) {
    let recorder = Arc::new(CaptureRecorder::default());
    let policy = attestation_policy(mode, &[PUBLISH_PREDICATE]).with_decision_recorder(recorder.clone());

    policy
        .check_upload(
            PolicyAction::Upload,
            "demo",
            &file("demo-1.0-py3-none-any.whl", Some(1)),
            &BTreeSet::new(),
        )
        .unwrap_err();

    assert_eq!(
        *recorder.0.lock().unwrap(),
        vec![(PolicyDecisionState::Deny, Some(rule.to_owned()))]
    );
}
