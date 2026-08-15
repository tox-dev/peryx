use peryx_core::ObservedFrontier;
use peryx_identity::ArtifactDigest;
use serde::{Deserialize, Serialize};

use crate::{BlobPlacementRecord, BlobPlacementState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimGuard {
    pub expires_at_unix: i64,
}

impl ReclaimGuard {
    #[must_use]
    pub const fn is_expired_at(self, now: i64) -> bool {
        self.expires_at_unix <= now
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimGuardArm {
    SerialChanged,
    Armed(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ReclamationState {
    Pending,
    Ready,
    Skipped { reason: SkipReason },
}

impl ReclamationState {
    #[must_use]
    pub const fn status(&self) -> ReclamationStatus {
        match self {
            Self::Pending => ReclamationStatus::Pending,
            Self::Ready => ReclamationStatus::Ready,
            Self::Skipped { .. } => ReclamationStatus::Skipped,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReclamationStatus {
    Pending,
    Ready,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Referenced,
    Serveable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclamationTombstone {
    pub digest: ArtifactDigest,
    pub state: ReclamationState,
    pub required_frontier: u64,
    pub fence: u64,
    pub attempts: u64,
    pub selected_at_unix: i64,
    pub updated_at_unix: i64,
}

impl ReclamationTombstone {
    /// # Errors
    /// Returns `StaleFence` when an older owner attempts the transition.
    pub const fn validate_fence(&self, fence: u64) -> Result<(), ReclamationDecisionError> {
        if fence < self.fence {
            Err(ReclamationDecisionError::StaleFence {
                current: self.fence,
                applied: fence,
            })
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub const fn is_skipped(&self) -> bool {
        matches!(self.state, ReclamationState::Skipped { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclamationSnapshot {
    pub tombstone: Option<ReclamationTombstone>,
    pub placements: Vec<BlobPlacementRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectOutcome {
    Selected(ReclamationTombstone),
    Skipped(ReclamationTombstone),
    Ineligible(SkipReason),
}

impl SelectOutcome {
    #[must_use]
    pub const fn replacement(&self) -> Option<&ReclamationTombstone> {
        match self {
            Self::Selected(record) | Self::Skipped(record) => Some(record),
            Self::Ineligible(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyOutcome {
    Ready(ReclamationTombstone),
    NotReady {
        tombstone: ReclamationTombstone,
        observed: ObservedFrontier,
    },
    Skipped(ReclamationTombstone),
}

impl ReadyOutcome {
    #[must_use]
    pub const fn replacement(&self) -> &ReclamationTombstone {
        match self {
            Self::Ready(record) | Self::Skipped(record) | Self::NotReady { tombstone: record, .. } => record,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReclamationDecisionError {
    #[error("a newer fence {current} supersedes the applied fence {applied}")]
    StaleFence { current: u64, applied: u64 },
    #[error("no reclamation candidate exists for this digest")]
    MissingCandidate,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReclamationProgress {
    pub pending: u64,
    pub ready: u64,
    pub skipped: u64,
}

impl ReclamationProgress {
    #[must_use]
    /// Counts tombstones by terminal status.
    pub fn from_tombstones<'a>(tombstones: impl IntoIterator<Item = &'a ReclamationTombstone>) -> Self {
        let mut progress = Self::default();
        for tombstone in tombstones {
            match tombstone.state.status() {
                ReclamationStatus::Pending => progress.pending += 1,
                ReclamationStatus::Ready => progress.ready += 1,
                ReclamationStatus::Skipped => progress.skipped += 1,
            }
        }
        progress
    }
}

/// # Errors
///
/// Returns [`ReclamationDecisionError::StaleFence`] when `fence` predates the stored tombstone.
pub fn decide_reclamation_selection(
    digest: &ArtifactDigest,
    snapshot: &ReclamationSnapshot,
    referenced: bool,
    required_frontier: u64,
    fence: u64,
    now: i64,
) -> Result<SelectOutcome, ReclamationDecisionError> {
    guard_fence(snapshot.tombstone.as_ref(), fence)?;
    let skip = skip_reason(snapshot, referenced);
    Ok(match (skip, snapshot.tombstone.as_ref()) {
        (Some(reason), Some(prior)) => SelectOutcome::Skipped(advance(
            prior,
            ReclamationState::Skipped { reason },
            prior.required_frontier,
            fence,
            now,
        )),
        (Some(reason), None) => SelectOutcome::Ineligible(reason),
        (None, Some(prior)) => SelectOutcome::Selected(advance(
            prior,
            ReclamationState::Pending,
            prior.required_frontier.max(required_frontier),
            fence,
            now,
        )),
        (None, None) => SelectOutcome::Selected(ReclamationTombstone {
            digest: digest.clone(),
            state: ReclamationState::Pending,
            required_frontier,
            fence,
            attempts: 1,
            selected_at_unix: now,
            updated_at_unix: now,
        }),
    })
}

/// # Errors
///
/// Returns [`ReclamationDecisionError::MissingCandidate`] when no tombstone exists, or
/// [`ReclamationDecisionError::StaleFence`] when `fence` predates it.
pub fn decide_reclamation_readiness(
    snapshot: &ReclamationSnapshot,
    referenced: bool,
    observed: ObservedFrontier,
    fence: u64,
    now: i64,
) -> Result<ReadyOutcome, ReclamationDecisionError> {
    let tombstone = snapshot
        .tombstone
        .as_ref()
        .ok_or(ReclamationDecisionError::MissingCandidate)?;
    guard_fence(Some(tombstone), fence)?;
    Ok(match tombstone.state {
        ReclamationState::Ready => ReadyOutcome::Ready(tombstone.clone()),
        ReclamationState::Skipped { .. } => ReadyOutcome::Skipped(tombstone.clone()),
        ReclamationState::Pending => skip_reason(snapshot, referenced).map_or_else(
            || {
                if observed.covers(tombstone.required_frontier) {
                    ReadyOutcome::Ready(advance(
                        tombstone,
                        ReclamationState::Ready,
                        tombstone.required_frontier,
                        fence,
                        now,
                    ))
                } else {
                    ReadyOutcome::NotReady {
                        tombstone: advance(
                            tombstone,
                            ReclamationState::Pending,
                            tombstone.required_frontier,
                            fence,
                            now,
                        ),
                        observed,
                    }
                }
            },
            |reason| {
                ReadyOutcome::Skipped(advance(
                    tombstone,
                    ReclamationState::Skipped { reason },
                    tombstone.required_frontier,
                    fence,
                    now,
                ))
            },
        ),
    })
}

const fn guard_fence(existing: Option<&ReclamationTombstone>, fence: u64) -> Result<(), ReclamationDecisionError> {
    match existing {
        Some(record) => record.validate_fence(fence),
        None => Ok(()),
    }
}

fn skip_reason(snapshot: &ReclamationSnapshot, referenced: bool) -> Option<SkipReason> {
    if referenced {
        Some(SkipReason::Referenced)
    } else if snapshot
        .placements
        .iter()
        .any(|record| matches!(record.state, BlobPlacementState::Verified { .. }))
    {
        Some(SkipReason::Serveable)
    } else {
        None
    }
}

fn advance(
    prior: &ReclamationTombstone,
    state: ReclamationState,
    required_frontier: u64,
    fence: u64,
    now: i64,
) -> ReclamationTombstone {
    ReclamationTombstone {
        digest: prior.digest.clone(),
        state,
        required_frontier,
        fence: prior.fence.max(fence),
        attempts: prior.attempts + 1,
        selected_at_unix: prior.selected_at_unix,
        updated_at_unix: now,
    }
}
