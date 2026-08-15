//! Replicated apply state for artifact visibility.
//!
//! Each operation carries an [`OpOrder`]. Apply is idempotent and monotonic, so duplicate or reordered
//! delivery cannot overwrite newer visibility.
//!
//! Trash/restore and revoke/lift have independent high-water marks. Snapshots retain those fences, and
//! [`VisibilityState::compact`] removes visible entries after all earlier epochs settle.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// Restore rejects other schemas to prevent a misread from dropping tombstones.
pub const VISIBILITY_APPLY_SCHEMA: u32 = 1;

/// Higher epochs win; serials order operations within an epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OpOrder {
    pub epoch: u64,
    pub serial: u64,
}

/// Matching artifact identities require the same coordinate and digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId {
    pub coordinate: String,
    pub digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VisibilityAction {
    Trash,
    Restore,
    Revoke,
    Lift,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityOp {
    pub artifact: ArtifactId,
    pub action: VisibilityAction,
    pub order: OpOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyEffect {
    /// The operation was newer than its dimension's last, so it advanced the high-water mark.
    Applied,
    /// The operation was a duplicate, stale, or reordered delivery, so the state stands unchanged.
    Ignored,
}

/// Unseen artifacts default to visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Visibility {
    pub trashed: bool,
    pub revoked: bool,
}

impl Visibility {
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        !self.trashed && !self.revoked
    }
}

/// Each required replica and backup has applied these serials to durable storage.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frontier {
    covered: BTreeMap<u64, u64>,
}

impl Frontier {
    /// Out-of-order acknowledgements cannot retract coverage.
    pub fn acknowledge(&mut self, epoch: u64, serial: u64) {
        let slot = self.covered.entry(epoch).or_default();
        *slot = (*slot).max(serial);
    }

    #[must_use]
    pub fn high_water(&self, epoch: u64) -> Option<u64> {
        self.covered.get(&epoch).copied()
    }

    fn covers(&self, order: OpOrder) -> bool {
        self.covered
            .get(&order.epoch)
            .is_some_and(|&serial| order.serial <= serial)
    }

    /// Settling requires contiguous coverage from epoch one; otherwise a late lower-epoch operation
    /// could resurrect an entry after compaction removes its fence.
    fn settles(&self, order: OpOrder) -> bool {
        self.covers(order) && self.covered.range(1..order.epoch).count() as u64 == order.epoch.saturating_sub(1)
    }
}

/// Restore rejects a snapshot that omits a tombstone.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("visibility apply snapshot is malformed: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("visibility apply snapshot schema {found} is not the {expected} this build restores")]
    UnsupportedSchema { expected: u32, found: u32 },
}

#[derive(Debug, Default, Clone)]
struct Entry {
    trashed: bool,
    trashed_at: Option<OpOrder>,
    revoked: bool,
    revoked_at: Option<OpOrder>,
}

/// Persists each dimension's high-water mark so restore keeps stale operations fenced.
#[derive(Serialize, Deserialize)]
struct EntrySnapshot {
    artifact: ArtifactId,
    trashed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trashed_at: Option<OpOrder>,
    revoked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at: Option<OpOrder>,
}

#[derive(Serialize, Deserialize)]
struct VisibilityStateSnapshot {
    schema: u32,
    artifacts: Vec<EntrySnapshot>,
}

/// Applies each visibility dimension in monotonic order across replay and reordering.
#[derive(Debug, Default, Clone)]
pub struct VisibilityState {
    artifacts: HashMap<ArtifactId, Entry>,
}

impl VisibilityState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Advances the operation's dimension when its order exceeds the high-water mark.
    pub fn apply(&mut self, op: &VisibilityOp) -> ApplyEffect {
        let entry = self.artifacts.entry(op.artifact.clone()).or_default();
        let (flag, at, target) = match op.action {
            VisibilityAction::Trash => (&mut entry.trashed, &mut entry.trashed_at, true),
            VisibilityAction::Restore => (&mut entry.trashed, &mut entry.trashed_at, false),
            VisibilityAction::Revoke => (&mut entry.revoked, &mut entry.revoked_at, true),
            VisibilityAction::Lift => (&mut entry.revoked, &mut entry.revoked_at, false),
        };
        if at.is_some_and(|last| op.order <= last) {
            return ApplyEffect::Ignored;
        }
        *at = Some(op.order);
        *flag = target;
        ApplyEffect::Applied
    }

    /// Returns visible for an unseen artifact.
    #[must_use]
    pub fn get(&self, artifact: &ArtifactId) -> Visibility {
        self.artifacts
            .get(artifact)
            .map_or_else(Visibility::default, |entry| Visibility {
                trashed: entry.trashed,
                revoked: entry.revoked,
            })
    }

    #[must_use]
    pub fn retained_artifacts(&self) -> usize {
        self.artifacts.len()
    }

    /// Removes visible entries after all earlier epochs settle their dimension fences.
    pub fn compact(&mut self, frontier: &Frontier) {
        self.artifacts.retain(|_, entry| {
            entry.trashed
                || entry.revoked
                || entry.trashed_at.is_some_and(|order| !frontier.settles(order))
                || entry.revoked_at.is_some_and(|order| !frontier.settles(order))
        });
    }

    /// Snapshots tombstones and their fences across log compaction.
    ///
    /// # Panics
    /// Panics if JSON serialization fails; the field types rule out that failure.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let snapshot = VisibilityStateSnapshot {
            schema: VISIBILITY_APPLY_SCHEMA,
            artifacts: self
                .artifacts
                .iter()
                .map(|(artifact, entry)| EntrySnapshot {
                    artifact: artifact.clone(),
                    trashed: entry.trashed,
                    trashed_at: entry.trashed_at,
                    revoked: entry.revoked,
                    revoked_at: entry.revoked_at,
                })
                .collect(),
        };
        serde_json::to_vec(&snapshot).expect("a visibility apply-state snapshot always serializes to JSON")
    }

    /// # Errors
    /// Returns [`SnapshotError::Malformed`] for unparseable bytes and [`SnapshotError::UnsupportedSchema`]
    /// for an unsupported schema. Restore fails closed because a dropped tombstone could resurrect an
    /// artifact.
    pub fn restore(bytes: &[u8]) -> Result<Self, SnapshotError> {
        let snapshot: VisibilityStateSnapshot = serde_json::from_slice(bytes).map_err(SnapshotError::Malformed)?;
        if snapshot.schema != VISIBILITY_APPLY_SCHEMA {
            return Err(SnapshotError::UnsupportedSchema {
                expected: VISIBILITY_APPLY_SCHEMA,
                found: snapshot.schema,
            });
        }
        Ok(Self {
            artifacts: snapshot
                .artifacts
                .into_iter()
                .map(|entry| {
                    (
                        entry.artifact,
                        Entry {
                            trashed: entry.trashed,
                            trashed_at: entry.trashed_at,
                            revoked: entry.revoked,
                            revoked_at: entry.revoked_at,
                        },
                    )
                })
                .collect(),
        })
    }
}
