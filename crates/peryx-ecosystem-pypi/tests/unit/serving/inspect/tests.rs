use axum::body::to_bytes;

use super::*;

async fn body_text(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn ready_body(value: u64) -> Response {
    (StatusCode::OK, value.to_string()).into_response()
}

#[tokio::test]
async fn test_inspect_response_shapes_a_ready_value() {
    let task = tokio::task::spawn_blocking(|| 42_u64);
    let response = inspect_response(task, "pkg.whl", None, ready_body).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "42");
}

#[tokio::test]
async fn test_inspect_response_maps_an_archive_panic_to_500() {
    let task = tokio::task::spawn_blocking(|| -> u64 { panic!("archive worker blew up") });
    let response = inspect_response(task, "pkg.whl", None, ready_body).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body_text(response)
            .await
            .contains("archive \"pkg.whl\": inspection failed")
    );
}

#[tokio::test]
async fn test_inspect_response_names_the_member_on_a_panic() {
    let task = tokio::task::spawn_blocking(|| -> u64 { panic!("member worker blew up") });
    let response = inspect_response(task, "pkg.whl", Some("README.md"), ready_body).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body_text(response)
            .await
            .contains("member \"README.md\" in archive \"pkg.whl\": inspection failed")
    );
}
