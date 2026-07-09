use std::io::Write as _;

use axum::body::{Body, to_bytes};
use futures_util::StreamExt as _;
use rstest::rstest;
use tempfile::NamedTempFile;

use crate::body::pipelined_file;

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

#[rstest]
#[case::streams_whole_file(b"hello peryx".to_vec(), 0, 13, b"hello peryx".to_vec())]
#[case::serves_offset_range(b"hello peryx world".to_vec(), 6, 7, b"peryx".to_vec())]
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
