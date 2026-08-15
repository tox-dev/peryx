//! Applies fresh changes from equivalent metadata peers, then advances each drained peer to the applied
//! serial. Unanswered peers keep their backoff state.

use std::time::Duration;

use crate::multi_peer::{MemberOutcome, PeerSet};
use crate::peer::PeerTransport;
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRound {
    pub serial: u64,
    pub applied: usize,
    /// True when each peer that answered reached its advertised head.
    pub caught_up: bool,
    /// False when all peers are unavailable, backing off, or absent.
    pub answered: bool,
    /// The highest advertised peer head, including heads not reached this round.
    pub head: u64,
    pub incompatible: Option<u16>,
}

/// Applies pages from the current serial. `apply` returns the serial it committed. A fresh replica uses
/// the peer's advertised source until it has a committed source.
///
/// # Errors
/// Returns the first `apply` error after resetting all drained peers to the last committed serial.
pub async fn pull_round<T, F, E>(
    set: &mut PeerSet<T>,
    now: Duration,
    after: u64,
    committed: Option<&str>,
    mut apply: F,
) -> Result<PullRound, E>
where
    T: PeerTransport,
    F: FnMut(ChangePage) -> Result<u64, E>,
{
    let report = set.advance(now).await;
    let answered = report.outcomes.iter().any(|outcome| {
        matches!(
            outcome,
            MemberOutcome::Progressed { .. } | MemberOutcome::BackPressured { .. }
        )
    });
    let caught_up = answered
        && report
            .outcomes
            .iter()
            .all(|outcome| matches!(outcome, MemberOutcome::Progressed { caught_up: true, .. }));
    let source = committed.map(str::to_owned).or_else(|| set.source().map(str::to_owned));
    let incompatible = (set.version() != PROTOCOL_VERSION).then_some(set.version());

    let mut drained: Vec<(String, Vec<Change>)> = Vec::new();
    for peer in set.sources() {
        if set.buffered(&peer).is_some_and(|held| held > 0) {
            let changes = set.drain(&peer);
            drained.push((peer, changes));
        }
    }

    let mut serial = after;
    let mut applied = 0;
    let mut failure = None;
    // An unsupported version must not reach `apply`; drained peers still reset below.
    if incompatible.is_none()
        && let Some(source) = source.as_deref()
    {
        for (_peer, changes) in &drained {
            if failure.is_some() {
                break;
            }
            let fresh: Vec<Change> = changes
                .iter()
                .filter(|change| change.serial > serial)
                .cloned()
                .collect();
            let Some(reached) = fresh.last().map(|change| change.serial) else {
                continue;
            };
            let count = fresh.len();
            let page = ChangePage {
                version: PROTOCOL_VERSION,
                source: source.to_owned(),
                after: serial,
                current_serial: reached,
                changes: fresh,
            };
            match apply(page) {
                Ok(reached) => {
                    serial = reached;
                    applied += count;
                }
                Err(error) => failure = Some(error),
            }
        }
    }

    // Keep drained peers at the committed serial to avoid replaying a change supplied by another peer.
    for (peer, _changes) in &drained {
        set.commit(peer, serial);
    }

    if let Some(error) = failure {
        return Err(error);
    }
    Ok(PullRound {
        serial,
        applied,
        caught_up,
        answered,
        head: set.head(),
        incompatible,
    })
}

#[cfg(test)]
#[path = "../tests/unit/multi_pull/tests.rs"]
mod tests;
