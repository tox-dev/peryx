//! Evaluates `PyPI` retention one project at a time to bound memory.
//!
//! Global version ranking and cross-referenced alternatives need one project's candidates in memory at
//! once, so the scan cannot stream within a project. It bounds that peak three ways: each raw
//! [`Uploaded`] record is projected to a compact [`RetentionCandidate`] and dropped as it is read,
//! never held alongside its decoded form; decisions expand one at a time out of a
//! [`RetentionPlan`](peryx_policy::RetentionPlan), so the surviving versions every removal repeats are
//! stored once rather than per row; and a per-project byte budget over that whole live set aborts a
//! project that would exceed it, so one oversized project rejects its run instead of allocating without
//! limit.
//!
//! The budget bounds live memory, not output. A project whose decisions serialize to far more than the
//! budget still streams, because no more than one of them is expanded at a time.

use std::cmp::Ordering;
use std::collections::HashMap;

use peryx_driver::serving::RetentionScan;
use peryx_policy::{
    RetentionCandidate, RetentionClass, RetentionDecision, RetentionFrontier, RetentionPolicy, RetentionSelector,
    RetentionSummary, RetentionVisibility,
};

use crate::policy::parse_upload_time;
use crate::store::scan_upload_policy_snapshot;
use crate::upload::Uploaded;
use crate::version::{VersionKey, version_key};
use crate::{Yanked, error_message};

/// Default ceiling on the live footprint one project may hold before a retention scan rejects it.
///
/// That footprint is each candidate's struct plus its owned string bytes, the plan's verdicts and
/// surviving-version index, and the one decision expanded at a time.
///
/// It bounds a run's peak memory independent of one project's artifact count; a project past it aborts
/// with a message rather than exhausting the process. 256 MiB leaves room for the largest realistic
/// project while still catching a pathological one.
pub const RETENTION_PROJECT_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// Evaluate one index's hosted uploads against `policy`.
///
/// `start` receives the plan identity after the read snapshot opens and before `emit` receives the first
/// artifact decision. Either callback may stop the read-only scan by returning an error.
///
/// `budget` caps the memory one project may hold at once (see [`RETENTION_PROJECT_BUDGET_BYTES`]): its
/// candidates, its plan's compact state, and the single decision in flight. A project whose live set
/// would exceed it aborts the scan before that decision is expanded, so peak memory stays bounded
/// regardless of any one project's artifact count. What the plan serializes to is not capped.
///
/// # Errors
/// Returns a message when the policy contains an unsupported selector, the store cannot be read, an
/// upload record does not decode, a callback stops the scan, or a project's live set exceeds `budget`.
pub fn evaluate_retention<S, F>(
    scan: &RetentionScan<'_>,
    budget: usize,
    mut start: S,
    mut emit: F,
) -> Result<(), String>
where
    S: FnMut(RetentionSummary) -> Result<(), String>,
    F: FnMut(RetentionDecision) -> Result<(), String>,
{
    validate_retention(scan.policy)?;
    evaluate_retention_with(scan, budget, &mut start, &mut emit)
}

/// # Errors
/// Returns the first selector hosted `PyPI` records cannot evaluate.
pub(crate) fn validate_retention(policy: &RetentionPolicy) -> Result<(), String> {
    policy
        .selectors()
        .find(|selector| {
            matches!(
                selector,
                RetentionSelector::Source { .. } | RetentionSelector::Cached | RetentionSelector::Orphan
            )
        })
        .map_or(Ok(()), |selector| {
            Err(format!(
                "pypi retention does not support selector {:?}",
                selector.name()
            ))
        })
}

fn evaluate_retention_with(
    scan: &RetentionScan<'_>,
    budget: usize,
    start: &mut dyn FnMut(RetentionSummary) -> Result<(), String>,
    emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
) -> Result<(), String> {
    let mut current: Option<String> = None;
    let mut group: Vec<RetentionCandidate> = Vec::new();
    let mut used: usize = 0;
    let mut scanned: usize = 0;
    scan_upload_policy_snapshot(
        scan.meta,
        scan.index,
        |generation| {
            start(RetentionSummary {
                policy_version: scan.policy.version(),
                frontier: RetentionFrontier {
                    repository: generation.repository,
                    catalog: generation.catalog,
                    policy: generation.policy,
                },
            })
        },
        |key, bytes| {
            if scanned.is_multiple_of(RETENTION_SCAN_PAGE) && scan.cancellation.is_cancelled() {
                return Err("retention scan cancelled".to_owned());
            }
            scanned += 1;
            let Some((project, _filename)) = key.split_once('/') else {
                return Ok(());
            };
            if current.as_deref() != Some(project) {
                if let Some(previous) = current.as_deref() {
                    plan_group(&mut group, previous, used, budget, scan.policy, scan.now, emit)?;
                }
                current = Some(project.to_owned());
                used = 0;
            }
            let uploaded: Uploaded =
                serde_json::from_slice(bytes).map_err(|err| format!("corrupt upload record {key}: {err}"))?;
            let candidate = candidate(project, uploaded);
            used = used.saturating_add(footprint(&candidate));
            if used > budget {
                return Err(over_budget(project, budget));
            }
            group.push(candidate);
            Ok::<(), String>(())
        },
    )
    .map_err(error_message)?;
    if scan.cancellation.is_cancelled() {
        return Err("retention scan cancelled".to_owned());
    }
    if let Some(previous) = current.as_deref() {
        plan_group(&mut group, previous, used, budget, scan.policy, scan.now, emit).map_err(error_message)?;
    }
    Ok(())
}

/// How many upload records the scan reads between cancellation checks. Reading an atomic per record
/// would cost more than it saves, and a page this small still bounds how long a cancelled request keeps
/// its blocking worker.
const RETENTION_SCAN_PAGE: usize = 100;

fn plan_group(
    group: &mut Vec<RetentionCandidate>,
    project: &str,
    used: usize,
    budget: usize,
    policy: &RetentionPolicy,
    now: Option<i64>,
    emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
) -> Result<(), String> {
    let mut group = std::mem::take(group);
    assign_ranks(&mut group);
    let plan = policy.plan_resource(now, group);
    if used.saturating_add(plan.live_bytes()) > budget {
        return Err(over_budget(project, budget));
    }
    for decision in plan.decisions() {
        emit(decision)?;
    }
    Ok(())
}

fn over_budget(project: &str, budget: usize) -> String {
    format!("retention plan for project {project} exceeds the {budget}-byte per-project memory budget")
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
        (None, Yanked::Yes | Yanked::Reason(_)) => RetentionVisibility::Withdrawn,
    };
    RetentionCandidate {
        resource: project.to_owned(),
        artifact: file.filename,
        digest: file.hashes.get("sha256").cloned().unwrap_or_default(),
        class,
        visibility,
        source: None,
        bytes: file.size.unwrap_or(0),
        upload_time_unix: file.upload_time.as_deref().and_then(parse_upload_time),
        group: Some(version),
        rank: 0,
        orphan: false,
    }
}

/// The bytes one candidate holds: its struct plus the strings this adapter fills, so the budget tracks
/// string weight rather than record count alone. A pypi candidate carries no `source`, so none counts.
fn footprint(candidate: &RetentionCandidate) -> usize {
    size_of::<RetentionCandidate>()
        + candidate.resource.len()
        + candidate.artifact.len()
        + candidate.digest.len()
        + candidate.group.as_deref().map_or(0, str::len)
}

/// Rank each distinct release newest-first. Two spellings of one release (`1.0`, `1.0.0`) collapse to
/// one rank; an unparseable legacy version ranks after every valid one, by string order.
fn assign_ranks(group: &mut [RetentionCandidate]) {
    let ranks = version_ranks(group);
    for candidate in &mut *group {
        candidate.rank = ranks[&version_key(candidate.group.as_deref().unwrap_or_default())];
    }
}

fn version_ranks(group: &[RetentionCandidate]) -> HashMap<VersionKey, u64> {
    let mut distinct: Vec<VersionKey> = group
        .iter()
        .map(|candidate| version_key(candidate.group.as_deref().unwrap_or_default()))
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
