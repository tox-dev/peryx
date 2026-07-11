//! Resolving a request path to an index, and what happens when it resolves to nothing.

use super::support::*;

#[tokio::test]
async fn test_unsupported_simple_api_major_version_is_bad_gateway() {
    let h = harness().await;
    let json = r#"{"name":"flask","meta":{"api-version":"2.0"},"versions":[],"files":[]}"#;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(json.as_bytes().to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("project detail on index \"pypi\" for project \"flask\""));
    assert!(body.contains("unsupported upstream Simple API version \"2.0\""));
}
#[tokio::test]
async fn test_unsupported_upstream_content_type_is_bad_gateway() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"not an index".to_vec(), "application/octet-stream"))
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("unsupported upstream Simple API Content-Type"));
    assert!(body.contains("/simple/flask/"));
}
#[tokio::test]
async fn test_unknown_route_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/nope/simple/flask/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_put_without_yank_suffix_is_not_found() {
    let h = harness().await;
    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn test_put_suffix_inside_segment_is_not_an_action() {
    let h = harness().await;
    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/1.0/notyank", Some(&upload_auth())).await,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn test_longest_prefix_wins() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    // Routes "a" and "a/b" both prefix "a/b/simple/"; the longer must win.
    let indexes = vec![
        Index {
            name: "a".to_owned(),
            route: "a".to_owned(),
            policy: Policy::default(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: None,
                volatile: true,
            },
        },
        Index {
            name: "ab".to_owned(),
            route: "a/b".to_owned(),
            policy: Policy::default(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
        },
    ];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    // Uploading requires a token; only "a/b" has one, so a 401-vs-200 proves which matched.
    assert_eq!(upload_peryxpkg(&state, "/a/b/", &fixture_wheel()).await, StatusCode::OK);
}
#[tokio::test]
async fn test_get_unrecognized_subpath_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/pypi/random/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_get_route_without_trailing_slash_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/pypi", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_project_list_html() {
    let h = harness().await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;
    let (status, headers, body) = get(&h.state, "/hosted/simple/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/html; charset=utf-8");
    assert!(body.contains("peryxpkg"));
}
#[rstest]
#[case::html("text/html", "text/html; charset=utf-8")]
#[case::pep691_json("application/json", "application/vnd.pypi.simple.v1+json")]
#[tokio::test]
async fn test_simple_detail_for_project_named_json_is_not_claimed_by_legacy_json(
    #[case] accept: &str,
    #[case] expected_content_type: &str,
) {
    let h = harness().await;
    // PEP 503 reserves `/simple/{project}/` for the detail page, so `/simple/json/` must reach the
    // project `json`, not the legacy-JSON view of a project `simple`. Only `/simple/json/` is mocked;
    // the shadowing bug would fetch `/simple/simple/` and 404.
    let body = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"json\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"json-1.0-py3-none-any.whl\",\"url\":\"{}/files/json.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}}}}]}}",
        h.server.uri(),
        Digest::of(b"json-wheel").as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/json/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;

    let (status, headers, body) = get(&h.state, "/pypi/simple/json/", Some(accept)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], expected_content_type);
    assert!(body.contains("json-1.0-py3-none-any.whl"), "{body}");
}
#[tokio::test]
async fn test_legacy_json_still_serves_a_normal_project() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, Digest::of(b"wheel").as_str(), &file_url, None).await;

    let (status, headers, body) = get(&h.state, "/pypi/flask/json", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let legacy: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(legacy["info"]["name"], "flask");
}
