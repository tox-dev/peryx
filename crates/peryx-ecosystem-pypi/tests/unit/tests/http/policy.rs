use super::support::*;

#[tokio::test]
async fn test_policy_rejects_legacy_json_project() {
    let overlay_policy = policy(|neutral, _pypi| {
        neutral.block_resources = vec!["flask".to_owned()];
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
        neutral.max_artifact_size_bytes = Some(20);
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
async fn test_policy_delays_a_young_upstream_release() {
    let overlay_policy = policy(|_neutral, pypi| {
        pypi.min_release_age_secs = Some(604_800); // seven days
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    h.clock.store(1_768_003_200, Ordering::Relaxed); // 2026-01-10T00:00:00Z
    let aged = Digest::of(b"aged");
    let young = Digest::of(b"young");
    let file_url = h.server.uri();
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\"}},\"name\":\"flask\",\"versions\":[\"1.0\",\"2.0\"],\"files\":[\
         {{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}/files/a.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"size\":10,\"upload-time\":\"2026-01-01T00:00:00Z\"}},\
         {{\"filename\":\"flask-2.0-py3-none-any.whl\",\"url\":\"{file_url}/files/b.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"size\":10,\"upload-time\":\"2026-01-08T00:00:00Z\"}}]}}",
        aged.as_str(),
        young.as_str(),
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
    assert!(body.contains("flask-1.0-py3-none-any.whl"));
    assert!(!body.contains("flask-2.0-py3-none-any.whl"));
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
#[tokio::test]
async fn test_policy_sizes_a_cached_download_from_the_stored_blob() {
    let overlay_policy = policy(|neutral, _pypi| {
        neutral.max_artifact_size_bytes = Some(1024);
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    let wheel = b"wheelcontent";
    let digest = h.state.serving.blobs.put_bytes(wheel).await.unwrap();
    let uri = format!("/root/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());

    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "wheelcontent");
}
#[tokio::test]
async fn test_policy_denies_a_cached_download_over_the_size_limit() {
    let overlay_policy = policy(|neutral, _pypi| {
        neutral.max_artifact_size_bytes = Some(4);
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    let digest = h.state.serving.blobs.put_bytes(b"wheelcontent").await.unwrap();
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
        neutral.block_resources = vec!["flask".to_owned()];
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;

    let (status, _, body) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["action"], "serve");
    assert_eq!(denial["project"], "flask");
    assert_eq!(denial["rule"], "project-block-list");
}
#[rstest]
#[case::hosted(true)]
#[case::virtual_index(false)]
#[tokio::test]
async fn test_policy_rejects_upload_when_index_blocks_project(#[case] hosted: bool) {
    let blocking_policy = policy(|neutral, _pypi| {
        neutral.block_resources = vec!["peryxpkg".to_owned()];
    });
    let (local_policy, virtual_policy) = if hosted {
        (blocking_policy, Policy::default())
    } else {
        (Policy::default(), blocking_policy)
    };
    let h = harness_with_policies(true, true, Policy::default(), local_policy, virtual_policy).await;
    let wheel = fixture_wheel();
    let (content_type, body) = multipart_body(&upload_fields(), Some(("peryxpkg-1.0-py3-none-any.whl", &wheel)));

    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &content_type, body).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["action"], "upload");
    assert_eq!(denial["project"], "peryxpkg");
    assert_eq!(denial["rule"], "project-block-list");
}
#[tokio::test]
async fn test_policy_rejects_upload_over_project_size_quota() {
    let first = fixture_wheel_for("1.0");
    let second = fixture_wheel_for("2.0");
    let limit = (first.len() + second.len() - 1) as u64;
    let quota_policy = policy(move |neutral, _pypi| neutral.max_resource_size_bytes = Some(limit));
    let h = harness_with_policies(true, true, Policy::default(), quota_policy, Policy::default()).await;

    assert_eq!(upload_version(&h.state, "/root/pypi/", "1.0").await, StatusCode::OK);

    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "2.0"),
        ("filetype", "bdist_wheel"),
    ];
    let (ct, body) = multipart_body(&fields, Some(("peryxpkg-2.0-py3-none-any.whl", &second)));
    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["action"], "upload");
    assert_eq!(denial["rule"], "max-project-size");
    let (_, _, detail) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("peryxpkg-1.0-py3-none-any.whl"));
    assert!(!detail.contains("peryxpkg-2.0-py3-none-any.whl"));
}
#[tokio::test]
async fn test_policy_accepts_upload_under_project_size_quota() {
    let first = fixture_wheel_for("1.0");
    let second = fixture_wheel_for("2.0");
    let limit = (first.len() + second.len()) as u64;
    let quota_policy = policy(move |neutral, _pypi| neutral.max_resource_size_bytes = Some(limit));
    let h = harness_with_policies(true, true, Policy::default(), quota_policy, Policy::default()).await;

    assert_eq!(upload_version(&h.state, "/root/pypi/", "1.0").await, StatusCode::OK);
    assert_eq!(upload_version(&h.state, "/root/pypi/", "2.0").await, StatusCode::OK);

    let (_, _, detail) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("peryxpkg-1.0-py3-none-any.whl"));
    assert!(detail.contains("peryxpkg-2.0-py3-none-any.whl"));
}
#[tokio::test]
async fn test_policy_quota_allows_reupload_of_a_file_at_the_limit() {
    let wheel = fixture_wheel_for("1.0");
    let limit = wheel.len() as u64;
    let quota_policy = policy(move |neutral, _pypi| neutral.max_resource_size_bytes = Some(limit));
    let h = harness_with_policies(true, true, Policy::default(), quota_policy, Policy::default()).await;

    assert_eq!(upload_version(&h.state, "/root/pypi/", "1.0").await, StatusCode::OK);
    assert_eq!(upload_version(&h.state, "/root/pypi/", "1.0").await, StatusCode::OK);
    assert_eq!(
        h.state
            .serving
            .meta
            .quota_resource_usage("hosted", "peryxpkg")
            .unwrap()
            .artifact_bytes
            .committed,
        limit
    );
}

#[tokio::test]
async fn test_policy_quota_disabled_upload_does_not_create_accounting() {
    let h = harness_with(true, true).await;

    assert_eq!(upload_version(&h.state, "/root/pypi/", "1.0").await, StatusCode::OK);
    assert_eq!(
        h.state.serving.meta.quota_resource_usage("hosted", "peryxpkg").unwrap(),
        peryx_storage::meta::QuotaResourceUsage::default()
    );
}

#[tokio::test]
async fn test_policy_quota_serializes_parallel_uploads_near_the_limit() {
    let first = fixture_wheel_for("1.0");
    let second = fixture_wheel_for("2.0");
    let limit = first.len().max(second.len()) as u64;
    let quota_policy = policy(move |neutral, _pypi| neutral.max_resource_size_bytes = Some(limit));
    let h = harness_with_policies(true, true, Policy::default(), quota_policy, Policy::default()).await;
    let first_fields = [
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
    ];
    let second_fields = [
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "2.0"),
        ("filetype", "bdist_wheel"),
    ];
    let (first_type, first_body) = multipart_body(&first_fields, Some(("peryxpkg-1.0-py3-none-any.whl", &first)));
    let (second_type, second_body) = multipart_body(&second_fields, Some(("peryxpkg-2.0-py3-none-any.whl", &second)));
    let auth = upload_auth();

    let (first, second) = tokio::join!(
        post_upload_response(&h.state, "/root/pypi/", Some(&auth), &first_type, first_body,),
        post_upload_response(&h.state, "/root/pypi/", Some(&auth), &second_type, second_body,),
    );
    let statuses = [first.0, second.0];

    assert_eq!(statuses.iter().filter(|status| **status == StatusCode::OK).count(), 1);
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::FORBIDDEN)
            .count(),
        1
    );
    assert!(
        h.state
            .serving
            .meta
            .quota_resource_usage("hosted", "peryxpkg")
            .unwrap()
            .artifact_bytes
            .committed
            <= limit
    );
}

#[tokio::test]
async fn test_policy_quota_audit_accepts_the_route_violation() {
    let wheel = fixture_wheel_for("1.0");
    let quota_policy = policy(|neutral, _pypi| {
        neutral.max_resource_size_bytes = Some(0);
        neutral.quota_audit = true;
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), quota_policy).await;

    assert_eq!(upload_version(&h.state, "/root/pypi/", "1.0").await, StatusCode::OK);
    assert_eq!(
        h.state
            .serving
            .meta
            .quota_resource_usage("hosted", "peryxpkg")
            .unwrap()
            .artifact_bytes
            .committed,
        wheel.len() as u64
    );
}

#[tokio::test]
async fn test_policy_quota_uses_the_stricter_enforcing_layer() {
    let local = policy(|neutral, _pypi| neutral.max_resource_size_bytes = Some(u64::MAX));
    let overlay = policy(|neutral, _pypi| {
        neutral.max_resource_size_bytes = Some(0);
        neutral.quota_audit = true;
    });
    let h = harness_with_policies(true, true, Policy::default(), local, overlay).await;

    assert_eq!(
        upload_version(&h.state, "/root/pypi/", "1.0").await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn test_policy_quota_releases_capacity_when_project_status_rejects() {
    let quota_policy = policy(|neutral, _pypi| neutral.max_resource_size_bytes = Some(u64::MAX));
    let h = harness_with_policies(true, true, Policy::default(), quota_policy, Policy::default()).await;
    let digest = Digest::of(b"archived");
    mount_status_detail(
        &h.server,
        "peryxpkg",
        "archived",
        "retired",
        digest.as_str(),
        &format!("{}/files/peryxpkg.whl", h.server.uri()),
    )
    .await;

    assert_eq!(
        upload_version(&h.state, "/root/pypi/", "1.0").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.state
            .serving
            .meta
            .quota_resource_usage("hosted", "peryxpkg")
            .unwrap()
            .artifact_bytes,
        peryx_storage::meta::QuotaValue::default()
    );
}

#[tokio::test]
async fn test_policy_quota_records_admitted_and_rejected_metrics() {
    let first = fixture_wheel_for("1.0");
    let second = fixture_wheel_for("2.0");
    let limit = (first.len() + second.len() - 1) as u64;
    let quota_policy = policy(move |neutral, _pypi| neutral.max_resource_size_bytes = Some(limit));
    let h = harness_with_policies(true, true, Policy::default(), quota_policy, Policy::default()).await;

    assert_eq!(upload_version(&h.state, "/root/pypi/", "1.0").await, StatusCode::OK);
    assert_eq!(
        upload_version(&h.state, "/root/pypi/", "2.0").await,
        StatusCode::FORBIDDEN
    );

    let expected = BTreeMap::from([("quota_admitted", 1), ("quota_rejected", 1)]);
    h.state.serving.metrics.flush().unwrap();
    let counters = h.state.serving.metrics.index_totals();
    assert!(
        counters.get("hosted").is_some_and(|hosted| {
            expected
                .iter()
                .all(|(key, value)| hosted.ecosystem.get(key) == Some(value))
        }),
        "quota metrics settled on an unexpected state: {:?}",
        counters.get("hosted")
    );
}
