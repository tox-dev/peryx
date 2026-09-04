use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt as _;

use crate::client::UpstreamError;
use crate::client::throughput::{THROUGHPUT_FLOOR, THROUGHPUT_GRACE, bounded};

/// One chunk of `bytes`, delivered `after` the previous one.
struct Chunk {
    after: Duration,
    bytes: usize,
}

fn chunk(after: Duration, bytes: usize) -> Chunk {
    Chunk { after, bytes }
}

/// Replays `script` against the paused clock, so a transfer that would take hours resolves in the time
/// it takes to advance a timer, and reports what the bounded stream produced.
async fn replay(script: Vec<Chunk>) -> Vec<Result<usize, UpstreamError>> {
    let body = futures_util::stream::iter(script).then(|chunk| async move {
        tokio::time::sleep(chunk.after).await;
        Ok(Bytes::from(vec![0; chunk.bytes]))
    });
    bounded(Box::pin(body))
        .map(|item| item.map(|chunk| chunk.len()))
        .collect()
        .await
}

fn floor_for(span: Duration) -> usize {
    usize::try_from(THROUGHPUT_FLOOR.get() * span.as_secs()).unwrap()
}

/// A transfer holding the floor is never stopped, however long it runs.
#[tokio::test(start_paused = true)]
async fn test_a_transfer_at_the_floor_runs_to_the_end() {
    let minute = Duration::from_mins(1);
    let script = (0..10).map(|_| chunk(minute, floor_for(minute))).collect();

    let delivered = replay(script).await;

    assert_eq!(
        delivered.iter().filter(|item| item.is_ok()).count(),
        10,
        "a transfer at the floor was stopped"
    );
}

/// The case the read timeout cannot catch: an upstream that keeps sending, so no gap is ever long
/// enough to trip the idle bound, while delivering far too little to be finishing.
#[tokio::test(start_paused = true)]
async fn test_a_trickle_that_never_idles_is_stopped() {
    let gap = THROUGHPUT_GRACE.checked_sub(Duration::from_secs(1)).unwrap();
    let script = (0..10).map(|_| chunk(gap, 1)).collect();

    let delivered = replay(script).await;

    let failures = delivered
        .iter()
        .filter(|item| matches!(item, Err(UpstreamError::BelowThroughputFloor { .. })))
        .count();
    assert_eq!((delivered.len(), failures), (2, 1));
}

/// A transfer slow to start is not stopped for it, since the grace covers a connection that has yet to
/// deliver anything and the read timeout already bounds a gap this long.
#[tokio::test(start_paused = true)]
async fn test_the_grace_covers_a_slow_first_chunk() {
    let script = vec![chunk(THROUGHPUT_GRACE.checked_sub(Duration::from_secs(1)).unwrap(), 1)];

    let delivered = replay(script).await;

    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].is_ok());
}

/// Delivered bytes buy time, so a server that sends a batch and then pauses to find the next one is not
/// punished for the pause. Sampling an instantaneous rate would stop this transfer.
#[tokio::test(start_paused = true)]
async fn test_a_burst_earns_time_for_the_pause_that_follows() {
    let pause = Duration::from_mins(10);
    let script = vec![chunk(Duration::ZERO, floor_for(pause)), chunk(pause, 1)];

    let delivered = replay(script).await;

    assert_eq!(delivered.len(), 2);
    assert!(delivered.iter().all(Result::is_ok));
}

/// The transfer ends on an error after the bytes it already delivered, so a caller collecting the body
/// sees a failure rather than a short body it could mistake for the whole artifact.
#[tokio::test(start_paused = true)]
async fn test_a_stopped_transfer_reports_failure_after_its_partial_bytes() {
    let gap = THROUGHPUT_GRACE.checked_sub(Duration::from_secs(1)).unwrap();
    let script = vec![chunk(gap, 4), chunk(gap, 4), chunk(gap, 4)];

    let delivered = replay(script).await;

    let shape: Vec<Result<usize, String>> = delivered
        .into_iter()
        .map(|item| item.map_err(|error| error.user_message()))
        .collect();
    assert_eq!(
        shape,
        vec![Ok(4), Err("upstream transfer was too slow to finish".to_owned())]
    );
}
