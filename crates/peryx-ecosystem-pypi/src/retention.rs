//! The `PyPI` half of retention-plan evaluation: adapt one index's hosted upload records into the
//! neutral [`RetentionCandidate`]s the [`peryx_policy`] engine plans over.
//!
//! Uploads scan in key order (`{index}/{normalized}/{filename}`), so a project's files arrive
//! contiguously. This groups them, ranks their versions newest-first under
//! [PEP 440](https://peps.python.org/pep-0440/), and streams the resulting decisions one project at a
//! time, so a large index never materializes as one in-memory plan. The scan reads only indexed
//! metadata, so an interrupted evaluation writes nothing.
//!
//! Global version ranking and cross-referenced alternatives need one project's candidates in memory at
//! once, so the scan cannot stream within a project. It bounds that peak two ways: each raw
//! [`Uploaded`] record is projected to a compact [`RetentionCandidate`] and dropped as it is read,
//! never held alongside its decoded form; and a per-project byte budget over the surviving candidates'
//! footprint aborts a project that would exceed it, so one oversized project rejects its run instead of
//! allocating without limit.

use std::cmp::Ordering;
use std::collections::HashMap;

use peryx_policy::{
    RetentionCandidate, RetentionClass, RetentionDecision, RetentionFrontier, RetentionPolicy, RetentionSummary,
    RetentionVisibility,
};
use peryx_storage::meta::MetaStore;

use crate::policy::parse_upload_time;
use crate::store::scan_upload_policy_snapshot;
use crate::upload::Uploaded;
use crate::version::{VersionKey, version_key};
use crate::{Yanked, error_message};

/// Default ceiling on the candidate footprint one project may accumulate before a retention scan
/// rejects it, counting each candidate's struct plus its owned string bytes.
///
/// It bounds a run's peak memory independent of one project's artifact count; a project past it aborts
/// with a message rather than exhausting the process. 256 MiB leaves room for the largest realistic
/// project while still catching a pathological one.
pub const RETENTION_PROJECT_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// Evaluate one index's hosted uploads against `policy`.
///
/// Each artifact's decision passes to `emit` in deterministic order (newest version first). Returns the
/// plan's identity: the policy version and the metadata frontier the scan read. `emit` returns a
/// message to stop early (a disconnected export client or a filled page), and the scan aborts without
/// reading further; the whole path only reads metadata, so an interrupted plan writes nothing.
///
/// `budget` caps the candidate footprint one project may hold at once (see
/// [`RETENTION_PROJECT_BUDGET_BYTES`]); a project whose surviving candidates exceed it aborts the scan
/// so peak memory stays bounded regardless of any one project's artifact count.
///
/// # Errors
/// Returns a message when the store cannot be read, an upload record does not decode, `emit` stops the
/// scan, or a project's candidates exceed `budget`.
pub fn evaluate_retention<F>(
    meta: &MetaStore,
    index: &str,
    policy: &RetentionPolicy,
    now: Option<i64>,
    budget: usize,
    mut emit: F,
) -> Result<RetentionSummary, String>
where
    F: FnMut(RetentionDecision) -> Result<(), String>,
{
    let mut current: Option<String> = None;
    let mut group: Vec<RetentionCandidate> = Vec::new();
    let mut used: usize = 0;
    let generation = scan_upload_policy_snapshot(meta, index, |key, bytes| {
        let Some((project, _filename)) = key.split_once('/') else {
            return Ok(());
        };
        if current.as_deref() != Some(project) {
            if current.is_some() {
                plan_group(&mut group, policy, now, &mut emit)?;
            }
            current = Some(project.to_owned());
            used = 0;
        }
        let uploaded: Uploaded =
            serde_json::from_slice(bytes).map_err(|err| format!("corrupt upload record {key}: {err}"))?;
        let candidate = candidate(project, uploaded);
        used = used.saturating_add(footprint(&candidate));
        if used > budget {
            return Err(format!(
                "retention plan for project {project} exceeds the {budget}-byte per-project memory budget"
            ));
        }
        group.push(candidate);
        Ok::<(), String>(())
    })
    .map_err(error_message)?;
    if current.is_some() {
        plan_group(&mut group, policy, now, &mut emit).map_err(error_message)?;
    }
    Ok(RetentionSummary {
        policy_version: policy.version(),
        frontier: RetentionFrontier {
            repository: generation.repository,
            catalog: generation.catalog,
            policy: generation.policy,
        },
    })
}

fn plan_group<F>(
    group: &mut Vec<RetentionCandidate>,
    policy: &RetentionPolicy,
    now: Option<i64>,
    emit: &mut F,
) -> Result<(), String>
where
    F: FnMut(RetentionDecision) -> Result<(), String>,
{
    let mut group = std::mem::take(group);
    assign_ranks(&mut group);
    for decision in policy.plan_project(now, group) {
        emit(decision)?;
    }
    Ok(())
}

/// Project one raw upload record to its compact candidate, moving the fields retention keeps out of the
/// decoded record so its heavier remainder (the served URL, the full hash map, the metadata and
/// provenance blobs) drops as this returns. `rank` is filled once the whole project is grouped.
fn candidate(project: &str, uploaded: Uploaded) -> RetentionCandidate {
    let Uploaded { version, file, trashed } = uploaded;
    let class = if trashed.is_some() {
        RetentionClass::Trash
    } else {
        RetentionClass::Hosted
    };
    let visibility = match (&trashed, &file.yanked) {
        (Some(_), _) => RetentionVisibility::Hidden,
        (None, Yanked::No) => RetentionVisibility::Active,
        (None, Yanked::Yes | Yanked::Reason(_)) => RetentionVisibility::Yanked,
    };
    RetentionCandidate {
        project: project.to_owned(),
        artifact: file.filename,
        digest: file.hashes.get("sha256").cloned().unwrap_or_default(),
        class,
        visibility,
        source: None,
        bytes: file.size.unwrap_or(0),
        upload_time_unix: file.upload_time.as_deref().and_then(parse_upload_time),
        version: Some(version),
        rank: 0,
        orphan: false,
    }
}

/// The bytes one candidate holds: its struct plus the strings this adapter fills, so the budget tracks
/// string weight rather than record count alone. A pypi candidate carries no `source`, so none counts.
fn footprint(candidate: &RetentionCandidate) -> usize {
    size_of::<RetentionCandidate>()
        + candidate.project.len()
        + candidate.artifact.len()
        + candidate.digest.len()
        + candidate.version.as_deref().map_or(0, str::len)
}

/// Rank each distinct release newest-first. Two spellings of one release (`1.0`, `1.0.0`) collapse to
/// one rank; an unparseable legacy version ranks after every valid one, by string order.
fn assign_ranks(group: &mut [RetentionCandidate]) {
    let ranks = version_ranks(group);
    for candidate in &mut *group {
        candidate.rank = ranks[&version_key(candidate.version.as_deref().unwrap_or_default())];
    }
}

fn version_ranks(group: &[RetentionCandidate]) -> HashMap<VersionKey, u64> {
    let mut distinct: Vec<VersionKey> = group
        .iter()
        .map(|candidate| version_key(candidate.version.as_deref().unwrap_or_default()))
        .collect();
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

#[cfg(test)]
#[path = "../tests/unit/retention/tests.rs"]
mod tests;
