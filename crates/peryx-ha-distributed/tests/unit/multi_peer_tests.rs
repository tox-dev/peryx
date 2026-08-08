use std::collections::BTreeSet;
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use async_trait::async_trait;

use crate::backoff::ReconnectPolicy;
use crate::multi_peer::{DEFAULT_SET_LIMITS, MemberOutcome, PeerSet, RoundReport, SetLimits};
use crate::peer::{
    BatchFrame, BatchRequest, LoopbackPeer, LoopbackTransport, PeerFault, PeerTransport, TransferLimits, TransportError,
};
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("non-zero")
}

fn limits(max_concurrent: usize, request_size: usize, per_peer_budget: usize, jitter: Duration) -> SetLimits {
    SetLimits {
        max_concurrent: nz(max_concurrent),
        request_size: nz(request_size),
        per_peer_budget: nz(per_peer_budget),
        jitter,
    }
}

fn policy(max_attempts: u32) -> ReconnectPolicy {
    ReconnectPolicy::new(
        Duration::from_millis(100),
        NonZeroU32::new(2).unwrap(),
        Duration::from_secs(30),
        NonZeroU32::new(max_attempts).unwrap(),
    )
}

fn peer_with(source: &str, token: &str, count: u64) -> LoopbackPeer {
    let mut peer = LoopbackPeer::new(source, token, TransferLimits::default());
    for index in 0..count {
        peer.append(format!("event-{index}").into_bytes());
    }
    peer
}

fn progressed(outcome: &MemberOutcome) -> (&str, usize, u64, bool) {
    match outcome {
        MemberOutcome::Progressed {
            source,
            buffered,
            through,
            caught_up,
        } => (source, *buffered, *through, *caught_up),
        other => panic!("expected Progressed, got {other:?}"),
    }
}

#[test]
fn test_default_limits_match_the_documented_constant() {
    assert_eq!(SetLimits::default(), DEFAULT_SET_LIMITS);
}

#[tokio::test]
async fn test_a_lone_peer_drains_to_its_frontier_in_one_round() {
    let peer = peer_with("a", "tok", 3);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default());
    set.join("a", LoopbackTransport::connect(&peer, "tok"), 0);

    let report = set.advance(Duration::ZERO).await;

    assert_eq!(report.advanced(), 1);
    let (source, buffered, through, caught_up) = progressed(&report.outcomes[0]);
    assert_eq!((source, buffered, through, caught_up), ("a", 3, 3, true));
    assert_eq!(set.buffered("a"), Some(3));
}

#[tokio::test]
async fn test_drain_and_commit_advance_the_durable_frontier() {
    let peer = peer_with("a", "tok", 2);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default());
    set.join("a", LoopbackTransport::connect(&peer, "tok"), 0);
    set.advance(Duration::ZERO).await;

    let drained = set.drain("a");

    assert_eq!(drained.len(), 2);
    assert_eq!(set.buffered("a"), Some(0));
    set.commit("a", 2);
    assert_eq!(set.frontier("a"), Some(2));
}

#[tokio::test]
async fn test_a_re_drain_after_a_committed_apply_replays_no_committed_change() {
    // A response lost after a durable apply leaves the frontier committed; the next drain resumes from
    // it and the peer serves only changes past it, so a re-drain never replays a committed change.
    let peer = peer_with("a", "tok", 5);
    let mut set = PeerSet::new(limits(4, 3, 8, Duration::ZERO), ReconnectPolicy::default());
    set.join("a", LoopbackTransport::connect(&peer, "tok"), 0);
    set.advance(Duration::ZERO).await;
    let first = set.drain("a");
    set.commit("a", 3);

    let report = set.advance(Duration::ZERO).await;
    let replayed = set.drain("a");

    assert_eq!(
        first.iter().map(|change| change.serial).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(progressed(&report.outcomes[0]), ("a", 2, 5, true));
    assert_eq!(
        replayed.iter().map(|change| change.serial).collect::<Vec<_>>(),
        vec![4, 5]
    );
}

#[tokio::test]
async fn test_a_drained_peer_is_held_out_until_it_commits() {
    let peer = peer_with("a", "tok", 5);
    let mut set = PeerSet::new(limits(4, 3, 8, Duration::ZERO), ReconnectPolicy::default());
    set.join("a", LoopbackTransport::connect(&peer, "tok"), 0);
    set.advance(Duration::ZERO).await;
    let drained = set.drain("a");

    // A fetch between the drain and the commit would resume from the regressed frontier and re-request
    // the drained changes; the peer is held out instead, so this round selects nothing.
    let held = set.advance(Duration::ZERO).await;
    assert_eq!(held.advanced(), 0, "a drained-but-uncommitted peer is not fetched");

    set.commit("a", 3);
    let resumed = set.advance(Duration::ZERO).await;
    let served = set.drain("a");

    assert_eq!(
        drained.iter().map(|change| change.serial).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(resumed.advanced(), 1, "the peer is due again once committed");
    assert_eq!(
        served.iter().map(|change| change.serial).collect::<Vec<_>>(),
        vec![4, 5]
    );
}

#[tokio::test]
async fn test_draining_an_empty_peer_leaves_it_in_the_rotation() {
    let peer = peer_with("a", "tok", 2);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default());
    set.join("a", LoopbackTransport::connect(&peer, "tok"), 0);

    // Draining before any fetch pops nothing, so the peer is never held out of the rotation.
    assert!(set.drain("a").is_empty());
    let report = set.advance(Duration::ZERO).await;

    assert_eq!(report.advanced(), 1, "an empty drain does not gate the peer");
}

#[tokio::test]
async fn test_a_duplicate_commit_cannot_move_the_frontier_backward() {
    let peer = peer_with("a", "tok", 3);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default());
    set.join("a", LoopbackTransport::connect(&peer, "tok"), 0);
    set.advance(Duration::ZERO).await;
    set.drain("a");
    set.commit("a", 3);

    set.commit("a", 1);

    assert_eq!(set.frontier("a"), Some(3));
}

#[tokio::test]
async fn test_a_full_channel_backpressures_and_bounds_retained_memory() {
    let peer = peer_with("a", "tok", 5);
    let mut set = PeerSet::new(limits(4, 5, 2, Duration::ZERO), ReconnectPolicy::default());
    set.join("a", LoopbackTransport::connect(&peer, "tok"), 0);

    let report = set.advance(Duration::ZERO).await;

    assert_eq!(
        report.outcomes[0],
        MemberOutcome::BackPressured {
            source: "a".to_owned(),
            buffered: 2,
            through: 2,
        }
    );
    assert_eq!(set.buffered("a"), Some(2));
}

#[tokio::test]
async fn test_a_backpressured_peer_is_skipped_until_it_is_drained() {
    let peer = peer_with("a", "tok", 5);
    let mut set = PeerSet::new(limits(4, 5, 2, Duration::ZERO), ReconnectPolicy::default());
    set.join("a", LoopbackTransport::connect(&peer, "tok"), 0);
    set.advance(Duration::ZERO).await;

    let stalled = set.advance(Duration::ZERO).await;
    assert_eq!(stalled.advanced(), 0);

    set.drain("a");
    set.commit("a", 2);
    let resumed = set.advance(Duration::ZERO).await;
    assert_eq!(
        resumed.outcomes[0],
        MemberOutcome::BackPressured {
            source: "a".to_owned(),
            buffered: 2,
            through: 4,
        }
    );
}

#[tokio::test]
async fn test_a_round_advances_no_more_than_the_concurrency_bound() {
    let sources: Vec<String> = (0..5).map(|index| format!("p{index}")).collect();
    let peers: Vec<LoopbackPeer> = sources.iter().map(|source| peer_with(source, "tok", 1)).collect();
    let mut set = PeerSet::new(limits(2, 8, 8, Duration::ZERO), ReconnectPolicy::default());
    for (source, peer) in sources.iter().zip(&peers) {
        set.join(source.clone(), LoopbackTransport::connect(peer, "tok"), 0);
    }

    let mut served: BTreeSet<String> = BTreeSet::new();
    for _ in 0..5 {
        let report = set.advance(Duration::ZERO).await;
        assert!(report.advanced() <= 2, "a round exceeded the concurrency bound");
        for outcome in &report.outcomes {
            let (source, buffered, _, _) = progressed(outcome);
            if buffered > 0 {
                served.insert(source.to_owned());
            }
        }
    }

    assert_eq!(served.len(), 5, "round-robin left a peer unserved");
}

#[tokio::test]
async fn test_a_slow_peer_backs_off_without_blocking_the_others() {
    let good_a = peer_with("a", "tok", 2);
    let slow = peer_with("b", "tok", 2);
    let good_c = peer_with("c", "tok", 2);
    slow.inject(PeerFault::Disconnect);
    let mut set = PeerSet::new(limits(3, 8, 8, Duration::ZERO), policy(10));
    set.join("a", LoopbackTransport::connect(&good_a, "tok"), 0);
    set.join("b", LoopbackTransport::connect(&slow, "tok"), 0);
    set.join("c", LoopbackTransport::connect(&good_c, "tok"), 0);

    let report = set.advance(Duration::ZERO).await;

    let backing_off = report
        .outcomes
        .iter()
        .filter(|outcome| matches!(outcome, MemberOutcome::RetryAfter { source, .. } if source == "b"))
        .count();
    let progressed_count = report
        .outcomes
        .iter()
        .filter(|outcome| matches!(outcome, MemberOutcome::Progressed { .. }))
        .count();
    assert_eq!(backing_off, 1, "the slow peer should back off");
    assert_eq!(progressed_count, 2, "the healthy peers should progress alongside it");
}

#[tokio::test]
async fn test_a_backed_off_peer_is_due_again_only_after_its_delay() {
    let slow = peer_with("b", "tok", 2);
    slow.inject(PeerFault::Timeout);
    let mut set = PeerSet::new(limits(4, 8, 8, Duration::ZERO), policy(10));
    set.join("b", LoopbackTransport::connect(&slow, "tok"), 0);

    let MemberOutcome::RetryAfter { delay, .. } = set.advance(Duration::ZERO).await.outcomes[0].clone() else {
        panic!("expected a retry");
    };

    assert_eq!(set.advance(Duration::ZERO).await.advanced(), 0, "still backing off");
    let resumed = set.advance(delay).await;
    assert_eq!(progressed(&resumed.outcomes[0]), ("b", 2, 2, true));
}

#[tokio::test]
async fn test_a_bad_credential_retires_a_peer_terminally() {
    let peer = peer_with("a", "tok", 2);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy(10));
    set.join("a", LoopbackTransport::connect(&peer, "wrong"), 0);

    let report = set.advance(Duration::ZERO).await;

    assert_eq!(
        report.outcomes[0],
        MemberOutcome::GaveUp {
            source: "a".to_owned(),
            reason: "unauthenticated",
        }
    );
    assert_eq!(
        set.advance(Duration::ZERO).await.advanced(),
        0,
        "a retired peer never returns"
    );
}

#[tokio::test]
async fn test_an_exhausted_retry_budget_retires_a_peer() {
    let slow = peer_with("b", "tok", 2);
    slow.inject(PeerFault::Disconnect);
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy(1));
    set.join("b", LoopbackTransport::connect(&slow, "tok"), 0);

    let report = set.advance(Duration::ZERO).await;

    assert_eq!(
        report.outcomes[0],
        MemberOutcome::GaveUp {
            source: "b".to_owned(),
            reason: "retry_exhausted",
        }
    );
}

struct GappyTransport;

#[async_trait]
impl PeerTransport for GappyTransport {
    async fn fetch_batch(&self, request: BatchRequest) -> Result<BatchFrame, TransportError> {
        let page = ChangePage {
            version: PROTOCOL_VERSION,
            source: "gappy".to_owned(),
            after: request.after,
            current_serial: request.after + 5,
            changes: vec![Change {
                serial: request.after + 2,
                event: Vec::new(),
                metadata: Vec::new(),
                blobs: Vec::new(),
            }],
        };
        Ok(BatchFrame::new(page))
    }
}

#[tokio::test]
async fn test_a_non_contiguous_batch_retires_the_peer() {
    let mut set = PeerSet::new(DEFAULT_SET_LIMITS, policy(10));
    set.join("gappy", GappyTransport, 0);

    let report = set.advance(Duration::ZERO).await;

    assert_eq!(
        report.outcomes[0],
        MemberOutcome::GaveUp {
            source: "gappy".to_owned(),
            reason: "frontier_gap",
        }
    );
}

#[tokio::test]
async fn test_an_empty_set_advances_nothing() {
    let mut set: PeerSet<GappyTransport> = PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default());

    assert_eq!(set.advance(Duration::ZERO).await, RoundReport::default());
}

#[tokio::test]
async fn test_accessors_ignore_an_unknown_peer() {
    let mut set: PeerSet<GappyTransport> = PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default());

    assert_eq!(set.frontier("ghost"), None);
    assert_eq!(set.buffered("ghost"), None);
    assert!(set.drain("ghost").is_empty());
    set.commit("ghost", 9);
    assert_eq!(set.frontier("ghost"), None);
}

#[tokio::test]
async fn test_disabled_jitter_leaves_the_backoff_at_the_base_delay() {
    let slow = peer_with("b", "tok", 1);
    slow.inject(PeerFault::Disconnect);
    let mut set = PeerSet::new(limits(4, 8, 8, Duration::ZERO), policy(10));
    set.join("b", LoopbackTransport::connect(&slow, "tok"), 0);

    let MemberOutcome::RetryAfter { delay, .. } = set.advance(Duration::ZERO).await.outcomes[0].clone() else {
        panic!("expected a retry");
    };

    assert_eq!(
        delay,
        Duration::from_millis(100),
        "no jitter means exactly the base delay"
    );
}

#[tokio::test]
async fn test_jitter_spreads_retries_and_stays_within_its_window() {
    let window = Duration::from_millis(50);
    let base = Duration::from_millis(100);
    let sources = ["a", "b", "c", "d"];
    let peers: Vec<LoopbackPeer> = sources.iter().map(|source| peer_with(source, "tok", 1)).collect();
    for peer in &peers {
        peer.inject(PeerFault::Disconnect);
    }
    let mut set = PeerSet::new(limits(4, 8, 8, window), policy(10));
    for (source, peer) in sources.iter().zip(&peers) {
        set.join(*source, LoopbackTransport::connect(peer, "tok"), 0);
    }

    let report = set.advance(Duration::ZERO).await;

    let mut delays: BTreeSet<Duration> = BTreeSet::new();
    for outcome in &report.outcomes {
        let MemberOutcome::RetryAfter { delay, .. } = outcome else {
            panic!("expected a retry");
        };
        assert!(
            *delay >= base && *delay < base + window,
            "delay {delay:?} left its window"
        );
        delays.insert(*delay);
    }
    assert!(delays.len() > 1, "jitter should not retry every peer in lockstep");
}
