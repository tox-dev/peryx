use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use async_trait::async_trait;

use super::pull_round;
use crate::backoff::ReconnectPolicy;
use crate::multi_peer::{DEFAULT_SET_LIMITS, PeerSet, SetLimits};
use crate::peer::{
    BatchFrame, BatchRequest, LoopbackPeer, LoopbackTransport, PeerFault, PeerTransport, TransferLimits, TransportError,
};
use crate::protocol::{ChangePage, PROTOCOL_VERSION};

const SOURCE: &str = "writer";

/// A peer that answers on an unsupported protocol version, advertising a head it never serves.
struct UnsupportedVersion;

#[async_trait]
impl PeerTransport for UnsupportedVersion {
    async fn fetch_batch(&self, request: BatchRequest) -> Result<BatchFrame, TransportError> {
        Ok(BatchFrame::new(ChangePage {
            version: PROTOCOL_VERSION + 1,
            source: SOURCE.to_owned(),
            after: request.after,
            current_serial: request.after + 1,
            changes: Vec::new(),
        }))
    }
}

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

fn limits(request_size: usize, per_peer_budget: usize) -> SetLimits {
    SetLimits {
        max_concurrent: nz(4),
        request_size: nz(request_size),
        per_peer_budget: nz(per_peer_budget),
        jitter: Duration::ZERO,
    }
}

fn policy() -> ReconnectPolicy {
    ReconnectPolicy::new(
        Duration::from_millis(100),
        NonZeroU32::new(2).unwrap(),
        Duration::from_secs(30),
        NonZeroU32::new(3).unwrap(),
    )
}

/// A peer serving `count` changes of the one authoritative stream.
fn peer(count: u64) -> LoopbackPeer {
    let mut peer = LoopbackPeer::new(SOURCE, "tok", TransferLimits::default());
    for index in 0..count {
        peer.append(format!("event-{index}").into_bytes());
    }
    peer
}

/// A single-serial applier that mirrors the replica's contract: a page must begin at the current serial
/// and carry a contiguous run, which it folds and acknowledges by returning the serial reached.
#[derive(Default)]
struct Applier {
    serial: u64,
    applied: Vec<u64>,
}

impl Applier {
    fn apply(&mut self, page: &ChangePage) -> u64 {
        assert_eq!(page.after, self.serial, "a page must begin at the applier's serial");
        assert_eq!(page.source, SOURCE, "every page carries the authoritative source");
        for change in &page.changes {
            assert_eq!(change.serial, self.serial + 1, "a folded run stays contiguous");
            self.serial += 1;
            self.applied.push(change.serial);
        }
        self.serial
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TestFailure;

#[tokio::test]
async fn test_a_lone_peer_converges_to_its_head() {
    let source = peer(3);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", LoopbackTransport::connect(&source, "tok"), 0);
    let mut applier = Applier::default();

    let after = applier.serial;
    // A committed source is stamped in preference to what the peer advertised.
    let round = pull_round(&mut set, Duration::ZERO, after, Some(SOURCE), |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();

    assert_eq!(
        (round.serial, round.applied, round.caught_up, round.answered),
        (3, 3, true, true)
    );
    assert_eq!(applier.applied, vec![1, 2, 3]);
    assert_eq!(set.frontier("a"), Some(3));
}

#[tokio::test]
async fn test_two_peers_at_one_head_apply_each_change_once() {
    let first = peer(3);
    let second = peer(3);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", LoopbackTransport::connect(&first, "tok"), 0);
    set.join("b", LoopbackTransport::connect(&second, "tok"), 0);
    let mut applier = Applier::default();

    let after = applier.serial;
    let round = pull_round(&mut set, Duration::ZERO, after, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();

    // Both peers deliver serials 1..3; the second's are recognized as already applied and folded once.
    assert_eq!((round.serial, round.applied), (3, 3));
    assert_eq!(applier.applied, vec![1, 2, 3]);
    assert_eq!((set.frontier("a"), set.frontier("b")), (Some(3), Some(3)));
}

#[tokio::test]
async fn test_a_caught_up_peer_extends_a_lagging_peers_run() {
    let lagging = peer(3);
    let ahead = peer(5);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", LoopbackTransport::connect(&lagging, "tok"), 0);
    set.join("b", LoopbackTransport::connect(&ahead, "tok"), 0);
    let mut applier = Applier::default();

    let after = applier.serial;
    let round = pull_round(&mut set, Duration::ZERO, after, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();

    // Peer a folds 1..3, then peer b extends the run with 4..5 without replaying 1..3.
    assert_eq!((round.serial, round.applied), (5, 5));
    assert_eq!(applier.applied, vec![1, 2, 3, 4, 5]);
    assert_eq!((set.frontier("a"), set.frontier("b")), (Some(5), Some(5)));
}

#[tokio::test]
async fn test_a_dead_peer_does_not_block_a_healthy_peer() {
    let dead = peer(3);
    dead.inject(PeerFault::Disconnect);
    let healthy = peer(3);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", LoopbackTransport::connect(&dead, "tok"), 0);
    set.join("b", LoopbackTransport::connect(&healthy, "tok"), 0);
    let mut applier = Applier::default();

    let after = applier.serial;
    let round = pull_round(&mut set, Duration::ZERO, after, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();

    // The healthy peer carries the round; the dead peer keeps its stale frontier and its backoff.
    assert_eq!((round.serial, round.applied, round.answered), (3, 3, true));
    assert_eq!(set.frontier("b"), Some(3));
    assert_eq!(set.frontier("a"), Some(0));
}

#[tokio::test]
async fn test_every_peer_down_reports_no_answer() {
    let first = peer(3);
    first.inject(PeerFault::Timeout);
    let second = peer(3);
    second.inject(PeerFault::Disconnect);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", LoopbackTransport::connect(&first, "tok"), 0);
    set.join("b", LoopbackTransport::connect(&second, "tok"), 0);
    let mut applier = Applier::default();

    let after = applier.serial;
    let round = pull_round(&mut set, Duration::ZERO, after, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();

    assert_eq!(
        (round.serial, round.applied, round.caught_up, round.answered),
        (0, 0, false, false)
    );
    assert!(applier.applied.is_empty());
}

#[tokio::test]
async fn test_a_recovered_peer_resumes_without_replay() {
    let down = peer(5);
    down.inject(PeerFault::Disconnect);
    let healthy = peer(3);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", LoopbackTransport::connect(&down, "tok"), 0);
    set.join("b", LoopbackTransport::connect(&healthy, "tok"), 0);
    let mut applier = Applier::default();

    // Round one: peer a is down, peer b advances the replica to 3.
    let after = applier.serial;
    pull_round(&mut set, Duration::ZERO, after, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();
    assert_eq!(applier.serial, 3);

    // Round two, past peer a's backoff: a recovers with a longer log, resumes from its stale frontier but
    // replays nothing the replica already holds, and extends the run to 5. Peer b has no change past 3, so
    // it delivers nothing and keeps its frontier there until the writer feeds it the rest.
    let after = applier.serial;
    let round = pull_round(&mut set, Duration::from_secs(1), after, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();

    assert_eq!((round.serial, round.applied), (5, 2));
    assert_eq!(applier.applied, vec![1, 2, 3, 4, 5]);
    assert_eq!((set.frontier("a"), set.frontier("b")), (Some(5), Some(3)));
}

#[tokio::test]
async fn test_an_apply_failure_releases_the_drained_peers() {
    let source = peer(3);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", LoopbackTransport::connect(&source, "tok"), 0);

    let error = pull_round(&mut set, Duration::ZERO, 0, None, |_page| Err::<u64, _>(TestFailure))
        .await
        .unwrap_err();

    assert_eq!(error, TestFailure);
    // The apply left the serial at zero, and the drained peer is released there rather than stranded, so
    // the next round re-fetches from the same point.
    assert_eq!(set.frontier("a"), Some(0));
    assert_eq!(set.buffered("a"), Some(0));
}

#[tokio::test]
async fn test_an_apply_failure_stops_the_fold_before_the_next_drained_peer() {
    let first = peer(3);
    let second = peer(3);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", LoopbackTransport::connect(&first, "tok"), 0);
    set.join("b", LoopbackTransport::connect(&second, "tok"), 0);

    // Both peers buffer a fresh run this round. The first peer's apply fails, so the fold breaks before
    // folding the second peer rather than applying past the failure.
    let error = pull_round(&mut set, Duration::ZERO, 0, None, |_page| Err::<u64, _>(TestFailure))
        .await
        .unwrap_err();

    assert_eq!(error, TestFailure);
    // Neither peer advanced, and both are released at the unchanged serial so the next round re-fetches.
    assert_eq!((set.frontier("a"), set.frontier("b")), (Some(0), Some(0)));
    assert_eq!((set.buffered("a"), set.buffered("b")), (Some(0), Some(0)));
}

#[tokio::test]
async fn test_a_peer_on_an_unsupported_version_is_reported_not_applied() {
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", UnsupportedVersion, 0);
    let mut applier = Applier::default();

    let after = applier.serial;
    let round = pull_round(&mut set, Duration::ZERO, after, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();

    // The version is surfaced and nothing applies under it.
    assert_eq!(round.incompatible, Some(PROTOCOL_VERSION + 1));
    assert_eq!((round.serial, round.applied), (0, 0));
    assert!(applier.applied.is_empty());
}

#[tokio::test]
async fn test_an_empty_set_answers_nothing() {
    let mut set: PeerSet<LoopbackTransport<'_>> = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    assert!(set.is_empty());
    let mut applier = Applier::default();

    let round = pull_round(&mut set, Duration::ZERO, 0, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();

    assert_eq!(
        (round.serial, round.applied, round.caught_up, round.answered),
        (0, 0, false, false)
    );
    assert_eq!(set.source(), None);
}

#[tokio::test]
async fn test_a_head_beyond_the_request_size_is_not_yet_caught_up() {
    let source = peer(5);
    let mut set = PeerSet::new(limits(2, 8), policy());
    set.join("a", LoopbackTransport::connect(&source, "tok"), 0);
    let mut applier = Applier::default();

    let after = applier.serial;
    let round = pull_round(&mut set, Duration::ZERO, after, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();

    // Only two of five changes fit the request, so the round applies them but is not caught up, and the
    // advertised head names how far the replica still lags.
    assert_eq!(
        (round.serial, round.applied, round.caught_up, round.head),
        (2, 2, false, 5)
    );
    assert_eq!(set.source(), Some(SOURCE));
}
