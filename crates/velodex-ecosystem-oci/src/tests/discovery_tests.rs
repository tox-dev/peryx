//! `/+api` discovery for OCI indexes: the registry endpoint, capabilities, and `docker pull` setup.

use axum::http::{Method, StatusCode};

use super::{send, send_with, virtual_stack};

#[tokio::test]
async fn test_root_discovery_renders_every_oci_index_with_docker_setup() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, "http://127.0.0.1:1/");
    let (status, _, body) = send_with(&app, Method::GET, "/+api", &[("host", "registry.example:5000")]).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let indexes = json["indexes"].as_array().unwrap();
    assert_eq!(indexes.len(), 3);

    let hub = indexes.iter().find(|index| index["route"] == "hub").unwrap();
    assert_eq!(hub["ecosystem"], "oci");
    assert_eq!(hub["urls"]["registry"], "http://registry.example:5000/v2/");
    assert_eq!(hub["capabilities"]["manifest_push"], false);
    let hub_docker = hub["client_configuration"]["docker"].as_str().unwrap();
    assert!(hub_docker.contains("docker pull registry.example:5000/hub/<image>:<tag>"));
    assert!(!hub_docker.contains("docker push"));

    let images = indexes.iter().find(|index| index["route"] == "images").unwrap();
    assert_eq!(images["capabilities"]["manifest_push"], true);
    let images_docker = images["client_configuration"]["docker"].as_str().unwrap();
    assert!(images_docker.contains("docker login registry.example:5000"));
    assert!(images_docker.contains("docker push registry.example:5000/images/<image>:<tag>"));
}

#[tokio::test]
async fn test_per_index_discovery_serves_oci_without_a_pypi_driver() {
    // This harness wires only the OCI driver, so `state.serving` is the unconfigured PyPI stub. The
    // neutral router still serves `/{route}/+api`, proving the discovery route is not tied to PyPI.
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, "http://127.0.0.1:1/");
    let (status, _, body) = send_with(&app, Method::GET, "/reg/+api", &[("host", "registry.example")]).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["index"]["route"], "reg");
    assert_eq!(json["index"]["ecosystem"], "oci");
    let docker = json["index"]["client_configuration"]["docker"].as_str().unwrap();
    assert!(docker.contains("docker pull registry.example/reg/<image>:<tag>"));
}

#[tokio::test]
async fn test_per_index_search_serves_oci_without_a_pypi_driver() {
    // `/{route}/+search` is likewise neutral: it answers here even though only the OCI driver is wired.
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, "http://127.0.0.1:1/");
    let (status, _, body) = send(&app, Method::GET, "/reg/+search?q=app").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["results"].is_array());
}
