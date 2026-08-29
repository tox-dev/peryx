use std::collections::BTreeSet;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::response::IntoResponse as _;
use axum::routing::get;
use axum::{Json, Router, http::StatusCode};

use crate::HttpPeerTransport;
use crate::backoff::ReconnectPolicy;
use crate::multi_peer::{DEFAULT_SET_LIMITS, MemberOutcome, PeerSet, RoundReport, SetLimits};
use crate::peer::{LoopbackPeer, LoopbackTransport, PeerFault, TransferLimits};
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};
use crate::support::TestServer;

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

async fn recovering_http_peer(
    failures: usize,
    status: StatusCode,
    max_attempts: u32,
) -> (TestServer, PeerSet<HttpPeerTransport>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let router = Router::new().route(
        "/+replication/v1/changes",
        get(move || {
            let handler_calls = Arc::clone(&handler_calls);
            async move {
                if handler_calls.fetch_add(1, Ordering::Relaxed) < failures {
                    status.into_response()
                } else {
                    Json(change_page(1, 1)).into_response()
                }
            }
        }),
    );
    let (server, set) = http_peer(router, 8, max_attempts).await;
    (server, set, calls)
}

async fn protocol_recovery_http_peer() -> (TestServer, PeerSet<HttpPeerTransport>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let router = Router::new().route(
        "/+replication/v1/changes",
        get(move || {
            let call = handler_calls.fetch_add(1, Ordering::Relaxed);
            async move {
                Json(if call == 0 {
                    change_page(2, 2)
                } else {
                    change_page(1, 1)
                })
            }
        }),
    );
    let (server, set) = http_peer(router, 8, 3).await;
    (server, set, calls)
}

async fn version_change_http_peer() -> (TestServer, PeerSet<HttpPeerTransport>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let router = Router::new().route(
        "/+replication/v1/changes",
        get(move || {
            let call = handler_calls.fetch_add(1, Ordering::Relaxed);
            async move {
                Json(if call == 0 {
                    change_page(1, 1)
                } else {
                    ChangePage {
                        version: PROTOCOL_VERSION + 1,
                        source: "writer".to_owned(),
                        after: 1,
                        current_serial: 2,
                        changes: vec![Change {
                            serial: 2,
                            event: b"event".to_vec(),
                            metadata: Vec::new(),
                            blobs: Vec::new(),
                        }],
                    }
                })
            }
        }),
    );
    http_peer(router, 8, 3).await
}

async fn buffered_then_outage() -> (TestServer, PeerSet<HttpPeerTransport>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let router = Router::new().route(
        "/+replication/v1/changes",
        get(move || {
            let call = handler_calls.fetch_add(1, Ordering::Relaxed);
            async move {
                if call == 0 {
                    Json(change_page(2, 1)).into_response()
                } else {
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            }
        }),
    );
    let (server, set) = http_peer(router, 1, 2).await;
    (server, set, calls)
}

async fn http_peer(router: Router, request_size: usize, max_attempts: u32) -> (TestServer, PeerSet<HttpPeerTransport>) {
    let server = TestServer::start(router).await;
    let mut set = PeerSet::new(limits(1, request_size, 8, Duration::ZERO), policy(max_attempts));
    set.join(
        "primary",
        HttpPeerTransport::new(&server.url, "tok", TransferLimits::default(), Duration::from_secs(1)).unwrap(),
        0,
    );
    (server, set)
}

fn change_page(current_serial: u64, change_serial: u64) -> ChangePage {
    ChangePage {
        version: PROTOCOL_VERSION,
        source: "writer".to_owned(),
        after: 0,
        current_serial,
        changes: vec![Change {
            serial: change_serial,
            event: b"event".to_vec(),
            metadata: Vec::new(),
            blobs: Vec::new(),
        }],
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
    assert_eq!(
        report.outcomes[0],
        MemberOutcome::Progressed {
            source: "a".to_owned(),
            buffered: 3,
            through: 3,
            caught_up: true,
        }
    );
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
    assert_eq!(
        report.outcomes[0],
        MemberOutcome::Progressed {
            source: "a".to_owned(),
            buffered: 2,
            through: 5,
            caught_up: true,
        }
    );
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
    let peers: Vec<LoopbackPeer> = sources.iter().map(|_| peer_with("writer", "tok", 1)).collect();
    let mut set = PeerSet::new(limits(2, 8, 8, Duration::ZERO), ReconnectPolicy::default());
    for (source, peer) in sources.iter().zip(&peers) {
        set.join(source.clone(), LoopbackTransport::connect(peer, "tok"), 0);
    }

    let mut served: BTreeSet<String> = BTreeSet::new();
    for _ in 0..5 {
        let report = set.advance(Duration::ZERO).await;
        assert!(report.advanced() <= 2, "a round exceeded the concurrency bound");
        for outcome in &report.outcomes {
            if let MemberOutcome::Progressed { source, buffered, .. } = outcome
                && *buffered > 0
            {
                served.insert(source.to_owned());
            }
        }
    }

    assert_eq!(served.len(), 5, "round-robin left a peer unserved");
}

#[tokio::test]
async fn test_a_slow_peer_backs_off_without_blocking_the_others() {
    let good_a = peer_with("writer", "tok", 2);
    let slow = peer_with("writer", "tok", 2);
    let good_c = peer_with("writer", "tok", 2);
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

    let report = set.advance(Duration::ZERO).await;
    assert_eq!(
        report.outcomes,
        vec![MemberOutcome::RetryAfter {
            source: "b".to_owned(),
            delay: Duration::from_millis(100),
        }]
    );

    assert_eq!(set.advance(Duration::ZERO).await.advanced(), 0, "still backing off");
    let resumed = set.advance(Duration::from_millis(100)).await;
    assert_eq!(
        resumed.outcomes[0],
        MemberOutcome::Progressed {
            source: "b".to_owned(),
            buffered: 2,
            through: 2,
            caught_up: true,
        }
    );
}

#[tokio::test]
async fn test_a_bad_credential_quarantines_a_peer() {
    let peer = peer_with("a", "tok", 2);
    let mut set = PeerSet::new(limits(4, 256, 1024, Duration::ZERO), policy(10));
    set.join("a", LoopbackTransport::connect(&peer, "wrong"), 0);

    let report = set.advance(Duration::ZERO).await;

    assert_eq!(
        report.outcomes[0],
        MemberOutcome::Quarantined {
            source: "a".to_owned(),
            reason: "unauthenticated",
            delay: Duration::from_secs(30),
        }
    );
    assert_eq!(
        set.advance(Duration::ZERO).await.advanced(),
        0,
        "a quarantined peer waits for its delay"
    );
}

#[tokio::test]
async fn test_an_exhausted_retry_budget_quarantines_a_peer() {
    let slow = peer_with("b", "tok", 2);
    slow.inject(PeerFault::Disconnect);
    let mut set = PeerSet::new(limits(4, 256, 1024, Duration::ZERO), policy(1));
    set.join("b", LoopbackTransport::connect(&slow, "tok"), 0);

    let report = set.advance(Duration::ZERO).await;

    assert_eq!(
        report.outcomes[0],
        MemberOutcome::Quarantined {
            source: "b".to_owned(),
            reason: "retry_exhausted",
            delay: Duration::from_millis(100),
        }
    );
}

#[tokio::test]
async fn test_a_bad_exchange_becomes_due_and_recovers() {
    let (_server, mut set, calls) = recovering_http_peer(1, StatusCode::TOO_MANY_REQUESTS, 3).await;

    let quarantined = set.advance(Duration::ZERO).await;
    assert_eq!(
        quarantined.outcomes,
        vec![MemberOutcome::Quarantined {
            source: "primary".to_owned(),
            reason: "bad_status",
            delay: Duration::from_millis(400),
        }]
    );
    assert_eq!(
        quarantined.retired,
        vec![crate::RetiredPeer {
            source: "primary".to_owned(),
            reason: "bad_status",
        }]
    );
    assert!(quarantined.fully_retired);
    assert_eq!(set.advance(Duration::from_millis(99)).await.advanced(), 0);
    assert_eq!(set.advance(Duration::from_secs(1)).await.advanced(), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn test_an_outage_past_the_retry_budget_becomes_due_and_recovers() {
    let (_server, mut set, calls) = recovering_http_peer(2, StatusCode::SERVICE_UNAVAILABLE, 2).await;

    assert!(matches!(
        set.advance(Duration::ZERO).await.outcomes.as_slice(),
        [MemberOutcome::RetryAfter { .. }]
    ));
    assert!(matches!(
        set.advance(Duration::from_millis(100)).await.outcomes.as_slice(),
        [MemberOutcome::Quarantined {
            reason: "retry_exhausted",
            ..
        }]
    ));
    assert_eq!(set.advance(Duration::from_secs(1)).await.advanced(), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn test_quarantine_preserves_attempts_across_commit_and_becomes_due() {
    let (_server, mut set, calls) = buffered_then_outage().await;

    assert!(matches!(
        set.advance(Duration::ZERO).await.outcomes.as_slice(),
        [MemberOutcome::Progressed { caught_up: false, .. }]
    ));
    assert_eq!(
        set.advance(Duration::ZERO).await.outcomes,
        vec![MemberOutcome::RetryAfter {
            source: "primary".to_owned(),
            delay: Duration::from_millis(100),
        }]
    );
    assert_eq!(set.drain("primary").len(), 1);
    set.commit("primary", 1);
    assert_eq!(set.advance(Duration::from_millis(99)).await.advanced(), 0);
    assert_eq!(
        set.advance(Duration::from_millis(100)).await.outcomes,
        vec![MemberOutcome::Quarantined {
            source: "primary".to_owned(),
            reason: "retry_exhausted",
            delay: Duration::from_millis(200),
        }]
    );
    assert_eq!(set.advance(Duration::from_millis(299)).await.advanced(), 0);
    assert_eq!(
        set.advance(Duration::from_millis(300)).await.outcomes,
        vec![MemberOutcome::Quarantined {
            source: "primary".to_owned(),
            reason: "retry_exhausted",
            delay: Duration::from_millis(200),
        }]
    );
    assert_eq!(calls.load(Ordering::Relaxed), 4);
}

#[tokio::test]
async fn test_a_protocol_violation_requires_an_explicit_rearm() {
    let (_server, mut set, calls) = protocol_recovery_http_peer().await;

    assert!(matches!(
        set.advance(Duration::ZERO).await.outcomes.as_slice(),
        [MemberOutcome::GaveUp {
            reason: "frontier_gap",
            ..
        }]
    ));
    assert_eq!(set.advance(Duration::from_secs(1)).await.advanced(), 0);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(set.rearm("primary"));
    assert!(!set.rearm("primary"));
    assert!(matches!(
        set.advance(Duration::from_secs(1)).await.outcomes.as_slice(),
        [MemberOutcome::Progressed { caught_up: true, .. }]
    ));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn test_a_foreign_source_cannot_replace_a_buffered_generation() {
    let writer = peer_with("writer", "tok", 1);
    let foreign = peer_with("foreign", "tok", 1);
    let mut set = PeerSet::new(limits(1, 8, 8, Duration::ZERO), policy(3));
    set.join("writer-peer", LoopbackTransport::connect(&writer, "tok"), 0);
    set.join("foreign-peer", LoopbackTransport::connect(&foreign, "tok"), 0);

    assert!(matches!(
        set.advance(Duration::ZERO).await.outcomes.as_slice(),
        [MemberOutcome::Progressed { source, .. }] if source == "writer-peer"
    ));
    let rejected = set.advance(Duration::ZERO).await;

    assert_eq!(set.source(), Some("writer"));
    assert_eq!(set.head(), 1);
    assert_eq!(
        (set.buffered("writer-peer"), set.buffered("foreign-peer")),
        (Some(1), Some(0))
    );
    assert_eq!(
        rejected.outcomes,
        vec![MemberOutcome::GaveUp {
            source: "foreign-peer".to_owned(),
            reason: "source_changed",
        }]
    );
    assert_eq!(set.drain("writer-peer")[0].event, b"event-0");
}

#[tokio::test]
async fn test_an_unsupported_version_cannot_replace_a_buffered_generation() {
    let (_server, mut set) = version_change_http_peer().await;

    set.advance(Duration::ZERO).await;
    let rejected = set.advance(Duration::ZERO).await;

    assert_eq!(set.source(), Some("writer"));
    assert_eq!(
        (set.version(), set.head(), set.buffered("primary")),
        (PROTOCOL_VERSION, 1, Some(1))
    );
    assert_eq!(rejected.incompatible, Some(PROTOCOL_VERSION + 1));
    assert_eq!(
        rejected.outcomes,
        vec![MemberOutcome::GaveUp {
            source: "primary".to_owned(),
            reason: "unsupported_version",
        }]
    );
}

#[tokio::test]
async fn test_an_empty_source_cannot_initialize_a_generation() {
    let router = Router::new().route(
        "/+replication/v1/changes",
        get(|| async {
            Json(ChangePage {
                source: String::new(),
                ..change_page(1, 1)
            })
        }),
    );
    let (_server, mut set) = http_peer(router, 8, 3).await;

    let rejected = set.advance(Duration::ZERO).await;

    assert_eq!((set.source(), set.head(), set.buffered("primary")), (None, 0, Some(0)));
    assert_eq!(
        rejected.outcomes,
        vec![MemberOutcome::GaveUp {
            source: "primary".to_owned(),
            reason: "source_changed",
        }]
    );
}

#[tokio::test]
async fn test_an_empty_set_advances_nothing() {
    let mut set: PeerSet<HttpPeerTransport> = PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default());

    assert_eq!(set.advance(Duration::ZERO).await, RoundReport::default());
}

#[tokio::test]
async fn test_accessors_ignore_an_unknown_peer() {
    let mut set: PeerSet<HttpPeerTransport> = PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default());

    assert_eq!(set.frontier("ghost"), None);
    assert_eq!(set.buffered("ghost"), None);
    assert!(set.drain("ghost").is_empty());
    assert!(!set.rearm("ghost"));
    set.commit("ghost", 9);
    assert_eq!(set.frontier("ghost"), None);
}

#[tokio::test]
async fn test_disabled_jitter_leaves_the_backoff_at_the_base_delay() {
    let slow = peer_with("b", "tok", 1);
    slow.inject(PeerFault::Disconnect);
    let mut set = PeerSet::new(limits(4, 8, 8, Duration::ZERO), policy(10));
    set.join("b", LoopbackTransport::connect(&slow, "tok"), 0);

    assert_eq!(
        set.advance(Duration::ZERO).await.outcomes,
        vec![MemberOutcome::RetryAfter {
            source: "b".to_owned(),
            delay: Duration::from_millis(100),
        }]
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
    let mut set = PeerSet::new(limits(5, 8, 8, window), policy(10));
    for (source, peer) in sources.iter().zip(&peers) {
        set.join(*source, LoopbackTransport::connect(peer, "tok"), 0);
    }
    let healthy = peer_with("healthy", "tok", 1);
    set.join("healthy", LoopbackTransport::connect(&healthy, "tok"), 0);

    let report = set.advance(Duration::ZERO).await;

    let delays: BTreeSet<Duration> = report
        .outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            MemberOutcome::RetryAfter { delay, .. } => Some(*delay),
            _ => None,
        })
        .collect();
    assert_eq!(delays.len(), peers.len());
    assert!(
        report
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, MemberOutcome::Progressed { source, .. } if source == "healthy"))
    );
    for delay in &delays {
        assert!(
            *delay >= base && *delay < base + window,
            "delay {delay:?} left its window"
        );
    }
    assert!(delays.len() > 1, "jitter should not retry every peer in lockstep");
}
