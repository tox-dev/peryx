use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use peryx_storage::blob::Digest;

use crate::dc_ack::Deadline;
use crate::filesystem_ack::FilesystemAck;
use crate::peer::TransportError;
use crate::peer_receipt::{LoopbackReceiptSource, PeerReceipt, ReceiptSource, gather_receipts};
use crate::readiness::DurabilityPolicy;
use crate::receipt_quorum::ReceiptAck;

const BUDGET: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

fn digest() -> Digest {
    Digest::of(b"artifact")
}

fn members(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A `FilesystemAck` over `members` at `policy` with the local node's own receipt already recorded, the
/// state a write reaches the gather in.
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
fn test_byte_decision_is_pending_below_quorum() {
    let ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");

    assert!(!ack.is_byte_durable());
    assert!(matches!(
        ack.byte_decision(),
        crate::byte_ack::ByteAckDecision::Pending { remaining: 1, .. }
    ));
}

#[test]
fn test_byte_decision_is_acknowledged_at_quorum() {
    let ack = local_ack(DurabilityPolicy::Local, &["a"], "a");

    assert!(ack.is_byte_durable());
    assert!(ack.byte_decision().is_acknowledged());
}

#[tokio::test(start_paused = true)]
async fn test_gather_returns_live_without_a_query_when_local_quorum_is_met() {
    let mut ack = local_ack(DurabilityPolicy::Local, &["a"], "a");
    // A source that would fault if consulted; a met quorum must return before touching it.
    let source = LoopbackReceiptSource::absent("b");
    source.inject(TransportError::Disconnected);
    let sources = sources(vec![source]);

    let deadline = gather_receipts(&sources, &digest(), &mut ack, BUDGET, POLL).await;

    assert_eq!(deadline, Deadline::Live);
    assert!(ack.is_byte_durable());
}

#[tokio::test(start_paused = true)]
async fn test_gather_reaches_quorum_from_a_peer_receipt() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let sources = sources(vec![
        LoopbackReceiptSource::holding("b", digest(), 7),
        LoopbackReceiptSource::absent("c"),
    ]);

    let deadline = gather_receipts(&sources, &digest(), &mut ack, BUDGET, POLL).await;

    assert_eq!(deadline, Deadline::Live);
    assert_eq!(ack.independent_receipts(), 2);
    assert!(ack.is_byte_durable());
}

#[tokio::test(start_paused = true)]
async fn test_gather_expires_when_peers_never_deliver() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let sources = sources(vec![
        LoopbackReceiptSource::absent("b"),
        LoopbackReceiptSource::absent("c"),
    ]);

    let deadline = gather_receipts(&sources, &digest(), &mut ack, BUDGET, POLL).await;

    assert_eq!(
        deadline,
        Deadline::Expired,
        "a short gather is retry-safe, never durable"
    );
    assert!(!ack.is_byte_durable());
}

#[tokio::test(start_paused = true)]
async fn test_gather_re_polls_a_peer_that_replicates_mid_window() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let sources = sources(vec![
        LoopbackReceiptSource::holding("b", digest(), 7).available_after(3),
    ]);

    let deadline = gather_receipts(&sources, &digest(), &mut ack, BUDGET, POLL).await;

    assert_eq!(deadline, Deadline::Live);
    assert!(ack.is_byte_durable());
}

#[tokio::test(start_paused = true)]
async fn test_gather_re_polls_past_a_transient_fault() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let holding = LoopbackReceiptSource::holding("b", digest(), 7);
    holding.inject(TransportError::Timeout);
    let sources = sources(vec![holding]);

    let deadline = gather_receipts(&sources, &digest(), &mut ack, BUDGET, POLL).await;

    assert_eq!(
        deadline,
        Deadline::Live,
        "a transient fault is re-polled, not a failure"
    );
    assert!(ack.is_byte_durable());
}

#[tokio::test(start_paused = true)]
async fn test_gather_skips_a_peer_it_already_holds_across_rounds() {
    // Everywhere over three members needs the local receipt and both peers; one peer answers at once and
    // the other only after two rounds, so the first is skipped as already-held while the gather waits.
    let mut ack = local_ack(DurabilityPolicy::Everywhere, &["a", "b", "c"], "a");
    let sources = sources(vec![
        LoopbackReceiptSource::holding("b", digest(), 7),
        LoopbackReceiptSource::holding("c", digest(), 7).available_after(2),
    ]);

    let deadline = gather_receipts(&sources, &digest(), &mut ack, BUDGET, POLL).await;

    assert_eq!(deadline, Deadline::Live);
    assert_eq!(ack.independent_receipts(), 3);
}

#[tokio::test(start_paused = true)]
async fn test_gather_ignores_a_receipt_for_another_digest() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    let sources = sources(vec![LoopbackReceiptSource::holding("b", Digest::of(b"other"), 7)]);

    let deadline = gather_receipts(&sources, &digest(), &mut ack, BUDGET, POLL).await;

    assert_eq!(deadline, Deadline::Expired);
    assert_eq!(ack.independent_receipts(), 1, "only the local receipt counts");
}

#[tokio::test(start_paused = true)]
async fn test_gather_ignores_a_receipt_from_a_non_member() {
    let mut ack = local_ack(DurabilityPolicy::Majority, &["a", "b", "c"], "a");
    // A receipt from a node outside the same-DC roster never advances the quorum.
    let sources = sources(vec![LoopbackReceiptSource::holding("stranger", digest(), 7)]);

    let deadline = gather_receipts(&sources, &digest(), &mut ack, BUDGET, POLL).await;

    assert_eq!(deadline, Deadline::Expired);
    assert_eq!(ack.independent_receipts(), 1);
}
