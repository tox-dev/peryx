use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::{Body, to_bytes};
use bytes::Bytes;
use futures_util::{StreamExt as _, stream};
use peryx_storage::blob::{BlobMetadata, BlobRead, BlobReadBody, Digest};
use rstest::rstest;
use tempfile::NamedTempFile;

use crate::body::{blob_read, on_body_complete, pipelined_file};

fn temp_file(contents: &[u8]) -> NamedTempFile {
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(contents).expect("write temp");
    file
}

fn reopen(file: &NamedTempFile) -> std::fs::File {
    std::fs::File::open(file.path()).expect("open temp")
}

async fn collect(body: Body) -> Vec<u8> {
    to_bytes(body, usize::MAX).await.expect("body collects").to_vec()
}

#[tokio::test]
async fn test_blob_read_stream_preserves_chunks() {
    let read = BlobRead::new(
        "test",
        Digest::of(b"hello"),
        BlobMetadata {
            bytes: 5,
            modified: None,
        },
        0..5,
        BlobReadBody::Stream(Box::pin(stream::iter([
            Ok(Bytes::from_static(b"he")),
            Ok(Bytes::from_static(b"llo")),
        ]))),
    );
    assert_eq!(collect(blob_read(read)).await, b"hello");
}

#[rstest]
#[case::short(6)]
#[case::long(4)]
#[tokio::test]
async fn test_blob_read_stream_rejects_declared_length_mismatch(#[case] declared: u64) {
    let read = BlobRead::new(
        "test",
        Digest::of(b"hello"),
        BlobMetadata {
            bytes: declared,
            modified: None,
        },
        0..declared,
        BlobReadBody::Stream(Box::pin(stream::once(async { Ok(Bytes::from_static(b"hello")) }))),
    );
    assert!(to_bytes(blob_read(read), usize::MAX).await.is_err());
}

#[tokio::test]
async fn test_blob_read_stream_rejects_reversed_range() {
    let read = BlobRead::new(
        "test",
        Digest::of(b"hello"),
        BlobMetadata {
            bytes: 5,
            modified: None,
        },
        std::ops::Range { start: 5, end: 0 },
        BlobReadBody::Stream(Box::pin(stream::empty())),
    );
    assert!(to_bytes(blob_read(read), usize::MAX).await.is_err());
}

#[tokio::test]
async fn test_body_completion_reports_bytes_at_clean_eof() {
    let completed = Arc::new(AtomicU64::new(u64::MAX));
    let recorded = completed.clone();
    let body = on_body_complete(Body::from("hello"), move |bytes| {
        recorded.store(bytes, Ordering::Relaxed);
    });
    assert_eq!(collect(body).await, b"hello");
    assert_eq!(completed.load(Ordering::Relaxed), 5);
}

#[tokio::test]
async fn test_body_completion_ignores_failed_streams() {
    let completed = Arc::new(AtomicU64::new(u64::MAX));
    let recorded = completed.clone();
    let body = Body::from_stream(stream::once(async { Err::<Bytes, _>(std::io::Error::other("failed")) }));
    let body = on_body_complete(body, move |bytes| {
        recorded.store(bytes, Ordering::Relaxed);
    });
    assert!(to_bytes(body, usize::MAX).await.is_err());
    assert_eq!(completed.load(Ordering::Relaxed), u64::MAX);
}

#[tokio::test]
async fn test_blob_read_file_honors_range() {
    let file = temp_file(b"hello peryx");
    let read = BlobRead::new(
        "test",
        Digest::of(b"hello peryx"),
        BlobMetadata {
            bytes: 11,
            modified: None,
        },
        6..11,
        BlobReadBody::File(reopen(&file)),
    );
    assert_eq!(collect(blob_read(read)).await, b"peryx");
}

#[rstest]
#[case::streams_whole_file(b"hello peryx".to_vec(), 0, 11, b"hello peryx".to_vec())]
#[case::serves_offset_range(b"hello peryx world".to_vec(), 6, 5, b"peryx".to_vec())]
#[case::stops_at_eof_past_length(b"abc".to_vec(), 0, 4096, b"abc".to_vec())]
#[case::streams_multiple_chunks(vec![7u8; 3 * 1024 * 1024], 0, 3 * 1024 * 1024, vec![7u8; 3 * 1024 * 1024])]
#[tokio::test]
async fn test_pipelined_file(
    #[case] contents: Vec<u8>,
    #[case] offset: u64,
    #[case] len: u64,
    #[case] expected: Vec<u8>,
) {
    let file = temp_file(&contents);
    assert_eq!(collect(pipelined_file(reopen(&file), offset, len)).await, expected);
}

#[tokio::test]
async fn test_pipelined_file_read_error_poisons_stream() {
    let file = temp_file(b"unreadable");
    let write_only = std::fs::OpenOptions::new()
        .write(true)
        .open(file.path())
        .expect("write-only handle");
    assert!(to_bytes(pipelined_file(write_only, 0, 10), usize::MAX).await.is_err());
}

#[tokio::test]
async fn test_pipelined_file_stops_when_client_drops() {
    let contents = vec![9u8; 8 * 1024 * 1024];
    let file = temp_file(&contents);
    let mut stream = pipelined_file(reopen(&file), 0, contents.len() as u64).into_data_stream();
    let first = stream.next().await.expect("first chunk").expect("chunk is ok");
    assert_eq!(first.len(), 1024 * 1024);
    drop(stream);
    tokio::task::yield_now().await;
}
