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
//! This module is the pure state and its `apply`; wiring it onto the journal and the served `PyPI`,
//! OCI, and search projections is deferred to the projection work.

use std::collections::HashMap;

/// Where an operation sits in the authoritative order. A higher authority `epoch` always wins; within
/// one epoch a higher `serial` wins. Declared epoch-first so the derived ordering compares that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpOrder {
    pub epoch: u64,
    pub serial: u64,
}

/// The artifact a visibility operation targets: its serving coordinate and content digest. Two
/// operations address the same artifact only when both match.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactId {
    pub coordinate: String,
    pub digest: String,
}

/// A visibility transition. Trash and restore move an artifact in and out of the trash; revoke and lift
/// block and unblock its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Default)]
struct Entry {
    trashed: bool,
    trashed_at: Option<OpOrder>,
    revoked: bool,
    revoked_at: Option<OpOrder>,
}

/// The visibility of every artifact a stream of operations has touched, kept idempotent and monotonic
/// so replay and reordering cannot resurrect an older state.
#[derive(Debug, Default)]
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
}
