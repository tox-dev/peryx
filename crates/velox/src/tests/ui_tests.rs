use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use crate::config::{Config, IndexConfig, IndexKind};
use crate::server::build_router;

fn ui_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![
            IndexConfig {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                kind: IndexKind::Mirror {
                    upstream: "http://127.0.0.1:9/simple/".to_owned(),
                    username: None,
                    password: None,
                    token: None,
                },
            },
            IndexConfig {
                name: "local".to_owned(),
                route: "local".to_owned(),
                kind: IndexKind::Local {
                    upload_token: Some("s3cret".to_owned()),
                    volatile: true,
                },
            },
            IndexConfig {
                name: "root/pypi".to_owned(),
                route: "root/pypi".to_owned(),
                kind: IndexKind::Overlay {
                    layers: vec!["local".to_owned(), "pypi".to_owned()],
                    upload: Some("local".to_owned()),
                },
            },
        ],
        ..Config::default()
    }
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Upload the frontend fixture wheel through the router, so UI pages have a metadata-rich package.
async fn upload_fixture(router: &axum::Router) {
    let wheel = include_bytes!("../../../../tests/frontend/fixtures/veloxdemo-1.0.0-py3-none-any.whl");
    let boundary = "veloxuitest";
    let mut body = Vec::new();
    for (name, value) in [(":action", "file_upload"), ("name", "veloxdemo"), ("version", "1.0.0")] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; \
             filename=\"veloxdemo-1.0.0-py3-none-any.whl\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wheel);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .uri("/root/pypi/")
        .method("POST")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("__token__:s3cret")),
        )
        .body(Body::from(body))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_ui_dashboard_renders_indexes_and_counters() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("change serial"));
    assert!(body.contains("root/pypi"));
    assert!(body.contains("badge kind-overlay"));
    assert!(body.contains("badge uploads"));
    assert!(body.contains("/pkg/velox_web.js"));
}

#[tokio::test]
async fn test_ui_browse_lists_projects_after_upload() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    upload_fixture(&router).await;
    let (status, body) = get(&router, "/browse?index=local").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("veloxdemo"));
    assert!(body.contains("Filter projects"));
}

#[tokio::test]
async fn test_ui_browse_empty_index_hint() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/browse?index=local").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No projects observed"));
}

#[tokio::test]
async fn test_ui_project_page_renders_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    upload_fixture(&router).await;
    let (status, body) = get(&router, "/browse?index=local&project=veloxdemo").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("A demonstration package for the velox web UI"));
    assert!(body.contains("uv pip install --index-url /local/simple/ veloxdemo"));
    assert!(body.contains("A demo package served by <strong>velox</strong>"));
    assert!(body.contains("requests&gt;=2"));
    assert!(body.contains("Development Status"));
    assert!(body.contains("badge meta-badge"));
    assert!(body.contains("Manage uploads"));
    assert!(body.contains("1.1 kB"));
}

#[tokio::test]
async fn test_ui_project_page_missing_project() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/browse?index=local&project=ghost").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Project not found on this index."));
}

#[tokio::test]
async fn test_ui_archive_listing_and_member() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    upload_fixture(&router).await;
    let (_, detail) = get(&router, "/local/simple/veloxdemo/").await;
    let sha = detail
        .split("files/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();

    let file = format!("{sha}%2Fveloxdemo-1.0.0-py3-none-any.whl");
    let (status, listing) = get(&router, &format!("/browse?index=local&project=veloxdemo&file={file}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.contains("dist-info/METADATA"));
    assert!(listing.contains("__init__.py"));

    let member =
        format!("/browse?index=local&project=veloxdemo&file={file}&member=veloxdemo-1.0.0.dist-info%2FMETADATA");
    let (status, content) = get(&router, &member).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("Metadata-Version: 2.1"));
    assert!(content.contains("back to the archive listing"));
}

#[tokio::test]
async fn test_ui_unknown_route_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let router = build_router(&ui_config(&dir)).unwrap();
    let (status, body) = get(&router, "/nosuchpage").await;
    // The catch-all API dispatcher answers for non-UI paths.
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}
