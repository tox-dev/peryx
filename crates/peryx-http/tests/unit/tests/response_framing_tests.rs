use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use http_body::Body as _;

use crate::response_framing::frame_not_modified;

#[test]
fn frame_not_modified_replaces_the_body_with_one_that_reports_itself_ended() {
    let mut response: Response = StatusCode::NOT_MODIFIED.into_response();

    frame_not_modified(&mut response);

    assert!(response.body().is_end_stream());
}
