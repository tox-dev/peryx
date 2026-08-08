//! How a result's source is classified across an index's layers.

use super::support::*;

#[tokio::test]
async fn test_search_filters_repository_policy_denials() {
    let overlay_policy = policy(|config| {
        config.block_projects = vec!["flask".to_owned()];
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    put_cached_package(
        &h.state,
        "pypi/flask",
        "pypi",
        "flask",
        &ProjectDetail {
            meta: Meta::default(),
            name: "Flask".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash("flask-1.0-py3-none-any.whl", &"a".repeat(64), None)],
        },
    );

    let (status, _headers, body) = get(
        &h.state,
        "/root/pypi/+search?q=flask&page_size=25",
        Some("application/json"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 0);
}
#[tokio::test]
async fn test_search_classifies_overlay_upstream_without_overrides() {
    let h = harness().await;
    put_cached_package(
        &h.state,
        "pypi/statused",
        "pypi",
        "statused",
        &ProjectDetail {
            meta: meta_status("archived", "read-only"),
            name: "Statused".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash(
                "statused-1.0-py3-none-any.whl",
                Digest::of(b"statused").as_str(),
                None,
            )],
        },
    );

    let (status, _headers, body) = get(
        &h.state,
        "/root/pypi/+search?q=statused&type=cached&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["type"], "cached");
}
#[tokio::test]
async fn test_search_classifies_overlay_without_upload_as_upstream() {
    let (_dir, state) = overlay_state_without_upload();
    put_cached_package(
        &state,
        "pypi/no-upload",
        "pypi",
        "no-upload",
        &ProjectDetail {
            meta: Meta::default(),
            name: "NoUpload".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![file_with_hash(
                "no-upload-1.0-py3-none-any.whl",
                Digest::of(b"no-upload").as_str(),
                None,
            )],
        },
    );

    let (status, _headers, body) = get(
        &state,
        "/root/pypi/+search?q=no-upload&type=cached&page_size=25",
        Some("application/json"),
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["type"], "cached");
}
