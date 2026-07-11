use wiremock::{MockServer, ResponseTemplate};

use super::{mount_get, simple_client};
use crate::simple_client::SimpleClientExt as _;

#[tokio::test]
async fn test_fetch_index_reports_decode_errors() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/simple/",
        ResponseTemplate::new(200)
            .insert_header("content-encoding", "gzip")
            .set_body_raw(b"not gzip".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;
    let client = simple_client(&server);

    let err = client.fetch_index().await.unwrap_err();

    assert_eq!(err.user_message(), "upstream response could not be decoded");
}

#[tokio::test]
async fn test_fetch_project_reports_decode_errors() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/simple/flask/",
        ResponseTemplate::new(200)
            .insert_header("content-encoding", "gzip")
            .set_body_raw(b"not gzip".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;
    let client = simple_client(&server);

    let err = client.fetch_project("flask", None).await.unwrap_err();

    assert_eq!(err.user_message(), "upstream response could not be decoded");
}
