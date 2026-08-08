use super::{archive_listing_task_error, archive_member_task_error, member_text};

async fn cancelled_task() -> tokio::task::JoinError {
    let task = tokio::spawn(std::future::pending::<()>());
    task.abort();
    task.await.unwrap_err()
}

#[tokio::test]
async fn task_errors_name_the_failed_operation() {
    assert!(archive_listing_task_error(cancelled_task().await).starts_with("archive listing task failed:"));
    assert!(archive_member_task_error(cancelled_task().await).starts_with("archive member task failed:"));
}

#[test]
fn member_text_rejects_non_utf8_bytes() {
    assert!(member_text("METADATA", vec![0xff]).unwrap_err().contains("METADATA"));
}
