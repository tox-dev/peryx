//! [`resolve`](super::resolve) merges leaf candidates in shadow order and drops the candidates a
//! leaf shadows; this replays the same order and records the losers instead. Like resolution it
//! ranks the leaves a member reaches rather than the member itself, so a cache nested one level down
//! is recorded as the cached source it is, and it asks the same
//! [`SourceSelection`](crate::source_policy::SourceSelection) why a cached leaf is out.
//!
//! It reads only stored records: a hosted leaf's uploads and a cached leaf's fetched page. It never
//! probes an upstream per row. Only this repository's source policy is replayed, so a leaf that a
//! nested repository already dropped under its own mode reads here as selected or shadowed on
//! precedence rather than excluded.

use std::collections::BTreeSet;

use peryx_driver::state::ServingState;
use peryx_index::{Index, IndexKind};

use super::CacheError;
use super::resolve::{local_detail, raw_to_detail};
use crate::shadow::{ShadowCandidate, ShadowReason, ShadowSource};
use crate::source_policy::SourceSelection;
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
    let ordered = peryx_index::leaf_order(&state.indexes, layers);
    let mut members = Vec::new();
    for member in ordered.into_iter().map(|pos| state.index_at(pos)) {
        if let Some(detail) = stored_detail(state, member, &project)? {
            members.push((member, detail));
        }
    }
    let hosted_found = members
        .iter()
        .any(|(member, detail)| !is_cached(member) && !detail.files.is_empty());
    let cached_exclusion = SourceSelection::new(index, &project).cached_exclusion(hosted_found);
    let mut selected = BTreeSet::new();
    let mut candidates = Vec::new();
    for (member, detail) in members {
        let cached = is_cached(member);
        let excluded = if cached { cached_exclusion } else { None };
        for file in detail.files {
            let (is_selected, reason) = if let Some(reason) = excluded {
                (false, Some(reason))
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
