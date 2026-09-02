use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::mem::size_of;

use peryx_driver::ScanCancellation;
use peryx_driver::serving::RetentionScan;
use peryx_policy::{
    RetentionCandidate, RetentionClass, RetentionConfig, RetentionDecision, RetentionFrontier, RetentionOutcome,
    RetentionPolicy, RetentionSelector, RetentionVisibility,
};
use peryx_storage::meta::{MetaError, MetaStore};
use rstest::rstest;

use super::{RETENTION_PROJECT_BUDGET_BYTES, RETENTION_SCAN_PAGE, evaluate_retention};
use crate::store::PypiStore as _;
use crate::upload::{TrashInfo, Uploaded};
use crate::version::version_key;
use crate::{CoreMetadata, File, Provenance, Yanked};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn version_digest(version: &str) -> String {
    peryx_core::Digest::of(version.as_bytes()).as_str().to_owned()
}

fn seed(meta: &MetaStore, index: &str, project: &str, version: &str, yanked: Yanked, trashed: Option<TrashInfo>) {
    seed_content(meta, index, project, version, &version_digest(version), yanked, trashed);
}

fn seed_content(
    meta: &MetaStore,
    index: &str,
    project: &str,
    version: &str,
    digest: &str,
    yanked: Yanked,
    trashed: Option<TrashInfo>,
) {
    let filename = format!("{project}-{version}.whl");
    let uploaded = Uploaded {
        version: version.to_owned(),
        file: File {
            filename: filename.clone(),
            url: format!("https://files/{filename}"),
            hashes: BTreeMap::from([("sha256".to_owned(), digest.to_owned())]),
            requires_python: None,
            size: Some(1024),
            upload_time: Some("2020-01-01T00:00:00Z".to_owned()),
            yanked,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
        trashed,
    };
    meta.put_upload(index, project, &filename, &serde_json::to_vec(&uploaded).unwrap())
        .unwrap();
}

fn evaluate(
    meta: &MetaStore,
    index: &str,
    policy: &RetentionPolicy,
) -> (
    Result<(), String>,
    Option<peryx_policy::RetentionSummary>,
    Vec<RetentionDecision>,
) {
    let mut decisions = Vec::new();
    let mut summary = None;
    let result = evaluate_retention(
        &scan(meta, index, policy, &ScanCancellation::new()),
        RETENTION_PROJECT_BUDGET_BYTES,
        |current| {
            summary = Some(current);
            Ok(())
        },
        |decision| {
            decisions.push(decision);
            Ok(())
        },
    );
    (result, summary, decisions)
}

fn scan<'a>(
    meta: &'a MetaStore,
    index: &'a str,
    policy: &'a RetentionPolicy,
    cancellation: &'a ScanCancellation,
) -> RetentionScan<'a> {
    RetentionScan {
        meta,
        index,
        policy,
        now: None,
        cancellation,
    }
}

fn plan(meta: &MetaStore, index: &str, policy: &RetentionPolicy) -> (Vec<RetentionDecision>, RetentionFrontier) {
    let (result, summary, decisions) = evaluate(meta, index, policy);
    result.unwrap();
    let summary = summary.unwrap();
    assert_eq!(summary.policy_version, policy.version());
    (decisions, summary.frontier)
}

fn expire_all_but_latest(count: u64) -> RetentionPolicy {
    RetentionPolicy::compile(
        &RetentionConfig {
            keep: vec![RetentionSelector::KeepLatestGroups { count }],
            expire: vec![RetentionSelector::ResourcePrefix { prefix: String::new() }],
        },
        crate::normalize_name,
    )
}

fn reject_decision(_: RetentionDecision) -> Result<(), String> {
    Err("client hung up".to_owned())
}

fn candidate_footprint(project: &str, version: &str) -> usize {
    size_of::<RetentionCandidate>()
        + project.len()
        + format!("{project}-{version}.whl").len()
        + version_digest(version).len()
        + version.len()
}

/// Versions in the project whose alternatives outweigh its budget, half of them kept.
const WIDE_PROJECT_VERSIONS: usize = 120;

/// What a plan holds live beyond its candidates: a verdict per candidate, plus the surviving-version
/// index counted twice, once as the index and once as the decision expanded from it.
fn plan_footprint(candidates: usize, retained: &[&str]) -> usize {
    candidates * size_of::<(RetentionOutcome, Option<&'static str>, u64)>() + 2 * owned_bytes(retained)
}

fn owned_bytes(groups: &[impl AsRef<str>]) -> usize {
    groups
        .iter()
        .map(|group| size_of::<String>() + group.as_ref().len())
        .sum()
}

#[rstest]
#[case::source(RetentionSelector::Source { name: "origin".to_owned() }, true)]
#[case::cached(RetentionSelector::Cached, false)]
#[case::orphan(RetentionSelector::Orphan, false)]
fn test_evaluate_retention_rejects_unsupported_selectors(#[case] selector: RetentionSelector, #[case] keep: bool) {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);
    let name = selector.name();
    let (keep, expire) = if keep {
        (vec![selector], Vec::new())
    } else {
        (Vec::new(), vec![selector])
    };
    let policy = RetentionPolicy::compile(&RetentionConfig { keep, expire }, crate::normalize_name);

    let (result, summary, decisions) = evaluate(&meta, "pypi", &policy);

    assert_eq!(
        result.unwrap_err(),
        format!("pypi retention does not support selector {name:?}")
    );
    assert!(summary.is_none());
    assert!(decisions.is_empty());
}

#[test]
fn test_evaluate_retention_orders_versions_by_pep440_and_keeps_the_newest() {
    let (_dir, meta) = store();
    for version in ["2.0", "1.0", "1.0rc1", "2.0+local", "not-a-version", "also-bad"] {
        seed(&meta, "pypi", "demo", version, Yanked::No, None);
    }

    let (decisions, _) = plan(&meta, "pypi", &expire_all_but_latest(2));

    let ordered: Vec<(&str, RetentionOutcome)> = decisions
        .iter()
        .map(|decision| (decision.group.as_deref().unwrap(), decision.outcome))
        .collect();
    assert_eq!(
        ordered,
        vec![
            ("2.0+local", RetentionOutcome::Retain),
            ("2.0", RetentionOutcome::Retain),
            ("1.0", RetentionOutcome::Remove),
            ("1.0rc1", RetentionOutcome::Remove),
            ("also-bad", RetentionOutcome::Remove),
            ("not-a-version", RetentionOutcome::Remove),
        ]
    );
}

#[rstest]
#[case::underscore_and_case("Acme_Tools", "acme-tools-extra")]
#[case::dot("acme.tools", "acme-tools-extra")]
#[case::canonical("acme-tools", "acme-tools-extra")]
#[case::partial("scratch-", "scratch-package")]
#[case::empty("", "any-project")]
fn test_evaluate_retention_normalizes_resource_prefixes(#[case] prefix: &str, #[case] project: &str) {
    let (_dir, meta) = store();
    seed(&meta, "pypi", project, "1.0", Yanked::No, None);
    let policy = RetentionPolicy::compile(
        &RetentionConfig {
            keep: Vec::new(),
            expire: vec![RetentionSelector::ResourcePrefix {
                prefix: prefix.to_owned(),
            }],
        },
        crate::normalize_name,
    );

    assert_eq!(plan(&meta, "pypi", &policy).0[0].outcome, RetentionOutcome::Remove);
}

#[test]
fn test_equivalent_resource_prefixes_share_a_policy_version() {
    let versions = ["Acme_Tools", "acme.tools", "acme-tools"].map(|prefix| {
        RetentionPolicy::compile(
            &RetentionConfig {
                keep: Vec::new(),
                expire: vec![RetentionSelector::ResourcePrefix {
                    prefix: prefix.to_owned(),
                }],
            },
            crate::normalize_name,
        )
        .version()
    });

    assert_eq!(versions, [versions[0]; 3]);
}

#[test]
fn test_evaluate_retention_lists_surviving_versions_as_alternatives() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "demo", "2.0", Yanked::No, None);
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);

    let (decisions, _) = plan(&meta, "pypi", &expire_all_but_latest(1));

    let removed = decisions
        .iter()
        .find(|decision| decision.outcome == RetentionOutcome::Remove)
        .unwrap();
    assert_eq!(removed.group.as_deref(), Some("1.0"));
    assert_eq!(removed.retained_groups, vec!["2.0".to_owned()]);
}

#[rstest]
#[case::two_releases_of_one_wheel("shared", "shared", 1024)]
#[case::two_distinct_wheels("1.0", "1.1", 2048)]
fn test_evaluate_retention_reclaims_each_removed_digest_once(
    #[case] older: &str,
    #[case] newer: &str,
    #[case] reclaimable: u64,
) {
    let (_dir, meta) = store();
    seed_content(&meta, "pypi", "demo", "1.0", &version_digest(older), Yanked::No, None);
    seed_content(&meta, "pypi", "demo", "1.1", &version_digest(newer), Yanked::No, None);
    seed(&meta, "pypi", "demo", "2.0", Yanked::No, None);

    let (decisions, _) = plan(&meta, "pypi", &expire_all_but_latest(1));

    let removed: Vec<&str> = decisions
        .iter()
        .filter(|decision| decision.outcome == RetentionOutcome::Remove)
        .map(|decision| decision.group.as_deref().unwrap_or_default())
        .collect();
    assert_eq!(removed, ["1.1", "1.0"]);
    assert_eq!(reclaimed_bytes(&decisions), reclaimable);
}

#[test]
fn test_evaluate_retention_leaves_a_digest_a_kept_release_shares_uncharged() {
    let (_dir, meta) = store();
    let shared = version_digest("shared");
    seed_content(&meta, "pypi", "demo", "1.0", &shared, Yanked::No, None);
    seed_content(&meta, "pypi", "demo", "2.0", &shared, Yanked::No, None);

    let (decisions, _) = plan(&meta, "pypi", &expire_all_but_latest(1));

    assert_eq!(reclaimed_bytes(&decisions), 0);
    assert_eq!(decisions[0].bytes, 1024);
}

/// What a reader summing a plan's removals is told the plan frees.
fn reclaimed_bytes(decisions: &[RetentionDecision]) -> u64 {
    decisions
        .iter()
        .filter(|decision| decision.outcome == RetentionOutcome::Remove)
        .map(|decision| decision.bytes)
        .sum()
}

#[test]
fn test_evaluate_retention_marks_a_trashed_record_and_records_its_class() {
    let (_dir, meta) = store();
    seed(
        &meta,
        "pypi",
        "demo",
        "1.0",
        Yanked::No,
        Some(TrashInfo {
            deleted_at_unix: 0,
            actor: None,
            reason: None,
        }),
    );

    let policy = RetentionPolicy::compile(
        &RetentionConfig {
            keep: Vec::new(),
            expire: vec![RetentionSelector::Trash],
        },
        crate::normalize_name,
    );
    let (decisions, _) = plan(&meta, "pypi", &policy);

    assert_eq!(decisions[0].outcome, RetentionOutcome::Remove);
    assert_eq!(decisions[0].rule, Some("trash"));
    assert_eq!(decisions[0].class, RetentionClass::Trash);
    assert_eq!(decisions[0].visibility, RetentionVisibility::Hidden);
    assert_eq!(decisions[0].bytes, 1024);
}

#[test]
fn test_evaluate_retention_records_yanked_visibility() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "demo", "1.0", Yanked::Reason("bad".to_owned()), None);

    let (decisions, _) = plan(
        &meta,
        "pypi",
        &RetentionPolicy::compile(&RetentionConfig::default(), crate::normalize_name),
    );

    assert_eq!(decisions[0].visibility, RetentionVisibility::Withdrawn);
    assert_eq!(decisions[0].class, RetentionClass::Hosted);
}

#[test]
fn test_evaluate_retention_streams_each_project_independently() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "alpha", "2.0", Yanked::No, None);
    seed(&meta, "pypi", "alpha", "1.0", Yanked::No, None);
    seed(&meta, "pypi", "beta", "1.0", Yanked::No, None);

    let (decisions, _) = plan(&meta, "pypi", &expire_all_but_latest(1));

    let removed: Vec<&str> = decisions
        .iter()
        .filter(|decision| decision.outcome == RetentionOutcome::Remove)
        .map(|decision| decision.resource.as_str())
        .collect();
    assert_eq!(removed, vec!["alpha"]);
}

#[test]
fn test_evaluate_retention_skips_records_from_other_indexes() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);
    seed(&meta, "other", "demo", "9.0", Yanked::No, None);

    let (decisions, _) = plan(&meta, "pypi", &expire_all_but_latest(1));

    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].group.as_deref(), Some("1.0"));
}

#[test]
fn test_evaluate_retention_skips_a_malformed_upload_key() {
    let (_dir, meta) = store();
    meta.put_driver_value("pypi\u{0}u\u{0}pypi/malformed", b"not an upload")
        .unwrap();

    let (decisions, _) = plan(&meta, "pypi", &expire_all_but_latest(1));

    assert!(decisions.is_empty());
}

#[test]
fn test_evaluate_retention_rejects_a_corrupt_upload_record() {
    let (_dir, meta) = store();

    seed(&meta, "pypi", "aaa", "1.0", Yanked::No, None);
    meta.put_upload("pypi", "demo", "demo-1.0.whl", b"not json").unwrap();

    let mut seen = 0_u32;
    let result = evaluate_retention(
        &scan(&meta, "pypi", &expire_all_but_latest(1), &ScanCancellation::new()),
        RETENTION_PROJECT_BUDGET_BYTES,
        |_| Ok(()),
        |_| {
            seen += 1;
            Ok(())
        },
    );

    assert_eq!(seen, 1);
    assert!(result.unwrap_err().contains("corrupt upload record"));
}

#[test]
fn test_evaluate_retention_stops_the_scan_when_emit_returns_an_error() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "demo", "2.0", Yanked::No, None);
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);

    let result = evaluate_retention(
        &scan(&meta, "pypi", &expire_all_but_latest(1), &ScanCancellation::new()),
        RETENTION_PROJECT_BUDGET_BYTES,
        |_| Ok(()),
        reject_decision,
    );

    assert!(result.unwrap_err().contains("client hung up"));
}

#[test]
fn test_evaluate_retention_stops_before_scanning_the_next_project() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "aaa", "1.0", Yanked::No, None);
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);

    let result = evaluate_retention(
        &scan(&meta, "pypi", &expire_all_but_latest(1), &ScanCancellation::new()),
        RETENTION_PROJECT_BUDGET_BYTES,
        |_| Ok(()),
        reject_decision,
    );

    assert!(result.unwrap_err().contains("client hung up"));
}

#[test]
fn test_evaluate_retention_stops_iteration_at_a_scan_page_boundary() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "aaa", "1.0", Yanked::No, None);
    for version in 0..RETENTION_SCAN_PAGE {
        seed(&meta, "pypi", "bbb", &version.to_string(), Yanked::No, None);
    }
    let cancellation = ScanCancellation::new();
    let mut decisions = Vec::new();

    let result = evaluate_retention(
        &scan(&meta, "pypi", &expire_all_but_latest(0), &cancellation),
        RETENTION_PROJECT_BUDGET_BYTES,
        |_| Ok(()),
        |decision| {
            cancellation.cancel();
            decisions.push(decision.resource);
            Ok(())
        },
    );

    assert_eq!(result, Err("retention scan cancelled".to_owned()));
    assert_eq!(decisions, ["aaa".to_owned()]);
}

#[test]
fn test_evaluate_retention_stops_after_the_final_scan_page() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "aaa", "1.0", Yanked::No, None);
    seed(&meta, "pypi", "bbb", "1.0", Yanked::No, None);
    let cancellation = ScanCancellation::new();
    let mut decisions = Vec::new();

    let result = evaluate_retention(
        &scan(&meta, "pypi", &expire_all_but_latest(0), &cancellation),
        RETENTION_PROJECT_BUDGET_BYTES,
        |_| Ok(()),
        |decision| {
            cancellation.cancel();
            decisions.push(decision.resource);
            Ok(())
        },
    );

    assert_eq!(result, Err("retention scan cancelled".to_owned()));
    assert_eq!(decisions, ["aaa".to_owned()]);
}

#[test]
fn test_evaluate_retention_rejects_a_project_over_the_memory_budget() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "demo", "2.0", Yanked::No, None);
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);

    let result = evaluate_retention(
        &scan(&meta, "pypi", &expire_all_but_latest(1), &ScanCancellation::new()),
        1,
        |_| Ok(()),
        reject_decision,
    );

    let message = result.unwrap_err();
    assert!(message.contains("project demo"), "{message}");
    assert!(message.contains("per-project memory budget"), "{message}");
}

#[test]
fn test_retention_project_budget_defaults_to_256_mib() {
    assert_eq!(RETENTION_PROJECT_BUDGET_BYTES, 256 * 1024 * 1024);
}

#[test]
fn test_evaluate_retention_accepts_a_project_at_the_memory_budget() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);

    let mut decisions = 0_u32;
    evaluate_retention(
        &scan(&meta, "pypi", &expire_all_but_latest(1), &ScanCancellation::new()),
        candidate_footprint("demo", "1.0") + plan_footprint(1, &["1.0"]),
        |_| Ok(()),
        |_| {
            decisions += 1;
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(decisions, 1);
}

#[test]
fn test_evaluate_retention_rejects_a_project_one_byte_over_the_memory_budget() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);

    let message = evaluate_retention(
        &scan(&meta, "pypi", &expire_all_but_latest(1), &ScanCancellation::new()),
        candidate_footprint("demo", "1.0") + plan_footprint(1, &["1.0"]) - 1,
        |_| Ok(()),
        reject_decision,
    )
    .unwrap_err();

    assert!(message.contains("project demo"), "{message}");
    assert!(message.contains("per-project memory budget"), "{message}");
}

#[test]
fn test_evaluate_retention_rejects_a_plan_whose_expansion_crosses_the_budget() {
    let (_dir, meta) = store();
    let versions = ["1.0", "2.0", "3.0", "4.0"];
    for version in versions {
        seed(&meta, "pypi", "demo", version, Yanked::No, None);
    }
    let candidates: usize = versions
        .iter()
        .map(|version| candidate_footprint("demo", version))
        .sum();

    let message = evaluate_retention(
        &scan(&meta, "pypi", &expire_all_but_latest(1), &ScanCancellation::new()),
        candidates + plan_footprint(versions.len(), &["4.0"]) - 1,
        |_| Ok(()),
        reject_decision,
    )
    .unwrap_err();

    assert!(message.contains("project demo"), "{message}");
    assert!(message.contains("per-project memory budget"), "{message}");
}

#[test]
fn test_evaluate_retention_streams_a_plan_whose_output_exceeds_the_budget() {
    let (_dir, meta) = store();
    let versions: Vec<String> = (1..=WIDE_PROJECT_VERSIONS)
        .map(|release| format!("{release}.0"))
        .collect();
    for version in &versions {
        seed(&meta, "pypi", "demo", version, Yanked::No, None);
    }
    let surviving: Vec<&str> = versions[WIDE_PROJECT_VERSIONS / 2..]
        .iter()
        .map(String::as_str)
        .collect();
    let budget = versions
        .iter()
        .map(|version| candidate_footprint("demo", version))
        .sum::<usize>()
        + plan_footprint(versions.len(), &surviving);
    let mut streamed = 0_usize;

    evaluate_retention(
        &scan(
            &meta,
            "pypi",
            &expire_all_but_latest(WIDE_PROJECT_VERSIONS as u64 / 2),
            &ScanCancellation::new(),
        ),
        budget,
        |_| Ok(()),
        |decision| {
            streamed += owned_bytes(&decision.retained_groups);
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(streamed, surviving.len() * owned_bytes(&surviving));
    assert!(streamed > budget, "the plan emitted {streamed} bytes of alternatives");
}

#[test]
fn test_evaluate_retention_plans_nothing_for_an_empty_index() {
    let (_dir, meta) = store();

    let (decisions, frontier) = plan(&meta, "pypi", &expire_all_but_latest(1));

    assert!(decisions.is_empty());
    assert_eq!(frontier, RetentionFrontier::default());
}

#[test]
fn test_evaluate_retention_reports_the_metadata_frontier() {
    let (_dir, meta) = store();
    meta.commit_driver_txn(|txn| {
        txn.touch_policy_inputs("pypi");
        Ok::<_, MetaError>(((), vec![b"journal entry".to_vec()]))
    })
    .unwrap();
    meta.advance_policy_generation("pypi").unwrap();
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);

    let (_, frontier) = plan(&meta, "pypi", &expire_all_but_latest(1));

    assert_eq!(frontier.repository, 1);
    assert_eq!(frontier.policy, 1);
}

#[test]
fn test_evaluate_retention_keeps_the_opened_frontier_during_a_concurrent_commit() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);
    let mut summary = None;
    let mut decisions = Vec::new();

    evaluate_retention(
        &scan(&meta, "pypi", &expire_all_but_latest(1), &ScanCancellation::new()),
        RETENTION_PROJECT_BUDGET_BYTES,
        |current| {
            summary = Some(current);
            meta.advance_policy_generation("pypi").unwrap();
            Ok(())
        },
        |decision| {
            decisions.push(decision);
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(summary.unwrap().frontier.policy, 0);
    assert_eq!(meta.policy_input_generation("pypi").unwrap().policy, 1);
    assert_eq!(decisions.len(), 1);
}

#[test]
fn test_evaluate_retention_is_byte_identical_across_runs() {
    let (_dir, meta) = store();
    seed(&meta, "pypi", "demo", "2.0", Yanked::No, None);
    seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);
    let policy = expire_all_but_latest(1);
    let render = || serde_json::to_string(&plan(&meta, "pypi", &policy).0).unwrap();

    assert_eq!(render(), render());
}

#[test]
fn test_version_key_desc_ranks_releases_before_legacy_spellings() {
    let release = version_key("2.0");
    let older = version_key("1.0");
    let legacy = version_key("not-a-version");
    let other_legacy = version_key("also-bad");

    assert_eq!(super::version_key_desc(&release, &older), Ordering::Less);
    assert_eq!(super::version_key_desc(&release, &legacy), Ordering::Less);
    assert_eq!(super::version_key_desc(&legacy, &release), Ordering::Greater);
    assert_eq!(super::version_key_desc(&other_legacy, &legacy), Ordering::Less);
}
