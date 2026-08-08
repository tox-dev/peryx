//! Explaining virtual-repository shadowing from stored records.
//!
//! [`resolve`](super::resolve) merges members in shadow order and drops the candidates a member
//! shadows; this replays the same order and records the losers instead. It reads only stored records:
//! a hosted member's uploads and a cached member's fetched page. It never probes an
//! upstream per row, and it applies the repository's fallback mode exactly as resolution does.

use std::collections::BTreeSet;

use peryx_core::{ShadowCandidate, ShadowReason, ShadowSource};
use peryx_driver::state::ServingState;
use peryx_index::{Index, IndexKind};
use peryx_policy::FallbackMode;

use super::CacheError;
use super::resolve::{local_detail, raw_to_detail};
use crate::store::PypiStore as _;
use crate::{ProjectDetail, name};

/// The selected and shadowed candidates for `project` in the virtual index at `position`.
///
/// A non-virtual index shadows nothing, so it returns an empty list.
///
/// # Errors
/// Returns a message when a member's stored records cannot be read.
pub fn shadowed_candidates(
    state: &ServingState,
    position: usize,
    project: &str,
) -> Result<Vec<ShadowCandidate>, String> {
    candidates(state, position, project).map_err(|error| error.user_message())
}

fn candidates(state: &ServingState, position: usize, project: &str) -> Result<Vec<ShadowCandidate>, CacheError> {
    let index = state.index_at(position);
    let IndexKind::Virtual { layers, .. } = &index.kind else {
        return Ok(Vec::new());
    };
    let project = name::normalize_name(project);
    let ordered = peryx_index::shadow_order(&state.indexes, layers);
    let mut members = Vec::new();
    for member in ordered.into_iter().map(|pos| state.index_at(pos)) {
        if let Some(detail) = stored_detail(state, member, &project)? {
            members.push((member, detail));
        }
    }
    let hosted_found = members
        .iter()
        .any(|(member, detail)| !is_cached(member) && !detail.files.is_empty());
    let excludes_cached = excludes_cached(index.policy.fallback_mode(), hosted_found);
    let mut selected = BTreeSet::new();
    let mut candidates = Vec::new();
    for (member, detail) in members {
        let cached = is_cached(member);
        for file in detail.files {
            let (is_selected, reason) = if cached && excludes_cached {
                (false, Some(ShadowReason::Fallback))
            } else if selected.insert(file.filename.clone()) {
                (true, None)
            } else {
                (false, Some(ShadowReason::Precedence))
            };
            candidates.push(ShadowCandidate {
                repository: index.name.clone(),
                project: project.clone(),
                member: member.name.clone(),
                source: if cached {
                    ShadowSource::Cached
                } else {
                    ShadowSource::Hosted
                },
                digest: file.hashes.get("sha256").map(|hex| format!("sha256:{hex}")),
                filename: file.filename,
                selected: is_selected,
                reason,
            });
        }
    }
    Ok(candidates)
}

/// A member's stored detail for `project`: a hosted member's uploads, or a cached member's already
/// fetched page. Never fetches; a cache with no stored page contributes nothing.
fn stored_detail(state: &ServingState, member: &Index, project: &str) -> Result<Option<ProjectDetail>, CacheError> {
    if is_cached(member) {
        let key = format!("{}/{project}", member.name);
        return match state.meta.get_index(&key)? {
            Some(record) => Ok(Some(raw_to_detail(state, &member.route, &record)?)),
            None => Ok(None),
        };
    }
    local_detail(state, &member.name, project)
}

const fn is_cached(index: &Index) -> bool {
    matches!(index.kind, IndexKind::Cached { .. })
}

/// Whether the repository's fallback mode drops cached members. Mirrors
/// [`resolve`](super::resolve)'s `consult_cached`/`PrivateFirst` handling: plain fallback keeps them,
/// private-first drops them only when a hosted member is present, and no-fallback never consults them.
const fn excludes_cached(mode: FallbackMode, hosted_found: bool) -> bool {
    match mode {
        FallbackMode::Fallback => false,
        FallbackMode::PrivateFirst => hosted_found,
        FallbackMode::NoFallback => true,
    }
}
