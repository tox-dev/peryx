use super::filesystem_worker;
use crate::blob::{BlobErrorKind, BlobOperation};

#[tokio::test]
async fn filesystem_worker_reports_a_panicked_task() {
    let error = filesystem_worker::<()>(
        tokio::task::spawn_blocking(|| panic!("worker failed")),
        BlobOperation::Commit,
        None,
    )
    .await
    .unwrap_err();

    assert_eq!(error.kind(), BlobErrorKind::Io);
    assert_eq!(error.context().unwrap().backend, "filesystem");
}
