use super::*;

#[test]
fn test_serve_error_maps_every_fault_to_a_gateway_error() {
    let decode = serde_json::from_str::<u8>("nope").unwrap_err();
    assert_eq!(
        ServeError::from(MetaError::Decode(decode)).into_response().status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        ServeError::from(std::io::Error::other("disk")).into_response().status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        ServeError::Transport("reset".to_owned()).into_response().status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        ServeError::Fenced.into_response().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn test_serve_error_message_describes_every_fault() {
    let decode = serde_json::from_str::<u8>("nope").unwrap_err();
    assert!(
        ServeError::from(MetaError::Decode(decode))
            .message()
            .contains("metadata store error")
    );
    assert!(
        ServeError::Io(std::io::Error::other("disk"))
            .message()
            .contains("blob io error")
    );
    assert!(
        ServeError::Transport("reset".to_owned())
            .message()
            .contains("upstream transfer failed")
    );
    assert_eq!(ServeError::Fenced.message(), "repository authority moved");
}

#[tokio::test]
async fn test_serve_error_wraps_a_transport_failure() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let err = reqwest::Client::new()
        .get("http://127.0.0.1:1/")
        .send()
        .await
        .unwrap_err();
    assert_eq!(ServeError::from(err).into_response().status(), StatusCode::BAD_GATEWAY);
}

#[test]
fn test_classify_route_buckets_blob_pulls_as_artifacts() {
    use peryx_driver::rate_limit::RouteClass;
    use peryx_driver::serving::AbsoluteProtocolDriver as _;
    let registry = OciRegistry::default();
    let digest = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    assert_eq!(
        registry.classify_route(&format!("/v2/store/app/blobs/{digest}")),
        RouteClass::Artifact
    );
    assert_eq!(
        registry.classify_route("/v2/store/app/manifests/1.0"),
        RouteClass::Listing
    );
    assert_eq!(registry.classify_route("/v2/store/app/tags/list"), RouteClass::Listing);
    assert_eq!(
        registry.classify_route(&format!("/v2/store/app/blobs/{digest}/contents")),
        RouteClass::Listing
    );
    assert_eq!(registry.classify_route("/v2/token"), RouteClass::Authentication);
    assert_eq!(registry.classify_route("/v2/token/"), RouteClass::Authentication);
}

#[test]
fn test_serve_error_converts_to_its_message_string() {
    assert_eq!(
        String::from(ServeError::Io(std::io::Error::other("disk"))),
        "blob io error: disk"
    );
    assert_eq!(
        String::from(ServeError::Transport("reset".to_owned())),
        "upstream transfer failed: reset"
    );
}

#[tokio::test]
async fn test_read_body_returns_bytes_within_the_cap_and_rejects_an_over_cap_body() {
    assert_eq!(
        read_body(Body::from(b"hello".to_vec()), 1 << 20).await.unwrap(),
        "hello"
    );
    assert!(read_body(Body::from(vec![0u8; 2 << 20]), 1 << 20).await.is_err());
}

#[tokio::test]
async fn test_layer_error_message_reports_an_unreadable_error_body() {
    let response = (StatusCode::BAD_GATEWAY, Body::from(vec![0u8; 2 << 20])).into_response();
    let message = layer_error_message("store/app", "sha256:x", response).await;
    assert!(message.contains("502"), "{message}");
}
