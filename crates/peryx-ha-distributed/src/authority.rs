//! The authority-epoch fence: whether a piece of work may proceed under the current committed epoch.
//!
//! Authority transfer advances an authority's epoch. A former home and its workers must stop mutating
//! state once a newer epoch commits, or a paused-then-restarted old home could publish, delete, or
//! acknowledge against state a new home now owns. This fence enforces that: it admits only work
//! carrying the current committed epoch and fences everything else, so two concurrent old- and
//! new-epoch requests resolve to one outcome.
//!
//! The fence is a pure consumer of committed epochs. Producing them — the Raft-replicated ownership
//! state machine — lives elsewhere; here [`commit`](AuthorityFence::commit) records a committed epoch
//! and [`admit`](AuthorityFence::admit) reads it. Wiring the gate into metadata writes, the outbox,
//! replica apply, acknowledgement, and background work is a deferred follow-up.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::envelope::AuthorityEpoch;

/// The canonical key of an authority whose epoch fences its work: a project or repository home.
/// Distinct authorities fence independently, each with its own committed epoch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthorityKey(pub String);

/// Whether a [`commit`](AuthorityFence::commit) advanced an authority's committed epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// The epoch was newer than the authority's committed epoch, so the gate moved to it.
    Committed,
    /// The epoch was not newer, a duplicate or a stale transfer record, so the gate stands.
    Ignored,
}

/// Whether work carrying an authority epoch may proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The work carries the current committed epoch and may proceed.
    Admit,
    /// The work's epoch is not the committed one, so it is fenced without effect. `presented` is the
    /// work's epoch and `committed` the authority's current one.
    Fenced {
        committed: AuthorityEpoch,
        presented: AuthorityEpoch,
    },
}

/// The committed authority epoch for each authority, and the gate that fences work by it.
///
/// An authority with no committed epoch reads as [`AuthorityEpoch(0)`](AuthorityEpoch), the unassigned
/// sentinel; since a real authority epoch is assigned from one, an unassigned authority fences all
/// work.
#[derive(Debug, Default)]
pub struct AuthorityFence {
    committed: HashMap<AuthorityKey, AuthorityEpoch>,
}

impl AuthorityFence {
    /// An empty fence, in which every authority is unassigned and fences all work.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The committed epoch for `authority`, or the [`AuthorityEpoch(0)`](AuthorityEpoch) unassigned
    /// sentinel when no epoch has committed for it.
    #[must_use]
    pub fn committed_epoch(&self, authority: &AuthorityKey) -> AuthorityEpoch {
        self.committed.get(authority).copied().unwrap_or(AuthorityEpoch(0))
    }

    /// Record `epoch` as `authority`'s committed epoch when it is newer, so a duplicate or a stale
    /// transfer record is a no-op. Only `authority`'s gate moves; other authorities are untouched.
    pub fn commit(&mut self, authority: &AuthorityKey, epoch: AuthorityEpoch) -> CommitOutcome {
        if epoch <= self.committed_epoch(authority) {
            return CommitOutcome::Ignored;
        }
        self.committed.insert(authority.clone(), epoch);
        CommitOutcome::Committed
    }

    /// Whether work carrying `epoch` under `authority` may proceed: only the current committed epoch is
    /// admitted, so a stale epoch below it and an epoch ahead of it are both fenced.
    #[must_use]
    pub fn admit(&self, authority: &AuthorityKey, epoch: AuthorityEpoch) -> Admission {
        let committed = self.committed_epoch(authority);
        if committed != AuthorityEpoch(0) && epoch == committed {
            Admission::Admit
        } else {
            Admission::Fenced {
                committed,
                presented: epoch,
            }
        }
    }
}
