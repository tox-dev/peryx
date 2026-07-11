//! Index policy: what it filters from a page, a download, and an upload.

use super::support::*;

#[tokio::test]
async fn test_policy_rejects_legacy_json_project() {
    let overlay_policy = policy(|neutral, _pypi| {
        neutral.block_projects = vec!["flask".to_owned()];
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;

    let (status, _, body) = get(&h.state, "/root/pypi/flask/json", None).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["action"], "serve");
    assert_eq!(denial["project"], "flask");
    assert_eq!(denial["rule"], "project-block-list");
}
#[tokio::test]
async fn test_policy_filters_upstream_simple_files() {
    let overlay_policy = policy(|_neutral, pypi| {
        pypi.allow_versions = Some("==1.0".to_owned());
        pypi.allow_package_types = vec![PackageType::Wheel];
        pypi.allow_wheel_platforms = vec!["any".to_owned()];
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    let allowed = Digest::of(b"allowed");
    let blocked_version = Digest::of(b"blocked-version");
    let blocked_sdist = Digest::of(b"blocked-sdist");
    let blocked_platform = Digest::of(b"blocked-platform");
    let file_url = h.server.uri();
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\"}},\"name\":\"flask\",\"versions\":[\"1.0\",\"2.0\"],\"files\":[\
         {{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}/files/a.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"size\":10}},\
         {{\"filename\":\"flask-2.0-py3-none-any.whl\",\"url\":\"{file_url}/files/b.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"size\":10}},\
         {{\"filename\":\"flask-1.0.tar.gz\",\"url\":\"{file_url}/files/c.tar.gz\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"size\":10}},\
         {{\"filename\":\"flask-1.0-py3-none-manylinux_2_28_x86_64.whl\",\"url\":\"{file_url}/files/d.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"size\":10}}]}}",
        allowed.as_str(),
        blocked_version.as_str(),
        blocked_sdist.as_str(),
        blocked_platform.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;

    let detail: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["versions"], serde_json::json!(["1.0"]));
    assert_eq!(detail["files"].as_array().unwrap().len(), 1);
    assert!(body.contains("flask-1.0-py3-none-any.whl"));
    assert!(!body.contains("flask-2.0-py3-none-any.whl"));
    assert!(!body.contains("flask-1.0.tar.gz"));
    assert!(!body.contains("manylinux_2_28_x86_64"));
}
#[tokio::test]
async fn test_policy_filters_files_without_declared_size() {
    let overlay_policy = policy(|neutral, _pypi| {
        neutral.max_file_size_bytes = Some(20);
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    let small = Digest::of(b"small");
    let missing_size = Digest::of(b"missing-size");
    let file_url = h.server.uri();
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\"files\":[\
         {{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}/files/small.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"size\":10}},\
         {{\"filename\":\"flask-1.0.tar.gz\",\"url\":\"{file_url}/files/missing.tar.gz\",\
         \"hashes\":{{\"sha256\":\"{}\"}}}}]}}",
        small.as_str(),
        missing_size.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/root/pypi/simple/flask/", Some("text/html")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("flask-1.0-py3-none-any.whl"));
    assert!(!body.contains("flask-1.0.tar.gz"));
}
#[tokio::test]
async fn test_policy_rejects_direct_download() {
    let overlay_policy = policy(|_neutral, pypi| {
        pypi.block_wheel_pythons = vec!["py3".to_owned()];
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    let digest = Digest::of(b"wheel");
    let uri = format!("/root/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());

    let (status, _, body) = get(&h.state, &uri, Some("application/json")).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["action"], "serve");
    assert_eq!(denial["project"], "flask");
    assert_eq!(denial["rule"], "wheel-python-block-list");
}
/// A size limit reads a cached artifact's size from the stored blob. An unknown size is a denial, so
/// serving the bytes proves the stat ran: the zero-config path skips it, and only an active policy
/// reaches for it.
#[tokio::test]
async fn test_policy_sizes_a_cached_download_from_the_stored_blob() {
    let overlay_policy = policy(|neutral, _pypi| {
        neutral.max_file_size_bytes = Some(1024);
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    let wheel = b"wheelcontent";
    let digest = h.state.blobs.write(wheel).unwrap();
    let uri = format!("/root/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());

    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "wheelcontent");
}
/// The same stat drives a denial: the limit is below the stored blob's real length.
#[tokio::test]
async fn test_policy_denies_a_cached_download_over_the_size_limit() {
    let overlay_policy = policy(|neutral, _pypi| {
        neutral.max_file_size_bytes = Some(4);
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    let digest = h.state.blobs.write(b"wheelcontent").unwrap();
    let uri = format!("/root/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());

    let (status, _, body) = get(&h.state, &uri, Some("application/json")).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["rule"], "max-file-size");
    assert_eq!(denial["reason"], "file size 12 exceeds limit 4");
}
#[tokio::test]
async fn test_policy_rejects_project_detail() {
    let overlay_policy = policy(|neutral, _pypi| {
        neutral.block_projects = vec!["flask".to_owned()];
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;

    let (status, _, body) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["action"], "serve");
    assert_eq!(denial["project"], "flask");
    assert_eq!(denial["rule"], "project-block-list");
}
#[tokio::test]
async fn test_policy_rejects_upload_when_target_local_index_denies() {
    let local_policy = policy(|neutral, _pypi| {
        neutral.max_file_size_bytes = Some(1);
    });
    let h = harness_with_policies(true, true, Policy::default(), local_policy, Policy::default()).await;
    let wheel = fixture_wheel();
    let (content_type, body) = multipart_body(&upload_fields(), Some(("peryxpkg-1.0-py3-none-any.whl", &wheel)));

    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &content_type, body).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["action"], "upload");
    assert_eq!(denial["project"], "peryxpkg");
    assert_eq!(denial["rule"], "max-file-size");
}
