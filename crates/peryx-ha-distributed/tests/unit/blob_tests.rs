use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use peryx_storage::blob::Digest;
use tokio::sync::Notify;

use crate::blob::{BlobRequest, BlobTransport, ByteRange, CapacityLimited, LoopbackBlobSource, collect_capped};
use crate::peer::{TransferLimits, TransportError};

fn limits(max_bytes: u64) -> TransferLimits {
    TransferLimits {
        max_operations: NonZeroUsize::new(256).unwrap(),
        max_encoded_bytes: NonZeroU64::new(max_bytes).unwrap(),
    }
}

fn source(digest: &Digest, bytes: &'static [u8], max_bytes: u64) -> LoopbackBlobSource {
    LoopbackBlobSource::new(
        HashMap::from([(digest.clone(), Bytes::from_static(bytes))]),
        limits(max_bytes),
    )
}

fn whole(digest: &Digest) -> BlobRequest {
    BlobRequest {
        digest: digest.clone(),
        range: None,
    }
}

#[tokio::test]
async fn test_collect_capped_returns_the_bounded_body() {
    let stream = futures_util::stream::iter([Bytes::from_static(b"ab"), Bytes::from_static(b"cd")]);

    let body = collect_capped(stream, 16).await.unwrap();

    assert_eq!(body, b"abcd");
}

#[tokio::test]
async fn test_collect_capped_rejects_an_unbounded_body_without_reading_it_all() {
    let stream = futures_util::stream::repeat_with(|| Bytes::from_static(&[b'x'; 8]));

    let error = collect_capped(stream, 16).await.unwrap_err();

    assert!(matches!(
        error,
        TransportError::FrameTooLarge { limit: 16, actual } if actual > 16
    ));
}

#[tokio::test]
async fn test_fetch_returns_and_verifies_a_whole_blob() {
    let digest = Digest::of(b"hello world");
    let source = source(&digest, b"hello world", 1024);

    let bytes = source.fetch_blob(whole(&digest)).await.unwrap();

    assert_eq!(bytes, b"hello world");
}

#[tokio::test]
async fn test_fetch_returns_an_unverified_range() {
    let digest = Digest::of(b"hello world");
    let source = source(&digest, b"hello world", 1024);

    let bytes = source
        .fetch_blob(BlobRequest {
            digest: digest.clone(),
            range: Some(ByteRange { offset: 6, length: 5 }),
        })
        .await
        .unwrap();

    assert_eq!(bytes, b"world");
}

#[tokio::test]
async fn test_fetch_rejects_a_blob_over_the_cap() {
    let digest = Digest::of(b"0123456789");
    let source = source(&digest, b"0123456789", 4);

    let error = source.fetch_blob(whole(&digest)).await.unwrap_err();

    assert_eq!(error, TransportError::FrameTooLarge { limit: 4, actual: 10 });
}

#[tokio::test]
async fn test_fetch_rejects_content_that_does_not_hash_to_its_digest() {
    let claimed = Digest::of(b"the real blob");
    let source = source(&claimed, b"substituted!", 1024);

    let error = source.fetch_blob(whole(&claimed)).await.unwrap_err();

    assert_eq!(
        error,
        TransportError::DigestMismatch {
            expected: claimed.as_str().to_owned(),
            actual: Digest::of(b"substituted!").as_str().to_owned(),
        }
    );
}

#[tokio::test]
async fn test_fetch_reports_a_missing_blob() {
    let digest = Digest::of(b"absent");
    let source = LoopbackBlobSource::new(HashMap::new(), limits(1024));

    let error = source.fetch_blob(whole(&digest)).await.unwrap_err();

    assert_eq!(
        error,
        TransportError::BlobNotFound {
            digest: digest.as_str().to_owned()
        }
    );
}

#[tokio::test]
async fn test_capacity_limited_delegates_when_a_permit_is_free() {
    let digest = Digest::of(b"data");
    let limited = CapacityLimited::new(source(&digest, b"data", 1024), NonZeroUsize::new(2).unwrap());

    let bytes = limited.fetch_blob(whole(&digest)).await.unwrap();

    assert_eq!(bytes, b"data");
}

struct BlockingBlob {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl BlobTransport for BlockingBlob {
    async fn fetch_blob(&self, _request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn test_capacity_limited_fails_closed_at_the_limit() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let inner = BlockingBlob {
        started: started.clone(),
        release: release.clone(),
    };
    let limited = Arc::new(CapacityLimited::new(inner, NonZeroUsize::new(1).unwrap()));

    let holder = tokio::spawn({
        let limited = Arc::clone(&limited);
        async move { limited.fetch_blob(whole(&Digest::of(b"x"))).await }
    });
    started.notified().await;

    let error = limited.fetch_blob(whole(&Digest::of(b"y"))).await.unwrap_err();

    assert_eq!(error, TransportError::AtCapacity);
    release.notify_one();
    holder.await.unwrap().unwrap();
}
