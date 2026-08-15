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

fn peer(count: u64) -> LoopbackPeer {
    let mut peer = LoopbackPeer::new(SOURCE, "tok", TransferLimits::default());
    for index in 0..count {
        peer.append(format!("event-{index}").into_bytes());
    }
    peer
}

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

fn apply_page(page: ChangePage) -> Result<u64, TestFailure> {
    let ChangePage {
        version,
        source,
        after,
        changes,
        ..
    } = page;
    if version != PROTOCOL_VERSION || source != SOURCE {
        return Err(TestFailure);
    }
    Ok(changes.last().map_or(after, |change| change.serial))
}

#[test]
fn test_a_function_applier_rejects_an_incompatible_page() {
    assert_eq!(
        apply_page(ChangePage {
            version: PROTOCOL_VERSION + 1,
            source: SOURCE.to_owned(),
            after: 0,
            current_serial: 0,
            changes: Vec::new(),
        }),
        Err(TestFailure)
    );
}

#[tokio::test]
async fn test_a_function_applier_acknowledges_a_page() {
    let source = peer(1);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", LoopbackTransport::connect(&source, "tok"), 0);

    let round = pull_round(&mut set, Duration::ZERO, 0, None, apply_page).await.unwrap();

    assert_eq!((round.serial, round.applied), (1, 1));
}

#[tokio::test]
async fn test_a_lone_peer_converges_to_its_head() {
    let source = peer(3);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", LoopbackTransport::connect(&source, "tok"), 0);
    let mut applier = Applier::default();

    let after = applier.serial;
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

    let round = pull_round(&mut set, Duration::ZERO, 0, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();

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
    let round = pull_round(&mut set, Duration::ZERO, 0, None, apply_page).await.unwrap();

    assert_eq!(
        (round.serial, round.applied, round.caught_up, round.answered),
        (0, 0, false, false)
    );
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

    let after = applier.serial;
    pull_round(&mut set, Duration::ZERO, after, None, |page| {
        Ok::<u64, TestFailure>(applier.apply(&page))
    })
    .await
    .unwrap();
    assert_eq!(applier.serial, 3);

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

    let error = pull_round(&mut set, Duration::ZERO, 0, None, |_page| Err::<u64, _>(TestFailure))
        .await
        .unwrap_err();

    assert_eq!(error, TestFailure);
    assert_eq!((set.frontier("a"), set.frontier("b")), (Some(0), Some(0)));
    assert_eq!((set.buffered("a"), set.buffered("b")), (Some(0), Some(0)));
}

#[tokio::test]
async fn test_a_peer_on_an_unsupported_version_is_reported_not_applied() {
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    set.join("a", UnsupportedVersion, 0);
    let round = pull_round(&mut set, Duration::ZERO, 0, None, apply_page).await.unwrap();

    assert_eq!(round.incompatible, Some(PROTOCOL_VERSION + 1));
    assert_eq!((round.serial, round.applied), (0, 0));
}

#[tokio::test]
async fn test_an_empty_set_answers_nothing() {
    let mut set: PeerSet<LoopbackTransport<'_>> = PeerSet::new(DEFAULT_SET_LIMITS, policy());
    assert!(set.is_empty());
    let round = pull_round(&mut set, Duration::ZERO, 0, None, apply_page).await.unwrap();

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

    assert_eq!(
        (round.serial, round.applied, round.caught_up, round.head),
        (2, 2, false, 5)
    );
    assert_eq!(set.source(), Some(SOURCE));
}
