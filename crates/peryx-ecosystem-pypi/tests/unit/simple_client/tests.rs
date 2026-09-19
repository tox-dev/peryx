use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use futures_util::{Stream, StreamExt as _, stream};
use peryx_upstream::UpstreamError;

use super::{MAX_SIMPLE_PAGE_BYTES, delta_seconds, read_capped};

fn body(sizes: Vec<usize>) -> impl Stream<Item = Result<Bytes, reqwest::Error>> {
    stream::iter(sizes.into_iter().map(|size| Ok(Bytes::from(vec![b'a'; size]))))
}

#[tokio::test]
async fn test_read_capped_returns_a_body_within_the_limit() {
    let read = read_capped(Box::pin(body(vec![8, 8])), 64).await.unwrap();

    assert_eq!(read.len(), 16);
}

/// The check measures the *remaining* budget (`limit - body.len()`), not the total limit, so a
/// second chunk that alone would fit under the raw limit must still be rejected once an earlier
/// chunk has already spent most of it.
#[tokio::test]
async fn test_read_capped_measures_the_remaining_budget_not_the_total_limit() {
    let error = read_capped(Box::pin(body(vec![4, 10])), 8).await.unwrap_err();

    assert!(matches!(error, UpstreamError::ResponseTooLarge { limit: 8 }));
}

#[tokio::test]
async fn test_read_capped_accepts_a_body_exactly_at_the_limit() {
    let read = read_capped(Box::pin(body(vec![4, 4])), 8).await.unwrap();

    assert_eq!(read.len(), 8);
}

/// `:` immediately follows `9` in ASCII, so `byte - b'0'` computes `10` for it: a digit filter that
/// let `10` through would silently accept it as a delta-seconds digit instead of rejecting the whole
/// value as malformed.
#[test]
fn test_delta_seconds_rejects_the_byte_immediately_past_nine() {
    assert_eq!(delta_seconds("60"), Some(60));
    assert_eq!(delta_seconds("6:"), None);
}

#[tokio::test]
async fn test_read_capped_stops_at_the_first_chunk_past_the_limit() {
    let polled = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&polled);
    let stream = body(vec![20, 20]).inspect(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });

    let error = read_capped(Box::pin(stream), 8).await.unwrap_err();

    assert!(matches!(error, UpstreamError::ResponseTooLarge { limit: 8 }));
    assert_eq!(polled.load(Ordering::SeqCst), 1);
}

#[test]
fn test_simple_page_cap_matches_the_project_sync_cap() {
    assert_eq!(MAX_SIMPLE_PAGE_BYTES as u64, crate::cache::MAX_PROJECT_BYTES);
}
