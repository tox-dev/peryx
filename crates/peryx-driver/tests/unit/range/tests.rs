use axum::http::{StatusCode, header};

use super::unsatisfiable_range;

#[test]
fn test_unsatisfiable_range_names_current_size() {
    let response = unsatisfiable_range(41);

    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */41");
}
