use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use peryx_driver::serving::{MirrorAction, MirrorDriver as _, MirrorRequest};
use peryx_index::{Index, IndexKind};
use rstest::rstest;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    ProjectAdmission, admission, all_projects, candidates, content_type_is_json, include_target, logical_lines,
    parse_selector, refusal, requirement_line, selection, strip_comment, tags_allowed, target, target_upstream,
    wheel_tags_allowed,
};
use crate::mirror::test_support::{self, cached_index, hosted_index};
use crate::mirror::{
    ArtifactFilters, FileCandidate, PrefetchConfig, PrefetchMode, PrefetchOptions, ProjectRule, SelectionSource,
};
use crate::policy::{FallbackMode, PackageType};
use crate::store::PypiStore as _;
use crate::{CoreMetadata, File, Meta, ProjectDetail, Provenance, Yanked, parse_version_specifiers};

fn config(mode: PrefetchMode) -> PrefetchConfig {
    PrefetchConfig {
        mode,
        packages: Vec::new(),
        requirements: Vec::new(),
        include_wheels: true,
        include_sdists: true,
        python_tags: Vec::new(),
        abi_tags: Vec::new(),
        platform_tags: Vec::new(),
        max_file_size_bytes: None,
        metadata_only: false,
    }
}

fn options() -> PrefetchOptions {
    PrefetchOptions {
        packages: Vec::new(),
        requirements: Vec::new(),
        mode: None,
        metadata_only: false,
        no_wheels: false,
        no_sdists: false,
        python_tags: Vec::new(),
        abi_tags: Vec::new(),
        platform_tags: Vec::new(),
        max_file_size_bytes: None,
    }
}

#[test]
fn project_rule_allows_unrestricted_versions() {
    assert!(ProjectRule::default().allows(&"1.0".parse().unwrap()));
}

#[test]
fn project_rule_allows_matching_versions() {
    let rule = ProjectRule {
        specs: vec![Some(">=1".parse().unwrap())],
    };
    assert!(rule.allows(&"1.0".parse().unwrap()));
}

#[test]
fn project_rule_rejects_nonmatching_versions() {
    let rule = ProjectRule {
        specs: vec![Some(">=2".parse().unwrap())],
    };
    assert!(!rule.allows(&"1.0".parse().unwrap()));
}

#[test]
fn project_rule_allows_an_unversioned_selector() {
    let rule = ProjectRule { specs: vec![None] };
    assert!(rule.allows(&"1.0".parse().unwrap()));
}

fn filters() -> ArtifactFilters {
    ArtifactFilters {
        include_wheels: true,
        include_sdists: true,
        python_tags: BTreeSet::new(),
        abi_tags: BTreeSet::new(),
        platform_tags: BTreeSet::new(),
        max_file_size_bytes: None,
        metadata_only: false,
    }
}

fn file(filename: &str, digest: Option<&str>, size: Option<u64>, metadata: CoreMetadata) -> File {
    File {
        filename: filename.to_owned(),
        url: format!("https://example.test/{filename}"),
        hashes: digest
            .map(|digest| BTreeMap::from([("sha256".to_owned(), digest.to_owned())]))
            .unwrap_or_default(),
        requires_python: None,
        size,
        upload_time: None,
        yanked: Yanked::No,
        core_metadata: metadata.clone(),
        dist_info_metadata: metadata,
        gpg_sig: None,
        provenance: Provenance::Absent,
    }
}

#[rstest]
#[case("demo", "demo", false)]
#[case("Demo[fast]>=1; python_version > '3.8'", "demo", true)]
fn selectors_parse_names_extras_markers_and_versions(#[case] raw: &str, #[case] project: &str, #[case] has_spec: bool) {
    let selector = parse_selector(raw).unwrap();
    assert_eq!(selector.project, project);
    assert_eq!(selector.spec.is_some(), has_spec);
}

#[rstest]
#[case("")]
#[case("demo @ https://example.test/demo.whl")]
#[case("bad/name")]
#[case("demo>>1")]
fn selectors_reject_unsupported_forms(#[case] raw: &str) {
    assert!(parse_selector(raw).is_err());
}

#[rstest]
#[case("-r child.txt", Some("child.txt"))]
#[case("--requirement=child.txt", Some("child.txt"))]
#[case("-cconstraints.txt", Some("constraints.txt"))]
#[case("--requirementchild.txt", None)]
#[case("-r", None)]
fn include_targets_match_pip_forms(#[case] line: &str, #[case] expected: Option<&str>) {
    assert_eq!(include_target(line), expected);
}

#[test]
fn requirement_files_join_lines_and_strip_comments() {
    assert_eq!(
        logical_lines("name#fragment # comment\ntrailing\\"),
        ["name#fragment", "trailing"]
    );
    assert_eq!(strip_comment("name\t# comment"), "name\t");
    assert_eq!(requirement_line("demo>=1 --hash=sha256:abc"), "demo>=1");
}

#[tokio::test]
async fn mirror_rejects_requirement_cycle_through_parent_alias() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("requirements.txt");
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(&root, "-r sub/../requirements.txt\n").unwrap();

    assert_eq!(
        mirror_requirements(&[&root]).await.unwrap_err(),
        format!(
            "requirements include cycle: {} -> {}",
            root.display(),
            dir.path().join("sub/../requirements.txt").display()
        )
    );
}

#[cfg(unix)]
#[tokio::test]
async fn mirror_rejects_requirement_cycle_through_symlink_alias() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("requirements.txt");
    let alias = dir.path().join("alias.txt");
    std::fs::write(&root, "-r alias.txt\n").unwrap();
    std::os::unix::fs::symlink(&root, &alias).unwrap();

    assert_eq!(
        mirror_requirements(&[&root]).await.unwrap_err(),
        format!("requirements include cycle: {} -> {}", root.display(), alias.display())
    );
}

#[tokio::test]
async fn mirror_accepts_a_noncyclic_repeated_requirement_include() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("requirements.txt");
    std::fs::write(&root, "-r left.txt\n-r right.txt\n").unwrap();
    std::fs::write(dir.path().join("left.txt"), "-r shared.txt\n").unwrap();
    std::fs::write(dir.path().join("right.txt"), "-r shared.txt\n").unwrap();
    std::fs::write(dir.path().join("shared.txt"), "demo==1\n").unwrap();

    assert_eq!(mirror_requirements(&[&root]).await.unwrap(), missing_demo_output());
}

#[tokio::test]
async fn mirror_reads_a_top_level_requirement_file_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("requirements.txt");
    std::fs::write(&root, "demo==1\n").unwrap();

    assert_eq!(
        mirror_requirements(&[&root, &root]).await.unwrap(),
        missing_demo_output()
    );
}

#[tokio::test]
async fn mirror_joins_a_requirement_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("requirements.txt");
    std::fs::write(&root, "demo>=1,\\\n<2\n").unwrap();

    assert_eq!(mirror_requirements(&[&root]).await.unwrap(), missing_demo_output());
}

#[tokio::test]
async fn mirror_reports_a_missing_include_by_its_resolved_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("requirements.txt");
    std::fs::write(&root, "-r nested/missing.txt\n").unwrap();

    assert_eq!(
        mirror_requirements(&[&root]).await.unwrap_err(),
        format!("read requirements {}", dir.path().join("nested/missing.txt").display())
    );
}

async fn mirror_requirements(paths: &[&Path]) -> Result<String, String> {
    let fixture = test_support::state(vec![cached_index("https://example.invalid/simple/", true)]);
    let configured = toml::Table::new();
    let mut overrides = toml::Table::new();
    overrides.insert(
        "requirements".to_owned(),
        toml::Value::Array(
            paths
                .iter()
                .map(|path| toml::Value::String(path.display().to_string()))
                .collect(),
        ),
    );
    let mut output = Vec::new();
    crate::PypiServing
        .mirror(
            fixture.state,
            MirrorRequest {
                action: MirrorAction::Plan,
                index: "pypi",
                settings: &configured,
                configured: &configured,
                overrides: &overrides,
            },
            &mut output,
        )
        .await?;
    Ok(String::from_utf8(output).expect("mirror output is UTF-8"))
}

fn missing_demo_output() -> &'static str {
    concat!(
        "kind\tindex\tproject\tfilename\tdigest\turl\tbytes\tstatus\treason\n",
        "page\tpypi\tdemo\t\t\t\t\tskipped\tproject not found\n",
        "summary\tpypi\t\tprojects\t\t\t1\tprojects\t\n",
        "summary\tpypi\t\tfiles\t\t\t0\tfiles\t\n",
        "summary\tpypi\t\tskipped\t\t\t1\tskipped\t\n",
        "summary\tpypi\t\tfailures\t\t\t0\tfailures\t\n",
    )
}

#[test]
fn candidates_report_each_filter_decision() {
    let digest = "a".repeat(64);
    let metadata = CoreMetadata::Hashes(BTreeMap::from([("sha256".to_owned(), "b".repeat(64))]));
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: "demo".to_owned(),
        versions: vec!["1.0".to_owned(), "2.0".to_owned()],
        files: vec![
            file("demo-1.0-py3-none-any.whl", None, Some(1), CoreMetadata::Absent),
            file("unknown.bin", Some(&digest), Some(1), CoreMetadata::Absent),
            file("demo-1.0-py3-none-any.whl", Some(&digest), Some(1), metadata),
            file("demo-2.0.tar.gz", Some(&digest), Some(20), CoreMetadata::Absent),
            file("demo-1.0.tar.gz", Some(&digest), Some(1), CoreMetadata::Absent),
        ],
    };
    let mut filters = filters();
    filters.python_tags.insert("cp312".to_owned());
    filters.max_file_size_bytes = Some(10);
    let rule = ProjectRule {
        specs: vec![Some(parse_version_specifiers("<2").unwrap())],
    };

    let outcomes = candidates(&detail, Some(&rule), &filters, &admitted())
        .map(candidate_outcome)
        .collect::<Vec<_>>();

    assert_eq!(
        outcomes,
        [
            ("demo-1.0-py3-none-any.whl".to_owned(), "missing sha256".to_owned()),
            ("unknown.bin".to_owned(), "unsupported filename".to_owned()),
            ("demo-1.0-py3-none-any.whl".to_owned(), "wheel tag filtered".to_owned()),
            ("demo-2.0.tar.gz".to_owned(), "size filtered".to_owned()),
            ("demo-1.0.tar.gz".to_owned(), "included".to_owned()),
        ]
    );
}

#[test]
fn artifact_filters_cover_disabled_types_tags_and_versions() {
    let mut filters = filters();
    filters.include_wheels = false;
    filters.include_sdists = false;
    let digest = "a".repeat(64);
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: "demo".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![
            file(
                "demo-1.0-py3-none-any.whl",
                Some(&digest),
                Some(1),
                CoreMetadata::Absent,
            ),
            file("demo-1.0.tar.gz", Some(&digest), Some(1), CoreMetadata::Absent),
        ],
    };
    let reasons = candidates(&detail, None, &filters, &admitted())
        .map(candidate_outcome)
        .collect::<Vec<_>>();

    assert_eq!(
        reasons,
        [
            ("demo-1.0-py3-none-any.whl".to_owned(), "wheels disabled".to_owned()),
            ("demo-1.0.tar.gz".to_owned(), "sdists disabled".to_owned()),
        ]
    );
    assert!(tags_allowed("py3.cp312", &BTreeSet::from(["cp312".to_owned()])));
    assert!(wheel_tags_allowed(
        "demo-1.0-py3-none-any.whl",
        &ArtifactFilters {
            include_wheels: true,
            include_sdists: true,
            python_tags: BTreeSet::new(),
            abi_tags: BTreeSet::new(),
            platform_tags: BTreeSet::new(),
            max_file_size_bytes: None,
            metadata_only: false
        }
    ));
}

fn candidate_outcome(candidate: FileCandidate) -> (String, String) {
    match candidate {
        FileCandidate::Include(file) => (file.filename, "included".to_owned()),
        FileCandidate::Skip(file, reason) => (file.filename, reason.into_owned()),
    }
}

fn admitted() -> ProjectAdmission {
    ProjectAdmission {
        project: None,
        files: BTreeMap::new(),
    }
}

#[tokio::test]
async fn selection_merges_options_and_reads_cached_catalogs() {
    let fixture = test_support::state(vec![cached_index("https://example.test/simple/", true)]);
    fixture.state.serving.meta.put_project("pypi", "Demo", "Demo").unwrap();
    let target = target(&config(PrefetchMode::All), &fixture.state.serving, "pypi").unwrap();
    let mut overrides = options();
    overrides.mode = Some(PrefetchMode::MetadataOnly);
    overrides.packages.push("Other>=1".to_owned());
    overrides.no_wheels = true;
    overrides.max_file_size_bytes = Some(9);

    let selected = selection(&fixture.state.serving, &target, &overrides, SelectionSource::Cache)
        .await
        .unwrap();
    assert_eq!(selected.projects, ["other"]);
    assert!(selected.filters.metadata_only);
    assert!(!selected.filters.include_wheels);
    assert_eq!(selected.filters.max_file_size_bytes, Some(9));

    let cached = all_projects(&fixture.state.serving, &target, SelectionSource::Cache)
        .await
        .unwrap();
    assert_eq!(cached, ["demo"]);
    assert!(fixture.dir.path().exists());
}

#[tokio::test]
async fn selection_applies_all_cli_filters_and_requires_selected_packages() {
    let fixture = test_support::state(vec![cached_index("https://example.test/simple/", true)]);
    let target = target(&config(PrefetchMode::Selected), &fixture.state.serving, "pypi").unwrap();
    assert!(
        selection(&fixture.state.serving, &target, &options(), SelectionSource::Cache)
            .await
            .is_err()
    );
    let mut overrides = options();
    overrides.requirements.push({
        let path = fixture.dir.path().join("requirements.txt");
        std::fs::write(&path, "demo>=1\n").unwrap();
        path
    });
    overrides.no_sdists = true;
    overrides.python_tags.push("py3".to_owned());
    overrides.abi_tags.push("none".to_owned());
    overrides.platform_tags.push("any".to_owned());
    let selected = selection(&fixture.state.serving, &target, &overrides, SelectionSource::Cache)
        .await
        .unwrap();
    assert_eq!(selected.projects, ["demo"]);
    assert!(!selected.filters.include_sdists);
}

#[tokio::test]
async fn all_projects_syncs_and_reports_upstream_catalog_failures() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Demo"}]}"#,
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", server.uri()), false)]);
    let selected_target = target(&config(PrefetchMode::All), &fixture.state.serving, "pypi").unwrap();
    assert_eq!(
        all_projects(&fixture.state.serving, &selected_target, SelectionSource::Upstream)
            .await
            .unwrap(),
        ["demo"]
    );

    let failing = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&failing)
        .await;
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", failing.uri()), false)]);
    let target = target(&config(PrefetchMode::All), &fixture.state.serving, "pypi").unwrap();
    assert!(
        all_projects(&fixture.state.serving, &target, SelectionSource::Upstream)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn all_projects_accepts_html_catalogs() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"<!doctype html><a href="/simple/demo/">Demo</a>"#, "text/html"),
        )
        .mount(&server)
        .await;
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", server.uri()), false)]);
    let selected_target = target(&config(PrefetchMode::All), &fixture.state.serving, "pypi").unwrap();
    assert_eq!(
        all_projects(&fixture.state.serving, &selected_target, SelectionSource::Upstream)
            .await
            .unwrap(),
        ["demo"]
    );
}

#[tokio::test]
async fn all_projects_reuses_an_unmodified_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "catalog-v1")
                .set_body_raw(
                    r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Demo"}]}"#,
                    "application/vnd.pypi.simple.v1+json",
                ),
        )
        .with_priority(10)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .and(header("if-none-match", "catalog-v1"))
        .respond_with(ResponseTemplate::new(304))
        .with_priority(1)
        .mount(&server)
        .await;
    let fixture = test_support::state(vec![cached_index(&format!("{}/simple/", server.uri()), false)]);
    let selected_target = target(&config(PrefetchMode::All), &fixture.state.serving, "pypi").unwrap();
    all_projects(&fixture.state.serving, &selected_target, SelectionSource::Upstream)
        .await
        .unwrap();
    assert_eq!(
        all_projects(&fixture.state.serving, &selected_target, SelectionSource::Upstream)
            .await
            .unwrap(),
        ["demo"]
    );
}

#[test]
fn candidate_reports_version_filters() {
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: "demo".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![file(
            "demo-1.0.tar.gz",
            Some(&"a".repeat(64)),
            Some(1),
            CoreMetadata::Absent,
        )],
    };
    let rule = ProjectRule {
        specs: vec![Some(parse_version_specifiers(">=2").unwrap())],
    };
    assert_eq!(
        candidate_outcome(
            candidates(&detail, Some(&rule), &filters(), &admitted())
                .next()
                .unwrap()
        ),
        ("demo-1.0.tar.gz".to_owned(), "version filtered".to_owned())
    );
    assert_eq!(include_target("--requirement=demo.txt"), Some("demo.txt"));
    assert_eq!(include_target("--requirement="), None);
}

#[test]
fn wheel_filters_require_each_configured_tag() {
    let filters = ArtifactFilters {
        include_wheels: true,
        include_sdists: true,
        python_tags: BTreeSet::from(["py3".to_owned()]),
        abi_tags: BTreeSet::from(["none".to_owned()]),
        platform_tags: BTreeSet::from(["any".to_owned()]),
        max_file_size_bytes: None,
        metadata_only: false,
    };
    assert!(wheel_tags_allowed("demo-1.0-py3-none-any.whl", &filters));
}

#[test]
fn virtual_targets_reject_multiple_cached_members() {
    let virtual_index = Index {
        name: "root".to_owned(),
        route: "root".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Virtual {
            layers: vec![0, 1],
            write_target: None,
        },
        policy: peryx_policy::Policy::default(),
        acl: peryx_identity::IndexAcl::default(),
    };
    let fixture = test_support::state(vec![
        cached_index("https://one.test/simple/", false),
        Index {
            name: "second".to_owned(),
            ..cached_index("https://two.test/simple/", false)
        },
        virtual_index,
    ]);
    assert!(target(&config(PrefetchMode::All), &fixture.state.serving, "root").is_err());
}

#[test]
fn targets_accept_cached_and_single_cached_virtual_indexes() {
    let cached = cached_index("https://example.test/simple/", false);
    let hosted = hosted_index("hosted");
    let virtual_index = Index {
        name: "root".to_owned(),
        route: "root".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Virtual {
            layers: vec![1, 0],
            write_target: Some(1),
        },
        policy: peryx_policy::Policy::default(),
        acl: peryx_identity::IndexAcl::default(),
    };
    let fixture = test_support::state(vec![cached, hosted, virtual_index]);
    let configured = config(PrefetchMode::Selected);

    assert_eq!(
        target(&configured, &fixture.state.serving, "pypi").unwrap().cached,
        "pypi"
    );
    assert_eq!(
        target(&configured, &fixture.state.serving, "root").unwrap().cached,
        "pypi"
    );
    assert!(target(&configured, &fixture.state.serving, "hosted").is_err());
    assert!(target(&configured, &fixture.state.serving, "missing").is_err());
    assert!(content_type_is_json(None));
    assert!(content_type_is_json(Some("application/json")));
    assert!(!content_type_is_json(Some("text/html")));
    assert!(target_upstream(&fixture.state.serving, 1).is_err());
}

/// One wheel and one sdist of the same release, so a per-file rule and a release-wide rule that sums
/// their sizes can be told apart.
fn release() -> ProjectDetail {
    let digest = "a".repeat(64);
    ProjectDetail {
        meta: Meta::default(),
        name: "demo".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![
            file(
                "demo-1.0-py3-none-any.whl",
                Some(&digest),
                Some(4),
                CoreMetadata::Absent,
            ),
            file("demo-1.0.tar.gz", Some(&digest), Some(1), CoreMetadata::Absent),
        ],
    }
}

fn outcomes(detail: &ProjectDetail, admission: &ProjectAdmission) -> Vec<(String, String)> {
    candidates(detail, None, &filters(), admission)
        .map(candidate_outcome)
        .collect()
}

#[test]
fn admission_reports_the_file_the_cached_member_refuses() {
    let fixture = test_support::state(vec![Index {
        policy: test_support::policy(|_neutral, pypi| pypi.block_package_types = vec![PackageType::Sdist]),
        ..cached_index("https://example.test/simple/", false)
    }]);
    let state = &fixture.state.serving;
    let target = target(&config(PrefetchMode::Selected), state, "pypi").unwrap();
    let detail = release();

    let admission = admission(state, &target, "demo", &detail);

    assert_eq!(admission.refusal(), None);
    assert_eq!(
        outcomes(&detail, &admission),
        [
            ("demo-1.0-py3-none-any.whl".to_owned(), "included".to_owned()),
            (
                "demo-1.0.tar.gz".to_owned(),
                "cached policy: package type sdist is blocked".to_owned()
            ),
        ]
    );
}

#[test]
fn admission_reports_the_file_a_virtual_target_would_not_serve() {
    let fixture = test_support::state(vec![
        cached_index("https://example.test/simple/", false),
        test_support::virtual_index(
            "root",
            vec![0],
            test_support::policy(|_neutral, pypi| pypi.block_package_types = vec![PackageType::Sdist]),
        ),
    ]);
    let state = &fixture.state.serving;
    let target = target(&config(PrefetchMode::Selected), state, "root").unwrap();
    let detail = release();

    let admission = admission(state, &target, "demo", &detail);

    assert_eq!(
        outcomes(&detail, &admission),
        [
            ("demo-1.0-py3-none-any.whl".to_owned(), "included".to_owned()),
            (
                "demo-1.0.tar.gz".to_owned(),
                "serve policy: package type sdist is blocked".to_owned()
            ),
        ]
    );
}

#[test]
fn admission_withdraws_a_project_a_release_wide_rule_rejects() {
    let fixture = test_support::state(vec![Index {
        policy: test_support::policy(|_neutral, pypi| pypi.max_project_size_bytes = Some(4)),
        ..cached_index("https://example.test/simple/", false)
    }]);
    let state = &fixture.state.serving;
    let target = target(&config(PrefetchMode::Selected), state, "pypi").unwrap();

    let admission = admission(state, &target, "demo", &release());

    assert_eq!(
        admission.refusal(),
        Some("cached policy: project size 5 exceeds limit 4")
    );
}

#[test]
fn admission_measures_a_release_wide_rule_against_the_admitted_siblings() {
    let fixture = test_support::state(vec![
        Index {
            policy: test_support::policy(|_neutral, pypi| pypi.block_package_types = vec![PackageType::Sdist]),
            ..cached_index("https://example.test/simple/", false)
        },
        test_support::virtual_index(
            "root",
            vec![0],
            test_support::policy(|_neutral, pypi| pypi.max_project_size_bytes = Some(4)),
        ),
    ]);
    let state = &fixture.state.serving;
    let target = target(&config(PrefetchMode::Selected), state, "root").unwrap();
    let detail = release();

    let admission = admission(state, &target, "demo", &detail);

    assert_eq!(admission.refusal(), None);
    assert_eq!(
        outcomes(&detail, &admission),
        [
            ("demo-1.0-py3-none-any.whl".to_owned(), "included".to_owned()),
            (
                "demo-1.0.tar.gz".to_owned(),
                "cached policy: package type sdist is blocked".to_owned()
            ),
        ]
    );
}

#[test]
fn refusal_reports_a_protected_project_before_any_upstream_request() {
    let fixture = test_support::state(vec![Index {
        policy: test_support::policy(|_neutral, pypi| pypi.protected_names = vec!["demo".to_owned()]),
        ..cached_index("https://example.test/simple/", false)
    }]);
    let state = &fixture.state.serving;
    let target = target(&config(PrefetchMode::Selected), state, "pypi").unwrap();

    assert_eq!(
        refusal(state, &target, "demo").as_deref(),
        Some("cached policy: project \"demo\" is protected from upstream fallback")
    );
    assert_eq!(refusal(state, &target, "other"), None);
}

#[test]
fn refusal_reports_a_project_the_target_blocks() {
    let fixture = test_support::state(vec![
        cached_index("https://example.test/simple/", false),
        test_support::virtual_index(
            "root",
            vec![0],
            test_support::policy(|_neutral, pypi| pypi.block_projects = vec!["demo".to_owned()]),
        ),
    ]);
    let state = &fixture.state.serving;
    let target = target(&config(PrefetchMode::Selected), state, "root").unwrap();

    assert_eq!(
        refusal(state, &target, "demo").as_deref(),
        Some("serve policy: project \"demo\" is blocked")
    );
}

#[rstest]
#[case::no_fallback(
    FallbackMode::NoFallback,
    false,
    Some("virtual policy: cached members excluded by fallback")
)]
#[case::private_first_shadowed(
    FallbackMode::PrivateFirst,
    true,
    Some("virtual policy: cached members excluded by fallback")
)]
#[case::private_first_unshadowed(FallbackMode::PrivateFirst, false, None)]
#[case::fallback(FallbackMode::Fallback, true, None)]
fn refusal_follows_a_virtual_targets_source_policy(
    #[case] mode: FallbackMode,
    #[case] hosted_publishes: bool,
    #[case] expected: Option<&str>,
) {
    let fixture = test_support::state(vec![
        cached_index("https://example.test/simple/", false),
        hosted_index("private"),
        test_support::virtual_index(
            "root",
            vec![1, 0],
            test_support::policy(|_neutral, pypi| pypi.fallback_mode = mode),
        ),
    ]);
    let state = &fixture.state.serving;
    if hosted_publishes {
        publish(state, "demo-1.0-py3-none-any.whl");
    }
    let target = target(&config(PrefetchMode::Selected), state, "root").unwrap();

    assert_eq!(refusal(state, &target, "demo").as_deref(), expected);
}

fn publish(state: &peryx_driver::ServingState, filename: &str) {
    let uploaded = crate::upload::Uploaded {
        version: "1.0".to_owned(),
        file: file(filename, Some(&"a".repeat(64)), Some(1), CoreMetadata::Absent),
        trashed: None,
    };
    state
        .meta
        .put_upload("private", "demo", filename, &serde_json::to_vec(&uploaded).unwrap())
        .unwrap();
}
