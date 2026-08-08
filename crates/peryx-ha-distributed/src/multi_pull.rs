//! Folds a [`PeerSet`] round into a replica's single-serial apply path.
//!
//! Every metadata peer serves the same authoritative writer stream, so the replica keeps them in
//! lockstep: it advances the set, applies every fresh change any peer delivered as one contiguous run
//! from its committed serial, then commits every peer that answered forward to the new serial. A peer
//! that answered with changes a faster peer already delivered contributes nothing but is still moved
//! forward, and a peer that did not answer keeps its backoff, so a slow or dead peer never stalls the
//! others and any peer can serve the next round.

use std::time::Duration;

use crate::multi_peer::{MemberOutcome, PeerSet};
use crate::peer::PeerTransport;
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};

/// What one multi-peer pull round produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRound {
    /// The replica's serial after every fresh change this round applied.
    pub serial: u64,
    /// How many changes applied this round.
    pub applied: usize,
    /// Whether every peer that answered reached its advertised head, so the loop may idle to its poll
    /// interval rather than pulling again at once.
    pub caught_up: bool,
    /// Whether any peer answered this round. `false` means every peer is down, backing off, or absent,
    /// so the caller backs the loop off rather than spinning.
    pub answered: bool,
    /// The highest head any peer has advertised, so the caller reports how far the replica lags even when
    /// the round did not reach it.
    pub head: u64,
    /// The version a peer advertised when it is not the supported one, so the caller reports the schema
    /// incompatibility rather than applying under a version it cannot read.
    pub incompatible: Option<u16>,
}

/// Advance `set` one round and fold every fresh change it buffered into the replica's serial.
///
/// `apply` folds one page beginning at the current serial and returns the serial it reached - the
/// replica's real apply in production, a serial-tracking double under test. Each page is stamped with the
/// authoritative source: `committed` when the replica already has one, else the source a peer advertised
/// this round, so a fresh replica's first apply still pins to the writer's identity rather than an empty
/// one.
///
/// # Errors
/// Returns the first error `apply` reports, after releasing every drained peer so the next round retries
/// from the serial the failed apply left in place rather than stranding the peers it had already drained.
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

    // Take the buffered changes from every peer that answered. Sources are snapshotted first so the read
    // of each peer's depth ends before the drain that empties it.
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
    // A peer on an unsupported version is reported, not applied under: skip the fold but still release
    // every drained peer below so the next round retries.
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

    // Release every drained peer to the serial the round reached: peers whose changes applied move in
    // lockstep, and a peer whose changes a faster one already delivered - or an apply left unapplied -     // resumes from that serial without replaying a committed change.
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
