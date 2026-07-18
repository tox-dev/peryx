//! The `PyPI` half of retention-plan evaluation: adapt one index's hosted upload records into the
//! neutral [`RetentionCandidate`]s the [`peryx_policy`] engine plans over.
//!
//! Uploads scan in key order (`{index}/{normalized}/{filename}`), so a project's files arrive
//! contiguously. This groups them, ranks their versions newest-first under
//! [PEP 440](https://peps.python.org/pep-0440/), and streams the resulting decisions one project at a
//! time, so a large index never materializes as one in-memory plan. The scan reads only indexed
//! metadata, so an interrupted evaluation writes nothing.

use std::cmp::Ordering;
use std::collections::HashMap;

use peryx_policy::{
    RetentionCandidate, RetentionClass, RetentionDecision, RetentionFrontier, RetentionPolicy, RetentionSummary,
    RetentionVisibility,
};
use peryx_storage::meta::MetaStore;

use crate::policy::parse_upload_time;
use crate::store::PypiStore as _;
use crate::upload::Uploaded;
use crate::version::{VersionKey, version_key};
use crate::{Yanked, error_message};

/// Evaluate one index's hosted uploads against `policy`.
///
/// Each artifact's decision passes to `emit` in deterministic order (newest version first). Returns the
/// plan's identity: the policy version and the metadata frontier the scan read.
///
/// # Errors
/// Returns a message when the store cannot be read or an upload record does not decode.
pub fn evaluate_retention<F>(
    meta: &MetaStore,
    index: &str,
    policy: &RetentionPolicy,
    now: Option<i64>,
    mut emit: F,
) -> Result<RetentionSummary, String>
where
    F: FnMut(RetentionDecision),
{
    let frontier = read_frontier(meta, index)?;
    let prefix = format!("{index}/");
    let mut current: Option<String> = None;
    let mut group: Vec<Uploaded> = Vec::new();
    meta.scan_upload_records(|key, bytes| {
        let Some((project, _filename)) = key.strip_prefix(&prefix).and_then(|rest| rest.split_once('/')) else {
            return Ok(());
        };
        if current.as_deref() != Some(project) {
            if let Some(name) = current.take() {
                plan_group(&name, &group, policy, now, &mut emit);
            }
            current = Some(project.to_owned());
            group.clear();
        }
        let uploaded: Uploaded =
            serde_json::from_slice(bytes).map_err(|err| format!("corrupt upload record {key}: {err}"))?;
        group.push(uploaded);
        Ok::<(), String>(())
    })
    .map_err(error_message)?;
    if let Some(name) = current {
        plan_group(&name, &group, policy, now, &mut emit);
    }
    Ok(RetentionSummary {
        policy_version: policy.version(),
        frontier,
    })
}

fn plan_group<F>(project: &str, group: &[Uploaded], policy: &RetentionPolicy, now: Option<i64>, emit: &mut F)
where
    F: FnMut(RetentionDecision),
{
    for decision in policy.plan_project(now, candidates(project, group)) {
        emit(decision);
    }
}

fn candidates(project: &str, group: &[Uploaded]) -> Vec<RetentionCandidate> {
    let ranks = version_ranks(group);
    group
        .iter()
        .map(|uploaded| {
            let file = &uploaded.file;
            RetentionCandidate {
                project: project.to_owned(),
                version: Some(uploaded.version.clone()),
                artifact: file.filename.clone(),
                digest: file.hashes.get("sha256").cloned().unwrap_or_default(),
                class: if uploaded.trashed.is_some() {
                    RetentionClass::Trash
                } else {
                    RetentionClass::Hosted
                },
                visibility: match file.yanked {
                    Yanked::No => RetentionVisibility::Active,
                    Yanked::Yes | Yanked::Reason(_) => RetentionVisibility::Yanked,
                },
                source: None,
                bytes: file.size.unwrap_or(0),
                upload_time_unix: file.upload_time.as_deref().and_then(parse_upload_time),
                rank: ranks[&version_key(&uploaded.version)],
                orphan: false,
            }
        })
        .collect()
}

/// Rank each distinct release newest-first. Two spellings of one release (`1.0`, `1.0.0`) collapse to
/// one rank; an unparseable legacy version ranks after every valid one, by string order.
fn version_ranks(group: &[Uploaded]) -> HashMap<VersionKey, u64> {
    let mut distinct: Vec<VersionKey> = group.iter().map(|uploaded| version_key(&uploaded.version)).collect();
    distinct.sort_by(version_key_desc);
    distinct.dedup();
    distinct
        .into_iter()
        .enumerate()
        .map(|(rank, key)| (key, rank as u64))
        .collect()
}

fn version_key_desc(left: &VersionKey, right: &VersionKey) -> Ordering {
    match (left, right) {
        (VersionKey::Parsed(left), VersionKey::Parsed(right)) => right.cmp(left),
        (VersionKey::Raw(left), VersionKey::Raw(right)) => left.cmp(right),
        // A parsed release outranks any legacy spelling; both mixed orders resolve here, so neither
        // depends on which direction the sort happens to compare them.
        _ => parse_class(left).cmp(&parse_class(right)),
    }
}

const fn parse_class(key: &VersionKey) -> u8 {
    match key {
        VersionKey::Parsed(_) => 0,
        VersionKey::Raw(_) => 1,
    }
}

fn read_frontier(meta: &MetaStore, index: &str) -> Result<RetentionFrontier, String> {
    let generation = meta.policy_input_generation(index).map_err(error_message)?;
    Ok(RetentionFrontier {
        repository: meta.current_serial().map_err(error_message)?,
        catalog: generation.catalog,
        policy: generation.policy,
    })
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;
    use std::collections::BTreeMap;

    use peryx_policy::{
        RetentionClass, RetentionConfig, RetentionDecision, RetentionFrontier, RetentionOutcome, RetentionPolicy,
        RetentionSelector, RetentionVisibility,
    };
    use peryx_storage::meta::MetaStore;

    use super::evaluate_retention;
    use crate::store::PypiStore as _;
    use crate::upload::{TrashInfo, Uploaded};
    use crate::version::version_key;
    use crate::{CoreMetadata, File, Provenance, Yanked};

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    fn seed(meta: &MetaStore, index: &str, project: &str, version: &str, yanked: Yanked, trashed: Option<TrashInfo>) {
        let filename = format!("{project}-{version}.whl");
        let uploaded = Uploaded {
            version: version.to_owned(),
            file: File {
                filename: filename.clone(),
                url: format!("https://files/{filename}"),
                hashes: BTreeMap::from([("sha256".to_owned(), format!("sha-{version}"))]),
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

    fn plan(meta: &MetaStore, index: &str, policy: &RetentionPolicy) -> (Vec<RetentionDecision>, RetentionFrontier) {
        let mut decisions = Vec::new();
        let summary = evaluate_retention(meta, index, policy, None, |decision| decisions.push(decision)).unwrap();
        assert_eq!(summary.policy_version, policy.version());
        (decisions, summary.frontier)
    }

    fn expire_all_but_latest(count: u64) -> RetentionPolicy {
        RetentionPolicy::compile(&RetentionConfig {
            keep: vec![RetentionSelector::KeepLatest { count }],
            expire: vec![RetentionSelector::ProjectPrefix { prefix: String::new() }],
        })
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
            .map(|decision| (decision.version.as_deref().unwrap(), decision.outcome))
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
        assert_eq!(removed.version.as_deref(), Some("1.0"));
        assert_eq!(removed.retained_alternatives, vec!["2.0".to_owned()]);
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

        let policy = RetentionPolicy::compile(&RetentionConfig {
            keep: Vec::new(),
            expire: vec![RetentionSelector::Trash],
        });
        let (decisions, _) = plan(&meta, "pypi", &policy);

        assert_eq!(decisions[0].outcome, RetentionOutcome::Remove);
        assert_eq!(decisions[0].rule, Some("trash"));
        assert_eq!(decisions[0].class, RetentionClass::Trash);
        assert_eq!(decisions[0].bytes, 1024);
    }

    #[test]
    fn test_evaluate_retention_records_yanked_visibility() {
        let (_dir, meta) = store();
        seed(&meta, "pypi", "demo", "1.0", Yanked::Reason("bad".to_owned()), None);

        let (decisions, _) = plan(&meta, "pypi", &RetentionPolicy::compile(&RetentionConfig::default()));

        assert_eq!(decisions[0].visibility, RetentionVisibility::Yanked);
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
            .map(|decision| decision.project.as_str())
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
        assert_eq!(decisions[0].version.as_deref(), Some("1.0"));
    }

    #[test]
    fn test_evaluate_retention_rejects_a_corrupt_upload_record() {
        let (_dir, meta) = store();
        meta.put_upload("pypi", "demo", "demo-1.0.whl", b"not json").unwrap();

        let result = evaluate_retention(&meta, "pypi", &expire_all_but_latest(1), None, |_| ());

        assert!(result.unwrap_err().contains("corrupt upload record"));
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
        meta.advance_policy_generation("pypi").unwrap();
        seed(&meta, "pypi", "demo", "1.0", Yanked::No, None);

        let (_, frontier) = plan(&meta, "pypi", &expire_all_but_latest(1));

        assert_eq!(frontier.policy, 1);
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
}
