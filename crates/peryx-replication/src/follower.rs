//! The receiver half of `AppendEntries` (Raft §5.3) and the commit-index rule a follower applies to
//! it (issue #498). Where [`crate::consensus`] owns the log and [`crate::election`] owns the term and
//! vote, this composes both into the decision a node makes when a leader pushes entries at it.
//!
//! [`receive_append_entries`] refuses, fail-closed, every request that would break a safety rule. A
//! leader whose term is below the node's own is stale and rejected, its term reported back so the
//! leader steps down. Otherwise the node adopts the leader's term through
//! [`PersistentState::observe_term`], forgetting a vote it cast in an older term, and hands the batch
//! to [`RaftLog::append`], which runs the §5.3 consistency check and truncates a conflicting suffix;
//! a rejected append means the node's log has no matching entry at the leader's `prev` position, so
//! the request is rejected rather than forced.
//!
//! An accepted request advances the commit index by the §5.3 rule through [`CommitTracker::follow`]:
//! `min(leader_commit, index of the last new entry)`, and never backwards, so a delayed request that
//! carries a stale `leader_commit` cannot un-commit entries already reported committed.

use crate::consensus::{AppendEntries, AppendOutcome, LogEntry, LogIndex, RaftLog, RaftLogError, Term};
use crate::election::PersistentState;

/// A leader's `AppendEntries` call: the leader's term, the `(prev_index, prev_term)` the leader
/// believes this node already holds, the entries to add after it, and how far the leader has committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendRequest {
    pub leader_term: Term,
    pub prev_index: LogIndex,
    pub prev_term: Term,
    pub entries: Vec<LogEntry>,
    pub leader_commit: LogIndex,
}

/// An accepted [`receive_append_entries`]: what the append changed in the log and where the commit
/// index sits afterward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendAccepted {
    pub log: AppendOutcome,
    pub commit_index: LogIndex,
    pub committed: bool,
}

/// Why a node refused an `AppendEntries` request. Either answer leaves the node's log intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendReject {
    /// The leader's term is below the node's current term.
    StaleTerm,
    /// The node's log has no entry matching the leader's `prev` position, or the batch is malformed.
    Log(RaftLogError),
}

/// The node's answer to an `AppendEntries` request: the term it now holds and whether it accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendResponse {
    pub term: Term,
    pub result: Result<AppendAccepted, AppendReject>,
}

impl AppendResponse {
    /// Whether the node accepted the entries.
    #[must_use]
    pub const fn accepted(&self) -> bool {
        self.result.is_ok()
    }
}

/// Decide a leader's `AppendEntries` against this node's term, log, and commit index.
///
/// A leader whose term is stale is rejected outright. Otherwise the node adopts the leader's term
/// (dropping a stale vote), runs the batch through the log's §5.3 consistency check and conflict
/// truncation, and advances its commit index to the entries it now holds. The returned term is the
/// node's term after any adoption, so a rejected leader learns it is behind.
pub fn receive_append_entries(
    state: &mut PersistentState,
    log: &mut impl RaftLog,
    commit: &mut CommitTracker,
    request: &AppendRequest,
) -> AppendResponse {
    if request.leader_term < state.current_term() {
        return AppendResponse {
            term: state.current_term(),
            result: Err(AppendReject::StaleTerm),
        };
    }
    if request.leader_term > state.current_term() {
        let _ = state.observe_term(request.leader_term);
    }
    let append = AppendEntries {
        prev_index: request.prev_index,
        prev_term: request.prev_term,
        entries: request.entries.clone(),
    };
    let outcome = match log.append(&append) {
        Ok(outcome) => outcome,
        Err(reason) => {
            return AppendResponse {
                term: state.current_term(),
                result: Err(AppendReject::Log(reason)),
            };
        }
    };
    let batch = u64::try_from(request.entries.len()).unwrap_or(u64::MAX);
    let last_new_index = LogIndex(request.prev_index.0 + batch);
    let committed = commit.follow(request.leader_commit, last_new_index);
    AppendResponse {
        term: state.current_term(),
        result: Ok(AppendAccepted {
            log: outcome,
            commit_index: commit.commit_index(),
            committed,
        }),
    }
}

/// A follower's volatile commit progress: the highest index known committed and the highest already
/// applied, each of which only ever moves forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitTracker {
    commit_index: LogIndex,
    last_applied: LogIndex,
}

impl Default for CommitTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitTracker {
    /// A tracker at the sentinel: nothing committed, nothing applied.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            commit_index: LogIndex::ZERO,
            last_applied: LogIndex::ZERO,
        }
    }

    /// The highest index known committed.
    #[must_use]
    pub const fn commit_index(&self) -> LogIndex {
        self.commit_index
    }

    /// The highest index already applied to the state machine.
    #[must_use]
    pub const fn last_applied(&self) -> LogIndex {
        self.last_applied
    }

    /// Follow the leader's commit index (Raft §5.3): advance to `min(leader_commit, last_new_index)`,
    /// but never backwards. Returns whether the commit index moved.
    ///
    /// Clamping to `last_new_index` keeps the node from committing past what it holds, and refusing a
    /// lower target keeps a delayed or reordered request from un-committing settled entries.
    pub fn follow(&mut self, leader_commit: LogIndex, last_new_index: LogIndex) -> bool {
        let target = leader_commit.min(last_new_index);
        if target > self.commit_index {
            self.commit_index = target;
            true
        } else {
            false
        }
    }

    /// Advance `last_applied` one index toward the commit index, returning the index now applied, or
    /// `None` when application has caught up to the commit index.
    pub fn apply_next(&mut self) -> Option<LogIndex> {
        if self.last_applied < self.commit_index {
            self.last_applied = LogIndex(self.last_applied.0 + 1);
            Some(self.last_applied)
        } else {
            None
        }
    }
}
