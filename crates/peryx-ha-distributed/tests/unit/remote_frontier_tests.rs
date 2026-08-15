use std::sync::Arc;
use std::time::Duration;

use crate::dc_ack::Deadline;
use crate::peer::TransportError;
use crate::remote_durability::{MetadataOperation, RemoteAck, assess_remote_metadata_durability};
use crate::remote_frontier::{LoopbackRemoteFrontierSource, RemoteFrontierSource, gather_remote_acks};

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
    let mut acks = Vec::new();
    let deadline = gather_remote_acks(sources, AUTHORITY, operation, &mut acks, BUDGET, POLL).await;
    (deadline, acks)
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

    let deadline = gather_remote_acks(&sources, AUTHORITY, &op(3, 100), &mut acks, BUDGET, POLL).await;

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
    assert!(assess_remote_metadata_durability(&op(3, 100), &acks).is_durable());
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
    assert!(!assess_remote_metadata_durability(&op(3, 100), &acks).is_durable());
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
    assert!(!assess_remote_metadata_durability(&op(3, 100), &acks).is_durable());
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
