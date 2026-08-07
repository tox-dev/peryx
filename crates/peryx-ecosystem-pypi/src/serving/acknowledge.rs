//! Deciding a hosted `PyPI` write's datacenter acknowledgement and mapping it to the client response.
//!
//! An admitted upload is durable on the ingress node the moment its bytes commit, which proves the
//! artifact's same-datacenter byte durability for a filesystem backend. This folds that local placement
//! receipt into the configured quorum over the datacenter's members and combines it with the metadata
//! decision through [`FilesystemAck::decide`], then maps the outcome to an HTTP response: a proven write
//! succeeds, and one still short of quorum when the deadline expires is reported retry-safe rather than
//! failed, because a durable completion may land after the client stops waiting.
//!
//! Gathering receipts from other datacenter members is the replication layer's, and is not wired yet, so
//! the only receipt available in-request is the local node's. A single-node or single-member-per-DC
//! deployment reaches quorum from it; a larger same-DC quorum stays unproven until that transport lands,
//! and the write is reported retry-safe-unknown rather than durable. The decision itself is pure: its
//! member set, receipts, metadata decision, and deadline are inputs, so a test drives every phase without
//! a clock or a network.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use peryx_core::TopologyConfig;
use peryx_driver::state::DcDurabilityMetrics;
use peryx_ha_distributed::{
    AckDecision, DEFAULT_FRONTIER_POLL, DEFAULT_RECEIPT_POLL, DcAck, Deadline, DurabilityPolicy, FilesystemAck,
    MetadataOperation, ReceiptAck, ReceiptSource, RemoteDurability, RemoteFrontierSource,
    assess_remote_metadata_durability, gather_receipts, gather_remote_acks,
};
use peryx_storage::blob::Digest;

/// The node id attributed to the local placement receipt when no roster names this node.
const STANDALONE_NODE: &str = "local";

/// The same-datacenter member identities a write must reach quorum across: every roster member sharing
/// the local node's datacenter. A rosterless single node yields its own id, so a `Local` or single-member
/// quorum is provable from the local receipt alone.
pub(super) fn same_dc_members(topology: &TopologyConfig) -> BTreeSet<String> {
    let dc = super::admission::ingress_dc(topology);
    let members: BTreeSet<String> = topology
        .members
        .iter()
        .filter(|member| member.dc == dc)
        .map(|member| member.node.clone())
        .collect();
    if members.is_empty() {
        return BTreeSet::from([local_node_id(topology)]);
    }
    members
}

/// The node identity the local placement receipt is attributed to: the configured local node, or the
/// standalone id when no roster names it. Always a member of [`same_dc_members`].
pub(super) fn local_node_id(topology: &TopologyConfig) -> String {
    topology
        .local_node
        .clone()
        .unwrap_or_else(|| STANDALONE_NODE.to_owned())
}

/// The write's [`FilesystemAck`] seeded with the ingress node's own placement receipt: an admitted upload
/// is byte-durable on the local node the moment its bytes commit, which is the first receipt toward the
/// same-datacenter quorum.
pub(super) fn local_ack(
    policy: DurabilityPolicy,
    members: BTreeSet<String>,
    local_node: String,
    digest: Digest,
) -> FilesystemAck {
    let mut ack = FilesystemAck::new(digest.clone(), members, policy);
    ack.record(ReceiptAck {
        node: local_node,
        digest,
    });
    ack
}

/// The metadata dimension a datacenter write acknowledges against.
pub(super) enum MetadataDimension<'a> {
    /// A `dc` write's metadata commits to the local journal synchronously, so the local commit is the
    /// whole metadata dimension and it is already decided.
    Local(AckDecision),
    /// An `ha` write additionally requires an eligible remote datacenter to have committed the exact
    /// operation. The transport gathers remote acknowledgements under the write deadline and the pure
    /// [`assess_remote_metadata_durability`] fold decides.
    Remote {
        sources: &'a [Arc<dyn RemoteFrontierSource + Send + Sync>],
        authority: &'a str,
        operation: MetadataOperation,
    },
}

/// Gather the write's durability evidence into `ack`, decide its datacenter acknowledgement, and record
/// it.
///
/// The byte dimension gathers same-datacenter placement receipts; an `ha` write's metadata dimension
/// concurrently gathers eligible remote datacenters' acknowledgements of the operation. Both gathers run
/// under one client `budget`, so an HA write waits once for both dimensions rather than twice. The write
/// is [`Durable`](DcAck::Durable) only when both prove within the budget; a dimension left short when the
/// budget expires resolves retry-safe [`Unknown`](DcAck::Unknown) rather than a definite failure, since a
/// durable copy or a remote commit may land after the client stops waiting and a retry then resolves to
/// success. The decision arithmetic stays pure; this composes the transport and the metrics around it.
pub(super) async fn resolve_dc_ack(
    mut ack: FilesystemAck,
    metadata: MetadataDimension<'_>,
    digest: &Digest,
    sources: &[Arc<dyn ReceiptSource + Send + Sync>],
    budget: Duration,
    metrics: &DcDurabilityMetrics,
) -> DcAck {
    match metadata {
        MetadataDimension::Local(decision) => {
            let deadline = gather_receipts(sources, digest, &mut ack, budget, DEFAULT_RECEIPT_POLL).await;
            record_and_decide(&ack, decision, deadline, metrics)
        }
        MetadataDimension::Remote {
            sources: remotes,
            authority,
            operation,
        } => {
            let mut remote_acks = Vec::new();
            let (byte_deadline, remote_deadline) = tokio::join!(
                gather_receipts(sources, digest, &mut ack, budget, DEFAULT_RECEIPT_POLL),
                gather_remote_acks(
                    remotes,
                    authority,
                    &operation,
                    &mut remote_acks,
                    budget,
                    DEFAULT_FRONTIER_POLL
                ),
            );
            let remote = assess_remote_metadata_durability(&operation, &remote_acks);
            let decision = remote_metadata_decision(&remote, operation.frontier);
            record_and_decide(&ack, decision, both_live(byte_deadline, remote_deadline), metrics)
        }
    }
}

/// Fold the held evidence into the client outcome: decide against the metadata `decision` and combined
/// `deadline`, record the outcome and the byte-quorum progress, and return the outcome.
fn record_and_decide(
    ack: &FilesystemAck,
    decision: AckDecision,
    deadline: Deadline,
    metrics: &DcDurabilityMetrics,
) -> DcAck {
    let outcome = ack.decide(decision, deadline);
    metrics.record_quorum(&ack.byte_decision());
    metrics.record(outcome);
    outcome
}

/// The metadata acknowledgement an HA write reaches from its gathered remote durability: acknowledged once
/// an eligible remote holds the operation, otherwise not-yet-durable at the operation's `frontier`.
const fn remote_metadata_decision(remote: &RemoteDurability, frontier: u64) -> AckDecision {
    if remote.is_durable() {
        AckDecision::Acknowledged
    } else {
        AckDecision::NotYetDurable {
            target: frontier,
            durable_frontier: 0,
        }
    }
}

/// The deadline both dimensions read: live only when each gather reached durability within the budget,
/// expired the moment either spent the budget short of it, so a write short on either dimension resolves
/// retry-safe rather than falsely durable.
const fn both_live(byte: Deadline, remote: Deadline) -> Deadline {
    match (byte, remote) {
        (Deadline::Live, Deadline::Live) => Deadline::Live,
        _ => Deadline::Expired,
    }
}

/// The client-facing outcome of a write's durability decision.
pub(super) struct AckResponse {
    /// `200` once the write is proven datacenter-durable, `202` while it stays retry-safe pending.
    pub status: StatusCode,
    /// The body replayed verbatim to a retry that finds the operation already finalized.
    pub body: Vec<u8>,
    /// Whether the operation ledger records a terminal result. Only a proven-durable write finalizes; a
    /// pending or unknown one is left open so a retry re-drives it rather than replaying a false success.
    pub finalize: bool,
}

/// Map a [`DcAck`] phase to the HTTP contract. A proven write is `200` and finalizes; a write still short
/// of quorum when the deadline expires is `202` carrying its retry-safe operation id, and one still
/// within its deadline is `202` pending. Neither pending outcome finalizes.
pub(super) fn ack_response(ack: DcAck, operation: &str) -> AckResponse {
    match ack {
        DcAck::Durable { .. } => AckResponse {
            status: StatusCode::OK,
            body: b"upload accepted".to_vec(),
            finalize: true,
        },
        DcAck::Unknown => AckResponse {
            status: StatusCode::ACCEPTED,
            body: format!("upload accepted; durability pending, retry-safe operation {operation}").into_bytes(),
            finalize: false,
        },
        DcAck::Pending => AckResponse {
            status: StatusCode::ACCEPTED,
            body: b"upload accepted; durability pending".to_vec(),
            finalize: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use peryx_core::{NodeRole, TopologyConfig, TopologyMember, TopologyMode};
    use peryx_storage::blob::BlobDurability;

    use super::*;

    fn digest() -> Digest {
        Digest::of(b"artifact")
    }

    fn member(node: &str, dc: &str, role: NodeRole) -> TopologyMember {
        TopologyMember {
            node: node.to_owned(),
            dc: dc.to_owned(),
            address: format!("{node}.{dc}:8080"),
            role,
        }
    }

    fn topology(local: &str, members: Vec<TopologyMember>) -> TopologyConfig {
        TopologyConfig {
            mode: TopologyMode::Dc,
            group: Some("group".to_owned()),
            local_node: Some(local.to_owned()),
            members,
        }
    }

    fn ack_over(policy: DurabilityPolicy, names: &[&str]) -> FilesystemAck {
        local_ack(
            policy,
            names.iter().map(|node| (*node).to_owned()).collect(),
            "a".to_owned(),
            digest(),
        )
    }

    fn sources(list: Vec<peryx_ha_distributed::LoopbackReceiptSource>) -> Vec<Arc<dyn ReceiptSource + Send + Sync>> {
        list.into_iter()
            .map(|source| Arc::new(source) as Arc<dyn ReceiptSource + Send + Sync>)
            .collect()
    }

    fn rendered(metrics: &DcDurabilityMetrics) -> String {
        use peryx_driver::state::PrometheusSource as _;
        let mut body = String::new();
        metrics.write_metrics(&mut body);
        body
    }

    const BUDGET: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn test_local_receipt_alone_resolves_durable_and_records_it() {
        let metrics = DcDurabilityMetrics::default();
        let ack = resolve_dc_ack(
            ack_over(DurabilityPolicy::Local, &["a"]),
            MetadataDimension::Local(AckDecision::Acknowledged),
            &digest(),
            &[],
            BUDGET,
            &metrics,
        )
        .await;

        assert_eq!(
            ack,
            DcAck::Durable {
                scope: BlobDurability::Filesystem
            }
        );
        let body = rendered(&metrics);
        assert!(
            body.contains("peryx_dc_ack_durable_total{scope=\"filesystem\"} 1\n"),
            "{body}"
        );
        assert!(body.contains("peryx_dc_ack_quorum_acknowledged 1\n"), "{body}");
    }

    #[tokio::test]
    async fn test_a_peer_receipt_completes_a_majority_quorum() {
        let metrics = DcDurabilityMetrics::default();
        let ack = resolve_dc_ack(
            ack_over(DurabilityPolicy::Majority, &["a", "b", "c"]),
            MetadataDimension::Local(AckDecision::Acknowledged),
            &digest(),
            &sources(vec![
                peryx_ha_distributed::LoopbackReceiptSource::holding("b", digest(), 4),
                peryx_ha_distributed::LoopbackReceiptSource::absent("c"),
            ]),
            BUDGET,
            &metrics,
        )
        .await;

        assert!(matches!(ack, DcAck::Durable { .. }));
        assert!(
            rendered(&metrics).contains("peryx_dc_ack_quorum_acknowledged 2\n"),
            "the local node and one peer proved the quorum",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_short_gather_resolves_unknown_and_records_it() {
        let metrics = DcDurabilityMetrics::default();
        let ack = resolve_dc_ack(
            ack_over(DurabilityPolicy::Majority, &["a", "b", "c"]),
            MetadataDimension::Local(AckDecision::Acknowledged),
            &digest(),
            &sources(vec![
                peryx_ha_distributed::LoopbackReceiptSource::absent("b"),
                peryx_ha_distributed::LoopbackReceiptSource::absent("c"),
            ]),
            BUDGET,
            &metrics,
        )
        .await;

        assert_eq!(
            ack,
            DcAck::Unknown,
            "a durable completion may land after the client stops waiting"
        );
        assert!(rendered(&metrics).contains("peryx_dc_ack_unknown_total 1\n"));
    }

    #[tokio::test]
    async fn test_unacknowledged_metadata_holds_the_write_pending_despite_byte_quorum() {
        let metrics = DcDurabilityMetrics::default();
        let ack = resolve_dc_ack(
            ack_over(DurabilityPolicy::Local, &["a"]),
            MetadataDimension::Local(AckDecision::NotYetDurable {
                target: 5,
                durable_frontier: 0,
            }),
            &digest(),
            &[],
            BUDGET,
            &metrics,
        )
        .await;

        assert_eq!(ack, DcAck::Pending);
        assert!(rendered(&metrics).contains("peryx_dc_ack_pending_total 1\n"));
    }

    fn remote_sources(
        list: Vec<peryx_ha_distributed::LoopbackRemoteFrontierSource>,
    ) -> Vec<Arc<dyn RemoteFrontierSource + Send + Sync>> {
        list.into_iter()
            .map(|source| Arc::new(source) as Arc<dyn RemoteFrontierSource + Send + Sync>)
            .collect()
    }

    fn operation() -> MetadataOperation {
        MetadataOperation {
            epoch: 3,
            frontier: 100,
        }
    }

    #[tokio::test]
    async fn test_ha_write_is_durable_once_an_eligible_remote_holds_the_operation() {
        let metrics = DcDurabilityMetrics::default();
        let remotes = remote_sources(vec![peryx_ha_distributed::LoopbackRemoteFrontierSource::reporting(
            "east", 3, 100,
        )]);
        let ack = resolve_dc_ack(
            ack_over(DurabilityPolicy::Local, &["a"]),
            MetadataDimension::Remote {
                sources: remotes.as_slice(),
                authority: "proj",
                operation: operation(),
            },
            &digest(),
            &[],
            BUDGET,
            &metrics,
        )
        .await;

        assert_eq!(
            ack,
            DcAck::Durable {
                scope: BlobDurability::Filesystem
            }
        );
        assert!(rendered(&metrics).contains("peryx_dc_ack_durable_total{scope=\"filesystem\"} 1\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn test_ha_write_is_unknown_when_no_remote_applies_the_operation() {
        let metrics = DcDurabilityMetrics::default();
        let remotes = remote_sources(vec![peryx_ha_distributed::LoopbackRemoteFrontierSource::silent("east")]);
        let ack = resolve_dc_ack(
            ack_over(DurabilityPolicy::Local, &["a"]),
            MetadataDimension::Remote {
                sources: remotes.as_slice(),
                authority: "proj",
                operation: operation(),
            },
            &digest(),
            &[],
            BUDGET,
            &metrics,
        )
        .await;

        assert_eq!(
            ack,
            DcAck::Unknown,
            "a remote may commit after the client stops waiting"
        );
        assert!(rendered(&metrics).contains("peryx_dc_ack_unknown_total 1\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn test_ha_write_is_unknown_when_bytes_lag_despite_a_durable_remote() {
        let metrics = DcDurabilityMetrics::default();
        // The remote holds the operation, but the same-DC byte quorum is never reached, so the write
        // stays retry-safe rather than durable.
        let remotes = remote_sources(vec![peryx_ha_distributed::LoopbackRemoteFrontierSource::reporting(
            "east", 3, 100,
        )]);
        let ack = resolve_dc_ack(
            ack_over(DurabilityPolicy::Majority, &["a", "b", "c"]),
            MetadataDimension::Remote {
                sources: remotes.as_slice(),
                authority: "proj",
                operation: operation(),
            },
            &digest(),
            &[],
            BUDGET,
            &metrics,
        )
        .await;

        assert_eq!(ack, DcAck::Unknown);
    }

    #[test]
    fn test_durable_response_is_a_finalizing_ok() {
        let response = ack_response(
            DcAck::Durable {
                scope: BlobDurability::Filesystem,
            },
            "op-1",
        );
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body, b"upload accepted");
        assert!(response.finalize);
    }

    #[test]
    fn test_unknown_response_is_a_retry_safe_accepted_carrying_the_operation() {
        let response = ack_response(DcAck::Unknown, "op-1");
        assert_eq!(response.status, StatusCode::ACCEPTED);
        assert!(String::from_utf8_lossy(&response.body).contains("op-1"));
        assert!(!response.finalize, "an unproven write must not replay a false success");
    }

    #[test]
    fn test_pending_response_is_accepted_and_does_not_finalize() {
        let response = ack_response(DcAck::Pending, "op-1");
        assert_eq!(response.status, StatusCode::ACCEPTED);
        assert!(!response.finalize);
    }

    #[test]
    fn test_same_dc_members_filters_by_the_local_datacenter() {
        let topology = topology(
            "a",
            vec![
                member("a", "east", NodeRole::Writer),
                member("b", "east", NodeRole::Replica),
                member("c", "west", NodeRole::Replica),
            ],
        );
        assert_eq!(
            same_dc_members(&topology),
            BTreeSet::from(["a".to_owned(), "b".to_owned()]),
            "a west member never counts toward the east node's same-DC quorum",
        );
        assert_eq!(local_node_id(&topology), "a");
    }

    #[test]
    fn test_a_rosterless_node_is_its_own_sole_member() {
        let topology = TopologyConfig::default();
        assert_eq!(same_dc_members(&topology), BTreeSet::from([STANDALONE_NODE.to_owned()]));
        assert_eq!(local_node_id(&topology), STANDALONE_NODE);
    }
}
