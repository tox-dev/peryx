use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};

use bytes::Bytes;
use peryx_storage::blob::Digest;

use crate::blob::{BlobRequest, BlobTransport, ByteRange, LoopbackBlobSource};
use crate::blob_routing::RoutingBlobTransport;
use crate::peer::{TransferLimits, TransportError};

fn limits() -> TransferLimits {
    TransferLimits {
        max_operations: NonZeroUsize::new(256).unwrap(),
        max_encoded_bytes: NonZeroU64::new(1024).unwrap(),
    }
}

fn source(entries: &[(&Digest, &[u8])]) -> LoopbackBlobSource {
    let blobs = entries
        .iter()
        .map(|(digest, bytes)| ((*digest).clone(), Bytes::copy_from_slice(bytes)))
        .collect();
    LoopbackBlobSource::new(blobs, limits())
}

fn whole(digest: &Digest) -> BlobRequest {
    BlobRequest {
        digest: digest.clone(),
        range: None,
    }
}

#[tokio::test]
async fn test_routes_a_digest_to_its_chosen_datacenter() {
    let content = b"eastern blob";
    let digest = Digest::of(content);
    let router = RoutingBlobTransport::new(
        HashMap::from([(digest.clone(), "east".to_owned())]),
        HashMap::from([("east".to_owned(), source(&[(&digest, content)]))]),
    );

    let bytes = router.fetch_blob(whole(&digest)).await.unwrap();

    assert_eq!(bytes, content);
}

#[tokio::test]
async fn test_different_digests_route_to_different_datacenters() {
    let east_content = b"eastern";
    let west_content = b"western";
    let east_digest = Digest::of(east_content);
    let west_digest = Digest::of(west_content);
    let router = RoutingBlobTransport::new(
        HashMap::from([
            (east_digest.clone(), "east".to_owned()),
            (west_digest.clone(), "west".to_owned()),
        ]),
        HashMap::from([
            ("east".to_owned(), source(&[(&east_digest, east_content)])),
            ("west".to_owned(), source(&[(&west_digest, west_content)])),
        ]),
    );

    assert_eq!(router.fetch_blob(whole(&east_digest)).await.unwrap(), east_content);
    assert_eq!(
        router.fetch_blob(whole(&west_digest)).await.unwrap(),
        west_content,
        "the west digest must reach the west delegate, not a single shared transport",
    );
}

#[tokio::test]
async fn test_an_unrouted_digest_fails_closed() {
    let digest = Digest::of(b"orphan");
    let router: RoutingBlobTransport<LoopbackBlobSource> = RoutingBlobTransport::new(HashMap::new(), HashMap::new());

    let error = router.fetch_blob(whole(&digest)).await.unwrap_err();

    assert_eq!(
        error,
        TransportError::BlobNotFound {
            digest: digest.as_str().to_owned(),
        },
    );
    assert!(
        !error.is_retryable(),
        "an unroutable blob is a terminal miss, not a retry"
    );
}

#[tokio::test]
async fn test_a_route_to_a_datacenter_without_a_delegate_fails_closed() {
    let digest = Digest::of(b"ghosted");
    let router: RoutingBlobTransport<LoopbackBlobSource> = RoutingBlobTransport::new(
        HashMap::from([(digest.clone(), "absent-dc".to_owned())]),
        HashMap::new(),
    );

    let error = router.fetch_blob(whole(&digest)).await.unwrap_err();

    assert_eq!(
        error,
        TransportError::BlobNotFound {
            digest: digest.as_str().to_owned(),
        },
    );
}

#[tokio::test]
async fn test_passes_a_range_through_to_the_delegate() {
    let content = b"0123456789";
    let digest = Digest::of(content);
    let router = RoutingBlobTransport::new(
        HashMap::from([(digest.clone(), "east".to_owned())]),
        HashMap::from([("east".to_owned(), source(&[(&digest, content)]))]),
    );

    let bytes = router
        .fetch_blob(BlobRequest {
            digest: digest.clone(),
            range: Some(ByteRange { offset: 2, length: 3 }),
        })
        .await
        .unwrap();

    assert_eq!(
        bytes, b"234",
        "the range reaches the delegate, which returns just those bytes"
    );
}
