use super::*;

#[test]
fn test_log_release_error_accepts_quota_errors() {
    let error = QuotaError::Empty { field: "project" };

    log_release_error(Some(&error), &"reservation");
    log_release_error(None, &"reservation");
}
