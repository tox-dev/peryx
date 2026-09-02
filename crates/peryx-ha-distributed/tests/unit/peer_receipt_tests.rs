use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use peryx_storage::blob::Digest;
use rstest::rstest;

use crate::evidence_gather::GatherEnd;
use crate::filesystem_ack::FilesystemAck;
use crate::peer::TransportError;
use crate::peer_receipt::{LoopbackReceiptSource, PeerReceipt, ReceiptRequest, ReceiptSource, gather_receipts};
use crate::readiness::DurabilityPolicy;
use crate::receipt_quorum::ReceiptAck;
use crate::support::{RequestBlocker, ended};

const BUDGET: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);
const SIZE: u64 = 7;

fn digest() -> Digest {
    Digest::of(b"artifact")
}

const fn request(digest: &Digest) -> ReceiptRequest<'_> {
    ReceiptRequest { digest, size: SIZE }
}

fn members(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn local_ack(policy: DurabilityPolicy, members_of: &[&str], local: &str) -> FilesystemAck {
    let mut ack = FilesystemAck::new(digest(), members(members_of), policy);
    ack.record(ReceiptAck {
        node: local.to_owned(),
        digest: digest(),
    });
    ack
}

fn sources(sources: Vec<LoopbackReceiptSource>) -> Vec<Arc<dyn ReceiptSource + Send + Sync>> {
    sources
        .into_iter()
        .map(|source| Arc::new(source) as Arc<dyn ReceiptSource + Send + Sync>)
        .collect()
}

struct BlockedReceiptSource {
    node: String,
    blocker: RequestBlocker,
}

#[async_trait]
impl ReceiptSource for BlockedReceiptSource {
    fn node(&self) -> &str {
        &self.node
    }

    async fn fetch_receipt(&self, _request: ReceiptRequest<'_>) -> Result<Option<PeerReceipt>, TransportError> {
        self.blocker.wait().await
    }
}

struct UnboundReceiptSource {
    node: String,
    calls: AtomicUsize,
}

#[async_trait]
impl ReceiptSource for UnboundReceiptSource {
    fn node(&self) -> &str {
        &self.node
    }

    async fn fetch_receipt(&self, _request: ReceiptRequest<'_>) -> Result<Option<PeerReceipt>, TransportError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(TransportError::ReceiptIdentity {
            expected: self.node.clone(),
            actual: "impostor".to_owned(),
        })
    }
}

#[test]
fn test_into_ack_carries_the_node_and_digest() {
    let ack: ReceiptAck = PeerReceipt {
        node: "b".to_owned(),
        digest: digest(),
        size: 12,
    }
    .into();

    assert_eq!(
        ack,
        ReceiptAck {
            node: "b".to_owned(),
            digest: digest(),
        }
    );
}

#[test]
fn test_holds_reports_a_recorded_node() {
    let ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");

    assert!(ack.holds("a"));
    assert!(!ack.holds("b"));
}

#[test]
fn test_byte_evidence_is_pending_below_quorum() {
    let ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");

    assert!(!ack.is_byte_durable());
    assert_eq!(
        ack.evidence(),
        crate::dc_ack::ByteEvidence::Filesystem(crate::byte_ack::ByteAckDecision::Pending {
            nodes: vec!["a".to_owned()],
            required: 2,
            remaining: 1,
        })
    );
}

#[test]
fn test_byte_evidence_is_acknowledged_at_quorum() {
    let ack = local_ack(DurabilityPolicy::Local, &["a"], "a");

    assert!(ack.is_byte_durable());
    assert_eq!(
        ack.evidence(),
        crate::dc_ack::ByteEvidence::Filesystem(crate::byte_ack::ByteAckDecision::Acknowledged {
            nodes: vec!["a".to_owned()],
            required: 1,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn test_gather_returns_live_without_a_query_when_local_quorum_is_met() {
    let mut ack = local_ack(DurabilityPolicy::Local, &["a"], "a");
    let source = LoopbackReceiptSource::absent("b");
    source.inject(TransportError::Disconnected);
    let sources = sources(vec![source]);

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(outcome, ended(GatherEnd::Durable, &[]));
    assert!(ack.is_byte_durable());
}

#[tokio::test(start_paused = true)]
async fn test_gather_reaches_quorum_from_a_peer_receipt() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let sources = sources(vec![
        LoopbackReceiptSource::holding("b", digest(), 7),
        LoopbackReceiptSource::absent("c"),
    ]);

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(outcome, ended(GatherEnd::Durable, &[]));
    assert_eq!(ack.independent_receipts(), 2);
    assert!(ack.is_byte_durable());
}

#[tokio::test(start_paused = true)]
async fn test_gather_queries_a_healthy_peer_while_the_first_peer_is_stalled() {
    let (blocker, started, cancelled) = RequestBlocker::new();
    let sources: Vec<Arc<dyn ReceiptSource + Send + Sync>> = vec![
        Arc::new(BlockedReceiptSource {
            node: "c".to_owned(),
            blocker,
        }),
        Arc::new(LoopbackReceiptSource::holding("b", digest(), 7).available_after(1)),
    ];
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let digest = digest();
    let gather = tokio::spawn(async move {
        let outcome = gather_receipts(&sources, request(&digest), &mut ack, BUDGET, POLL).await;
        (outcome, ack)
    });

    started.await.unwrap();
    let (outcome, ack) = gather.await.unwrap();

    assert_eq!((outcome, ack.is_byte_durable()), (ended(GatherEnd::Durable, &[]), true));
    assert_eq!(cancelled.await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn test_gather_returns_before_a_later_peer_finishes() {
    let (blocker, _, _) = RequestBlocker::new();
    let sources: Vec<Arc<dyn ReceiptSource + Send + Sync>> = vec![
        Arc::new(LoopbackReceiptSource::holding("b", digest(), 7)),
        Arc::new(BlockedReceiptSource {
            node: "c".to_owned(),
            blocker,
        }),
    ];
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!((outcome, ack.is_byte_durable()), (ended(GatherEnd::Durable, &[]), true));
}

#[tokio::test(start_paused = true)]
async fn test_gather_expires_when_peers_never_deliver() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let sources = sources(vec![
        LoopbackReceiptSource::absent("b"),
        LoopbackReceiptSource::absent("c"),
    ]);

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(
        outcome,
        ended(GatherEnd::TimedOut, &[]),
        "a short gather is retry-safe, never durable"
    );
    assert!(!ack.is_byte_durable());
}

#[tokio::test(start_paused = true)]
async fn test_gather_expires_without_an_eligible_peer() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");

    let outcome = gather_receipts(&[], request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(
        (outcome, ack.is_byte_durable()),
        (ended(GatherEnd::Exhausted, &[]), false)
    );
}

#[tokio::test(start_paused = true)]
async fn test_gather_re_polls_a_peer_that_replicates_mid_window() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let sources = sources(vec![
        LoopbackReceiptSource::holding("b", digest(), 7).available_after(3),
    ]);

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(outcome, ended(GatherEnd::Durable, &[]));
    assert!(ack.is_byte_durable());
}

#[tokio::test(start_paused = true)]
async fn test_gather_re_polls_past_a_transient_fault() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let holding = LoopbackReceiptSource::holding("b", digest(), 7);
    holding.inject(TransportError::Timeout);
    let sources = sources(vec![holding]);

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(
        outcome,
        ended(GatherEnd::Durable, &[]),
        "a transient fault is re-polled, not a failure"
    );
    assert!(ack.is_byte_durable());
}

#[rstest]
#[case::unauthenticated(TransportError::Unauthenticated, "unauthenticated")]
#[case::malformed(TransportError::Malformed, "malformed")]
#[tokio::test(start_paused = true)]
async fn test_gather_retires_a_peer_that_fails_terminally(#[case] fault: TransportError, #[case] reason: &'static str) {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let holding = LoopbackReceiptSource::holding("b", digest(), SIZE);
    holding.inject(fault);
    let sources = sources(vec![holding]);
    let started = tokio::time::Instant::now();

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(
        (outcome, started.elapsed()),
        (ended(GatherEnd::Exhausted, &[("b", reason)]), Duration::ZERO),
        "a retired peer answers no later poll, so the write stops asking instead of waiting out its budget"
    );
    assert!(!ack.is_byte_durable());
}

#[tokio::test(start_paused = true)]
async fn test_gather_skips_a_peer_it_already_holds_across_rounds() {
    let mut ack = local_ack(DurabilityPolicy::Everywhere, &["a", "b", "c"], "a");
    let sources = sources(vec![
        LoopbackReceiptSource::holding("b", digest(), 7),
        LoopbackReceiptSource::holding("c", digest(), 7).available_after(2),
    ]);

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(outcome, ended(GatherEnd::Durable, &[]));
    assert_eq!(ack.independent_receipts(), 3);
}

#[tokio::test(start_paused = true)]
async fn test_gather_ignores_a_receipt_for_another_digest() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let sources = sources(vec![LoopbackReceiptSource::holding("b", Digest::of(b"other"), 7)]);

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(outcome, ended(GatherEnd::TimedOut, &[]));
    assert_eq!(ack.independent_receipts(), 1, "only the local receipt counts");
}

#[tokio::test(start_paused = true)]
async fn test_gather_retires_a_source_that_answers_for_another_node() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let unbound = Arc::new(UnboundReceiptSource {
        node: "b".to_owned(),
        calls: AtomicUsize::new(0),
    });
    let sources: Vec<Arc<dyn ReceiptSource + Send + Sync>> = vec![unbound.clone()];

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(
        (outcome, ack.independent_receipts()),
        (ended(GatherEnd::Exhausted, &[("b", "receipt_identity")]), 1)
    );
    assert_eq!(
        unbound.calls.load(Ordering::Relaxed),
        1,
        "a protocol violation is terminal, so the source is not polled again"
    );
}

#[tokio::test(start_paused = true)]
async fn test_gather_ignores_a_receipt_from_a_non_member() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let sources = sources(vec![LoopbackReceiptSource::holding("stranger", digest(), 7)]);

    let outcome = gather_receipts(&sources, request(&digest()), &mut ack, BUDGET, POLL).await;

    assert_eq!(outcome, ended(GatherEnd::TimedOut, &[]));
    assert_eq!(ack.independent_receipts(), 1);
}
