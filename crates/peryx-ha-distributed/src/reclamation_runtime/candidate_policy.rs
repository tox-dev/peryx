use peryx_ha::{
    ObservedFrontier, ReadyOutcome, ReclamationDecisionError, ReclamationStore, SelectOutcome, TombstoneWrite,
    decide_reclamation_readiness, decide_reclamation_selection,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::meta::{MetaError, MetaStore};

#[derive(Debug, thiserror::Error)]
pub enum ReclamationError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Decision(#[from] ReclamationDecisionError),
}

/// Returns `None` when the reference revision moved past `revision`, which retires the `referenced`
/// verdict without writing it; the caller re-proves the inventory before deciding this digest again.
///
/// # Errors
/// Returns a persistence or reclamation decision error.
pub fn select_reclamation_candidate(
    meta: &MetaStore,
    digest: &ArtifactDigest,
    referenced: bool,
    revision: u64,
    required_frontier: u64,
    fence: u64,
    now: i64,
) -> Result<Option<SelectOutcome>, ReclamationError> {
    loop {
        let snapshot = meta.reclamation_snapshot(digest)?;
        let outcome = decide_reclamation_selection(digest, &snapshot, referenced, required_frontier, fence, now)?;
        let Some(replacement) = outcome.replacement() else {
            return Ok(Some(outcome));
        };
        match meta.compare_and_put_reclamation_tombstone(&snapshot, replacement, revision)? {
            TombstoneWrite::Written => return Ok(Some(outcome)),
            TombstoneWrite::ReferencesMoved => return Ok(None),
            TombstoneWrite::Conflict => {}
        }
    }
}

/// Returns `None` when the reference revision moved past `revision`, so a digest that gained a
/// reference after the inventory was proved cannot be marked ready from that stale verdict.
///
/// # Errors
/// Returns a persistence or reclamation decision error.
pub fn mark_reclamation_ready(
    meta: &MetaStore,
    digest: &ArtifactDigest,
    referenced: bool,
    revision: u64,
    observed: ObservedFrontier,
    fence: u64,
    now: i64,
) -> Result<Option<ReadyOutcome>, ReclamationError> {
    loop {
        let snapshot = meta.reclamation_snapshot(digest)?;
        let outcome = decide_reclamation_readiness(&snapshot, referenced, observed, fence, now)?;
        if snapshot.tombstone.as_ref() == Some(outcome.replacement()) {
            return Ok(Some(outcome));
        }
        match meta.compare_and_put_reclamation_tombstone(&snapshot, outcome.replacement(), revision)? {
            TombstoneWrite::Written => return Ok(Some(outcome)),
            TombstoneWrite::ReferencesMoved => return Ok(None),
            TombstoneWrite::Conflict => {}
        }
    }
}
