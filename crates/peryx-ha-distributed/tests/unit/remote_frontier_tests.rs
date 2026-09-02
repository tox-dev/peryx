use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rstest::rstest;

use crate::evidence_gather::{GatherEnd, GatherOutcome};
use crate::peer::TransportError;
use crate::remote_durability::{DurabilityPolicy, MetadataOperation, RemoteAck, assess_remote_metadata_durability};
use crate::remote_frontier::{LoopbackRemoteFrontierSource, RemoteFrontierSource, gather_remote_acks};
use crate::support::{RequestBlocker, ended};

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
) -> (GatherOutcome, Vec<RemoteAck>) {
    gather_under(sources, operation, DurabilityPolicy::Local).await
}

async fn gather_under(
    sources: &[Arc<dyn RemoteFrontierSource + Send + Sync>],
    operation: &MetadataOperation,
    policy: DurabilityPolicy,
) -> (GatherOutcome, Vec<RemoteAck>) {
    let mut acks = Vec::new();
    let outcome = gather_remote_acks(sources, AUTHORITY, operation, &mut acks, policy, BUDGET, POLL).await;
    (outcome, acks)
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

    let outcome = gather_remote_acks(
        &sources,
        AUTHORITY,
        &op(3, 100),
        &mut acks,
        DurabilityPolicy::Local,
        BUDGET,
        POLL,
    )
    .await;

    assert_eq!(outcome, ended(GatherEnd::Durable, &[]));
}

#[tokio::test(start_paused = true)]
async fn test_gather_reaches_durability_from_one_eligible_remote() {
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::reporting("east", 3, 120),
        LoopbackRemoteFrontierSource::silent("west"),
    ]);

    let (outcome, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(outcome, ended(GatherEnd::Durable, &[]));
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
        let outcome = gather_remote_acks(
            &sources,
            AUTHORITY,
            &op(3, 100),
            &mut acks,
            DurabilityPolicy::Local,
            BUDGET,
            POLL,
        )
        .await;
        (outcome, acks)
    });

    started.await.unwrap();
    let (outcome, acks) = gather.await.unwrap();

    assert_eq!(
        (outcome, durable(&op(3, 100), &acks, 2)),
        (ended(GatherEnd::Durable, &[]), true)
    );
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

    let outcome = gather_remote_acks(
        &sources,
        AUTHORITY,
        &op(3, 100),
        &mut acks,
        DurabilityPolicy::Local,
        BUDGET,
        POLL,
    )
    .await;

    assert_eq!(
        (outcome, durable(&op(3, 100), &acks, 2)),
        (ended(GatherEnd::Durable, &[]), true)
    );
}

#[tokio::test(start_paused = true)]
async fn test_gather_expires_when_no_remote_applies() {
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::silent("east"),
        LoopbackRemoteFrontierSource::silent("west"),
    ]);

    let (outcome, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(
        outcome,
        ended(GatherEnd::TimedOut, &[]),
        "a short gather is retry-safe, never durable"
    );
    assert!(!durable(&op(3, 100), &acks, sources.len()));
}

#[tokio::test(start_paused = true)]
async fn test_gather_re_polls_a_remote_that_applies_mid_window() {
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::reporting("east", 3, 100).available_after(3),
    ]);

    let (outcome, _) = gather(&sources, &op(3, 100)).await;

    assert_eq!(outcome, ended(GatherEnd::Durable, &[]));
}

#[tokio::test(start_paused = true)]
async fn test_gather_re_polls_past_a_transient_fault() {
    let reporting = LoopbackRemoteFrontierSource::reporting("east", 3, 100);
    reporting.inject(TransportError::Timeout);
    let sources = sources(vec![reporting]);

    let (outcome, _) = gather(&sources, &op(3, 100)).await;

    assert_eq!(
        outcome,
        ended(GatherEnd::Durable, &[]),
        "a transient fault is re-polled, not a failure"
    );
}

#[rstest]
#[case::unauthenticated(TransportError::Unauthenticated, "unauthenticated")]
#[case::malformed(TransportError::Malformed, "malformed")]
#[case::bad_status(TransportError::BadStatus { status: 418 }, "bad_status")]
#[tokio::test(start_paused = true)]
async fn test_gather_retires_a_remote_that_fails_terminally(
    #[case] fault: TransportError,
    #[case] reason: &'static str,
) {
    let reporting = LoopbackRemoteFrontierSource::reporting("east", 3, 100);
    reporting.inject(fault);
    let sources = sources(vec![reporting]);
    let started = tokio::time::Instant::now();

    let (outcome, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(
        (outcome, started.elapsed()),
        (ended(GatherEnd::Exhausted, &[("east", reason)]), Duration::ZERO),
        "a retired remote answers no later poll, so the write stops asking instead of waiting out its budget"
    );
    assert!(
        !durable(&op(3, 100), &acks, sources.len()),
        "the report the remote would have given on the next poll never counts"
    );
}

#[tokio::test(start_paused = true)]
async fn test_gather_names_every_retired_remote_in_source_order() {
    let west = LoopbackRemoteFrontierSource::silent("west");
    west.inject(TransportError::Unauthenticated);
    let east = LoopbackRemoteFrontierSource::silent("east");
    east.inject(TransportError::Malformed);
    let sources = sources(vec![west, east]);

    let (outcome, _) = gather(&sources, &op(3, 100)).await;

    assert_eq!(
        outcome,
        ended(
            GatherEnd::Exhausted,
            &[("east", "malformed"), ("west", "unauthenticated")]
        ),
        "remotes fail in whatever order they answer, so the record is ordered by name instead"
    );
}

#[tokio::test(start_paused = true)]
async fn test_gather_keeps_polling_a_healthy_remote_past_a_terminal_one() {
    let rejecting = LoopbackRemoteFrontierSource::silent("west");
    rejecting.inject(TransportError::Unauthenticated);
    let sources = sources(vec![
        rejecting,
        LoopbackRemoteFrontierSource::reporting("east", 3, 100).available_after(2),
    ]);

    let (outcome, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(outcome, ended(GatherEnd::Durable, &[("west", "unauthenticated")]));
    assert!(durable(&op(3, 100), &acks, sources.len()));
}

#[tokio::test(start_paused = true)]
async fn test_gather_never_counts_a_remote_at_a_stale_epoch() {
    let sources = sources(vec![LoopbackRemoteFrontierSource::reporting("east", 2, 100)]);

    let (outcome, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(outcome, ended(GatherEnd::TimedOut, &[]));
    assert!(!durable(&op(3, 100), &acks, sources.len()));
}

#[tokio::test(start_paused = true)]
async fn test_gather_keeps_only_a_remotes_latest_report() {
    let sources = sources(vec![LoopbackRemoteFrontierSource::reporting("east", 3, 99)]);

    let (outcome, acks) = gather(&sources, &op(3, 100)).await;

    assert_eq!(outcome, ended(GatherEnd::TimedOut, &[]));
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

    let (outcome, acks) = gather_under(&sources, &op(3, 100), DurabilityPolicy::Everywhere).await;

    assert_eq!(outcome, ended(GatherEnd::Durable, &[]));
    assert_eq!(acks.len(), 2, "everywhere waits for the second datacenter");
}

#[tokio::test(start_paused = true)]
async fn test_gather_expires_when_only_part_of_the_policy_quorum_reports() {
    let sources = sources(vec![
        LoopbackRemoteFrontierSource::reporting("east", 3, 100),
        LoopbackRemoteFrontierSource::silent("west"),
    ]);

    let (outcome, acks) = gather_under(&sources, &op(3, 100), DurabilityPolicy::Everywhere).await;

    assert_eq!(outcome, ended(GatherEnd::TimedOut, &[]));
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

    let outcome = gather_remote_acks(
        &sources,
        AUTHORITY,
        &op(3, 100),
        &mut acks,
        DurabilityPolicy::Everywhere,
        BUDGET,
        POLL,
    )
    .await;

    assert_eq!(outcome, ended(GatherEnd::Durable, &[]));
}
