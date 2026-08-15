//! Work requires the current nonzero authority epoch. Exact matching prevents a restarted former
//! home from mutating state after an authority transfer.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::envelope::AuthorityEpoch;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthorityKey(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    Committed,
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Admit,
    Fenced {
        committed: AuthorityEpoch,
        presented: AuthorityEpoch,
    },
}

/// Authorities without a committed epoch read as epoch zero and fence all work.
#[derive(Debug, Default)]
pub struct AuthorityFence {
    committed: HashMap<AuthorityKey, AuthorityEpoch>,
}

impl AuthorityFence {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns epoch zero when `authority` has no committed epoch.
    #[must_use]
    pub fn committed_epoch(&self, authority: &AuthorityKey) -> AuthorityEpoch {
        self.committed.get(authority).copied().unwrap_or(AuthorityEpoch(0))
    }

    /// Advances `authority` when `epoch` is newer; duplicates and stale epochs have no effect.
    pub fn commit(&mut self, authority: &AuthorityKey, epoch: AuthorityEpoch) -> CommitOutcome {
        if epoch <= self.committed_epoch(authority) {
            return CommitOutcome::Ignored;
        }
        self.committed.insert(authority.clone(), epoch);
        CommitOutcome::Committed
    }

    /// Admits the current committed epoch and fences stale, future, and zero epochs.
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
