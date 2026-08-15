use super::*;

#[test]
fn test_download_error_maps_mismatch_to_client_and_the_rest_to_gateway() {
    let mismatch = DownloadError::Blob(BlobError::digest_mismatch(&Digest::of(b"a"), &Digest::of(b"b")));
    assert_eq!(download_error_response(mismatch).status(), StatusCode::BAD_REQUEST);
    let io = DownloadError::Blob(BlobError::io(std::io::Error::other("disk")));
    assert_eq!(download_error_response(io).status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        download_error_response(DownloadError::Stream("reset".to_owned())).status(),
        StatusCode::BAD_GATEWAY
    );
}

#[test]
fn test_download_blob_error_reports_a_source() {
    use std::error::Error as _;

    assert!(
        DownloadError::Blob(BlobError::io(std::io::Error::other("disk")))
            .source()
            .is_some()
    );
}

#[test]
fn test_download_stream_error_has_no_source() {
    use std::error::Error as _;

    assert!(DownloadError::Stream("reset".to_owned()).source().is_none());
}

#[test]
fn test_blob_error_converts_to_a_transport_error() {
    assert!(matches!(
        ServeError::from(BlobError::not_found(&Digest::of(b"x"))),
        ServeError::Transport(_)
    ));
}

#[tokio::test]
async fn test_join_layer_contents_returns_the_task_response() {
    let task = tokio::task::spawn_blocking(|| error_response(ErrorCode::BlobUnknown, "blob unknown"));
    assert_eq!(join_layer_contents(task).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_join_layer_contents_maps_a_panic_to_a_gateway_error() {
    let task = tokio::task::spawn_blocking(|| -> Response { panic!("layer worker blew up") });
    assert_eq!(join_layer_contents(task).await.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_ingest_blob_reports_a_stream_error() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let storage = Digest::of(b"x");
    let stream = futures_util::stream::iter(vec![Err("boom".to_owned())]);
    let err = ingest_blob(&blobs, &storage, stream).await.unwrap_err();
    assert!(matches!(err, DownloadError::Stream(message) if message == "boom"));
}

#[tokio::test]
async fn test_ingest_blob_reports_a_cleanup_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    let blobs = BlobStorage::filesystem(&root);
    let storage = Digest::of(b"x");
    let stream = futures_util::stream::once(async move {
        let stage = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        std::fs::remove_file(&stage).unwrap();
        std::fs::create_dir(&stage).unwrap();
        Err("boom".to_owned())
    });
    let err = ingest_blob(&blobs, &storage, stream).await.unwrap_err();
    assert!(matches!(err, DownloadError::Blob(_)));
}
