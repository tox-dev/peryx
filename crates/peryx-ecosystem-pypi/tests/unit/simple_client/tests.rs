use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use futures_util::{Stream, StreamExt as _, stream};
use peryx_upstream::UpstreamError;

use super::{MAX_SIMPLE_PAGE_BYTES, read_capped};

fn body(sizes: Vec<usize>) -> impl Stream<Item = Result<Bytes, reqwest::Error>> {
    stream::iter(sizes.into_iter().map(|size| Ok(Bytes::from(vec![b'a'; size]))))
}

#[tokio::test]
async fn test_read_capped_returns_a_body_within_the_limit() {
    let read = read_capped(body(vec![8, 8]), 64).await.unwrap();

    assert_eq!(read.len(), 16);
}

#[tokio::test]
async fn test_read_capped_stops_at_the_first_chunk_past_the_limit() {
    let polled = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&polled);
    let stream = body(vec![20, 20]).inspect(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });

    let error = read_capped(stream, 8).await.unwrap_err();

    assert!(matches!(error, UpstreamError::ResponseTooLarge { limit: 8 }));
    assert_eq!(polled.load(Ordering::SeqCst), 1);
}

#[test]
fn test_simple_page_cap_matches_the_project_sync_cap() {
    assert_eq!(MAX_SIMPLE_PAGE_BYTES as u64, crate::cache::MAX_PROJECT_BYTES);
}
