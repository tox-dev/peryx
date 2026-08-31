use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::dc_ack::Deadline;
use crate::peer::TransportError;
use crate::remote_durability::{DurabilityPolicy, MetadataOperation, RemoteAck, assess_remote_metadata_durability};
use crate::remote_frontier::{LoopbackRemoteFrontierSource, RemoteFrontierSource, gather_remote_acks};
use crate::support::RequestBlocker;

const BUDGET: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);
const AUTHORITY: &str = "proj";

fn op(epoch: u64, frontier: u64) -> MetadataOperation {
    MetadataOperation { epoch, frontier }
}

fn sources(list: Vec<LoopbackRemoteFrontierSource>) -> Vec<Arc<dyn RemoteFrontierSource + Send + Sync>> {
    list.into_iter()
        .map(|source| Arc::new(source) as Arc<dyn RemoteFrontierSource + Send + Sync>)
        .collect()
}

async fn gather(
    sources: &[Arc<dyn RemoteFrontierSource + Send + Sync>],
    operation: &MetadataOperation,
) -> (Deadline, Vec<RemoteAck>) {
    gather_under(sources, operation, DurabilityPolicy::Local).await
}

async fn gather_under(
    sources: &[Arc<dyn RemoteFrontierSource + Send + Sync>],
    operation: &MetadataOperation,
    policy: DurabilityPolicy,
) -> (Deadline, Vec<RemoteAck>) {
    let mut acks = Vec::new();
    let deadline = gather_remote_acks(sources, AUTHORITY, operation, &mut acks, policy, BUDGET, POLL).await;
    (deadline, acks)
}

fn durable(operation: &MetadataOperation, acks: &[RemoteAck], configured: usize) -> bool {
    assess_remote_metadata_durability(operation, acks, configured, DurabilityPolicy::Local).is_durable()
}

struct BlockedRemoteFrontierSource {
    datacenter: String,
    blocker: RequestBlocker,
}

#[async_trait]
impl RemoteFrontierSource for BlockedRemoteFrontierSource {
    fn datacenter(&self) -> &str {
        &self.datacenter
    }

    async fn fetch_frontier(&self, _authority: &str) -> Result<Option<RemoteAck>, TransportError> {
        self.blocker.wait().await
    }
}

#[test]
fn test_datacenter_reports_the_configured_remote() {
    assert_eq!(LoopbackRemoteFrontierSource::silent("west").datacenter(), "west");
}

#[tokio::test(start_paused = true)]
async fn test_gather_returns_live_without_a_query_when_already_durable() {
    let mut acks = vec![RemoteAck {
        datacenter: "east".to_owned(),
        epoch: 3,
        applied_frontier: 100,
    }];
    let source = LoopbackRemoteFrontierSource::silent("west");
    source.inject(TransportError::Disconnected);
    let sources = sources(vec![source]);

    let deadline = gather_remote_acks(
        &sources,
        AUTHORITY,
        &op(3, 100),
        &mut acks,
        DurabilityPolicy::Local,
        BUDGET,
        POLL,
    )
    .await;

    assert_eq!(deadline, Deadline::Live);
}

#[tokio::test(start_paused = true)]
async fn test_gather_reaches_durability_from_one_eligible_remote() {
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::reporting("east", 3, 120),
        LoopbackRemoteFrontierSource::silent("west"),
    ]);

    let (deadline, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(deadline, Deadline::Live);
    assert!(durable(&op(3, 100), &acks, sources.len()));
}

#[tokio::test(start_paused = true)]
async fn test_gather_queries_a_healthy_remote_while_the_first_remote_is_stalled() {
    let (blocker, started, cancelled) = RequestBlocker::new();
    let stalled_source = BlockedRemoteFrontierSource {
        datacenter: "west".to_owned(),
        blocker,
    };
    assert_eq!(stalled_source.datacenter(), "west");
    let sources: Vec<Arc<dyn RemoteFrontierSource + Send + Sync>> = vec![
        Arc::new(stalled_source),
        Arc::new(LoopbackRemoteFrontierSource::reporting("east", 3, 100).available_after(1)),
    ];
    let mut acks = Vec::new();
    let gather = tokio::spawn(async move {
        let deadline = gather_remote_acks(
            &sources,
            AUTHORITY,
            &op(3, 100),
            &mut acks,
            DurabilityPolicy::Local,
            BUDGET,
            POLL,
        )
        .await;
        (deadline, acks)
    });

    started.await.unwrap();
    let (deadline, acks) = gather.await.unwrap();

    assert_eq!((deadline, durable(&op(3, 100), &acks, 2)), (Deadline::Live, true));
    assert_eq!(cancelled.await, Ok(()));
}

#[tokio::test(start_paused = true)]
async fn test_gather_returns_before_a_later_remote_finishes() {
    let (blocker, _, _) = RequestBlocker::new();
    let sources: Vec<Arc<dyn RemoteFrontierSource + Send + Sync>> = vec![
        Arc::new(LoopbackRemoteFrontierSource::reporting("east", 3, 100)),
        Arc::new(BlockedRemoteFrontierSource {
            datacenter: "west".to_owned(),
            blocker,
        }),
    ];
    let mut acks = Vec::new();

    let deadline = gather_remote_acks(
        &sources,
        AUTHORITY,
        &op(3, 100),
        &mut acks,
        DurabilityPolicy::Local,
        BUDGET,
        POLL,
    )
    .await;

    assert_eq!((deadline, durable(&op(3, 100), &acks, 2)), (Deadline::Live, true));
}

#[tokio::test(start_paused = true)]
async fn test_gather_expires_when_no_remote_applies() {
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::silent("east"),
        LoopbackRemoteFrontierSource::silent("west"),
    ]);

    let (deadline, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(
        deadline,
        Deadline::Expired,
        "a short gather is retry-safe, never durable"
    );
    assert!(!durable(&op(3, 100), &acks, sources.len()));
}

#[tokio::test(start_paused = true)]
async fn test_gather_re_polls_a_remote_that_applies_mid_window() {
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::reporting("east", 3, 100).available_after(3),
    ]);

    let (deadline, _) = gather(&sources, &op(3, 100)).await;

    assert_eq!(deadline, Deadline::Live);
}

#[tokio::test(start_paused = true)]
async fn test_gather_re_polls_past_a_transient_fault() {
    let reporting = LoopbackRemoteFrontierSource::reporting("east", 3, 100);
    reporting.inject(TransportError::Timeout);
    let sources = sources(vec![reporting]);

    let (deadline, _) = gather(&sources, &op(3, 100)).await;

    assert_eq!(
        deadline,
        Deadline::Live,
        "a transient fault is re-polled, not a failure"
    );
}

#[tokio::test(start_paused = true)]
async fn test_gather_never_counts_a_remote_at_a_stale_epoch() {
    let sources = sources(vec![LoopbackRemoteFrontierSource::reporting("east", 2, 100)]);

    let (deadline, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(deadline, Deadline::Expired);
    assert!(!durable(&op(3, 100), &acks, sources.len()));
}

#[tokio::test(start_paused = true)]
async fn test_gather_keeps_only_a_remotes_latest_report() {
    let sources = sources(vec![LoopbackRemoteFrontierSource::reporting("east", 3, 99)]);

    let (deadline, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(deadline, Deadline::Expired);
    assert_eq!(
        acks.len(),
        1,
        "the remote holds one acknowledgement, not one per poll round"
    );
}

#[tokio::test(start_paused = true)]
async fn test_gather_sorts_retained_reports_by_datacenter() {
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::reporting("west", 3, 99),
        LoopbackRemoteFrontierSource::reporting("east", 3, 99),
    ]);

    let (_, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(
        acks,
        vec![
            RemoteAck {
                datacenter: "east".to_owned(),
                epoch: 3,
                applied_frontier: 99,
            },
            RemoteAck {
                datacenter: "west".to_owned(),
                epoch: 3,
                applied_frontier: 99,
            },
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn test_gather_keeps_polling_until_the_policy_quorum_reports() {
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::reporting("east", 3, 100),
        LoopbackRemoteFrontierSource::reporting("west", 3, 100).available_after(3),
    ]);

    let (deadline, acks) = gather_under(&sources, &op(3, 100), DurabilityPolicy::Everywhere).await;

    assert_eq!(deadline, Deadline::Live);
    assert_eq!(acks.len(), 2, "everywhere waits for the second datacenter");
}

#[tokio::test(start_paused = true)]
async fn test_gather_expires_when_only_part_of_the_policy_quorum_reports() {
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::reporting("east", 3, 100),
        LoopbackRemoteFrontierSource::silent("west"),
    ]);

    let (deadline, acks) = gather_under(&sources, &op(3, 100), DurabilityPolicy::Everywhere).await;

    assert_eq!(deadline, Deadline::Expired);
    assert_eq!(
        acks,
        vec![RemoteAck {
            datacenter: "east".to_owned(),
            epoch: 3,
            applied_frontier: 100
        }]
    );
}

#[tokio::test(start_paused = true)]
async fn test_gather_accepts_seeded_evidence_that_already_meets_the_quorum() {
    let mut acks = vec![
        RemoteAck {
            datacenter: "east".to_owned(),
            epoch: 3,
            applied_frontier: 100,
        },
        RemoteAck {
            datacenter: "west".to_owned(),
            epoch: 3,
            applied_frontier: 100,
        },
    ];
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::silent("east"),
        LoopbackRemoteFrontierSource::silent("west"),
    ]);

    let deadline = gather_remote_acks(
        &sources,
        AUTHORITY,
        &op(3, 100),
        &mut acks,
        DurabilityPolicy::Everywhere,
        BUDGET,
        POLL,
    )
    .await;

    assert_eq!(deadline, Deadline::Live);
}
