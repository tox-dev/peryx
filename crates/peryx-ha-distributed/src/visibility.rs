//! The replicated apply-state for artifact visibility: trash, restore, revoke, and lift.
//!
//! A follower applies visibility operations off the replicated log, where duplicate, reordered, and
//! replayed delivery are all possible. This state machine makes that safe. Each operation carries its
//! place in the authoritative order as an [`OpOrder`] (authority epoch first, serial within the epoch),
//! and apply is idempotent and monotonic per dimension: applying the same operation twice changes
//! nothing, and a stale operation can never overwrite a newer visibility state. A revoke that a
//! replica has applied cannot be undone by a lift that was authored earlier and merely arrived late, so
//! a replica cannot resurrect a revoked or trashed artifact on duplicate or out-of-order delivery.
//!
//! Trash/restore and revoke/lift are independent dimensions, each with its own high-water mark, so a
//! trash and a revoke on one artifact do not fence each other out by serial order.
//!
//! Beyond the live apply, this module owns the durable side of a replica's visibility: an [`encode`] /
//! [`restore`] snapshot so a follower keeps every tombstone across a log compaction, and a
//! frontier-bounded [`compact`] that releases an artifact only once a required-replica-and-backup
//! [`Frontier`] covers its operations and it has returned to the visible default. A still-trashed or
//! still-revoked tombstone is never released, because it is what holds the artifact out of sight.
//! Wiring the snapshot onto the journal and served ecosystem projections is deferred
//! to the projection work.
//!
//! [`encode`]: VisibilityState::encode
//! [`restore`]: VisibilityState::restore
//! [`compact`]: VisibilityState::compact

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// The snapshot schema this build writes and is willing to restore. A format change is a deliberate
/// migration, never a silent misread that could drop a tombstone and resurrect a revoked artifact.
pub const VISIBILITY_APPLY_SCHEMA: u32 = 1;

/// Where an operation sits in the authoritative order. A higher authority `epoch` always wins; within
/// one epoch a higher `serial` wins. Declared epoch-first so the derived ordering compares that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OpOrder {
    pub epoch: u64,
    pub serial: u64,
}

/// The artifact a visibility operation targets: its serving coordinate and content digest. Two
/// operations address the same artifact only when both match.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId {
    pub coordinate: String,
    pub digest: String,
}

/// A visibility transition. Trash and restore move an artifact in and out of the trash; revoke and lift
/// block and unblock its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VisibilityAction {
    Trash,
    Restore,
    Revoke,
    Lift,
}

/// A typed visibility operation: which artifact, what transition, and its place in the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityOp {
    pub artifact: ArtifactId,
    pub action: VisibilityAction,
    pub order: OpOrder,
}

/// Whether an [`apply`](VisibilityState::apply) advanced the state or dropped a stale delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyEffect {
    /// The operation was newer than its dimension's last, so it advanced the high-water mark.
    Applied,
    /// The operation was a duplicate, stale, or reordered delivery, so the state stands unchanged.
    Ignored,
}

/// One artifact's servability: whether it sits in the trash and whether its content is revoked. A never
/// seen artifact is visible, the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Visibility {
    pub trashed: bool,
    pub revoked: bool,
}

impl Visibility {
    /// Whether a reader may serve the artifact: neither trashed nor revoked.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        !self.trashed && !self.revoked
    }
}

/// The highest operation serial each authority epoch has durably passed on every required replica and
/// backup.
///
/// [`compact`](VisibilityState::compact) may release an artifact only once this frontier covers its
/// operations, because the authority never resends an operation below a serial it has been
/// acknowledged for everywhere, so a released entry can no longer be re-litigated by a late delivery.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frontier {
    covered: BTreeMap<u64, u64>,
}

impl Frontier {
    /// Record that `epoch` is durable through `serial` everywhere, keeping the highest serial seen so
    /// an out-of-order acknowledgement never retracts coverage.
    pub fn acknowledge(&mut self, epoch: u64, serial: u64) {
        let slot = self.covered.entry(epoch).or_default();
        *slot = (*slot).max(serial);
    }

    /// The highest serial acknowledged for `epoch`, or `None` when the frontier has recorded none. A
    /// replica reports this per epoch as the operation frontier it has durably applied.
    #[must_use]
    pub fn high_water(&self, epoch: u64) -> Option<u64> {
        self.covered.get(&epoch).copied()
    }

    /// Whether `order` sits at or below the durable frontier for its epoch. An epoch this frontier has
    /// not acknowledged covers nothing, so an operation in it is always retained.
    fn covers(&self, order: OpOrder) -> bool {
        self.covered
            .get(&order.epoch)
            .is_some_and(|&serial| order.serial <= serial)
    }

    /// Whether `order` and every epoch beneath it are settled, so forgetting an entry whose high-water is
    /// `order` cannot let a late lower-epoch operation resurrect it.
    ///
    /// [`covers`](Self::covers) settles the order's own epoch through its serial, but a dimension's
    /// high-water in a later epoch overwrote and forgot the orders it held in earlier ones, and those
    /// earlier epochs can still deliver an operation below the high-water that the fence would reject.
    /// Authority epochs are contiguous from one, and the frontier records a superseded epoch only once it
    /// is drained everywhere, so every epoch below `order.epoch` being acknowledged is what proves no such
    /// operation remains in flight. A single unacknowledged epoch below the high-water leaves the entry
    /// retained, so its fence stands until the earlier epoch drains.
    fn settles(&self, order: OpOrder) -> bool {
        self.covers(order) && self.covered.range(1..order.epoch).count() as u64 == order.epoch.saturating_sub(1)
    }
}

/// A snapshot that cannot be trusted to preserve every tombstone, so restore refuses it rather than
/// rebuild a visibility that silently drops one and resurrects a revoked or trashed artifact.
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

/// The persisted serde shape of one artifact's converged visibility: both flags and the high-water
/// order of each dimension, so a restore keeps the fencing that makes a later stale op a no-op.
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

/// The persisted serde shape of a [`VisibilityState`]: a schema tag guarding the retained tombstones,
/// so a format change is a deliberate migration rather than a silent misread.
#[derive(Serialize, Deserialize)]
struct VisibilityStateSnapshot {
    schema: u32,
    artifacts: Vec<EntrySnapshot>,
}

/// The visibility of every artifact a stream of operations has touched, kept idempotent and monotonic
/// so replay and reordering cannot resurrect an older state.
#[derive(Debug, Default, Clone)]
pub struct VisibilityState {
    artifacts: HashMap<ArtifactId, Entry>,
}

impl VisibilityState {
    /// An empty state, in which every artifact is visible.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply `op`, returning whether it advanced the state or was dropped as stale.
    ///
    /// The operation's dimension (trash/restore or revoke/lift) advances only when `op.order` is newer
    /// than that dimension's last applied order; otherwise the state stands, so a duplicate or a
    /// reordered older operation cannot overwrite a newer visibility.
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

    /// The current visibility of `artifact`, or the visible default when no operation has touched it.
    #[must_use]
    pub fn get(&self, artifact: &ArtifactId) -> Visibility {
        self.artifacts
            .get(artifact)
            .map_or_else(Visibility::default, |entry| Visibility {
                trashed: entry.trashed,
                revoked: entry.revoked,
            })
    }

    /// The number of artifacts the state currently retains.
    #[must_use]
    pub fn retained_artifacts(&self) -> usize {
        self.artifacts.len()
    }

    /// Release every artifact that has returned to the visible default and whose operations `frontier`
    /// covers, bounding retention once an entry can no longer be re-litigated by a late delivery.
    ///
    /// A still-trashed or still-revoked artifact is kept regardless of the frontier, because its
    /// tombstone is what holds the artifact out of sight. An artifact with an operation the frontier has
    /// not settled is kept so a lagging replica or backup still converges to the same state, and an entry
    /// whose high-water sits in a later epoch is kept until every earlier epoch is drained too, so a stale
    /// lower-epoch operation cannot resurrect it once the entry is forgotten. Compaction never turns a
    /// visible artifact invisible or the reverse: it only forgets an entry whose absence reads as the
    /// visible default it already holds.
    pub fn compact(&mut self, frontier: &Frontier) {
        self.artifacts.retain(|_, entry| {
            entry.trashed
                || entry.revoked
                || entry.trashed_at.is_some_and(|order| !frontier.settles(order))
                || entry.revoked_at.is_some_and(|order| !frontier.settles(order))
        });
    }

    /// Serialize the retained tombstones to their JSON snapshot form, so a follower keeps every
    /// visibility fact across a log compaction.
    ///
    /// # Panics
    /// Panics only if serializing to JSON fails, which the field types make unreachable.
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

    /// Restore a state from its snapshot, preserving both the converged visibility and the per-dimension
    /// high-water order that keeps a later stale operation a no-op.
    ///
    /// # Errors
    /// Returns [`SnapshotError::Malformed`] for unparseable bytes and [`SnapshotError::UnsupportedSchema`]
    /// for a schema this build does not restore. Restore fails closed rather than rebuild a partial
    /// visibility, because a dropped tombstone would resurrect a revoked or trashed artifact.
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
