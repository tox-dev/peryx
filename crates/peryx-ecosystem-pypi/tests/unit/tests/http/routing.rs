use super::support::*;
use peryx_identity::IndexAcl;

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
    assert!(body.contains("upstream returned an invalid response"), "{body}");
    assert!(!body.contains("/simple/flask/"));
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
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    // Routes "a" and "a/b" both prefix "a/b/simple/"; the longer must win.
    let indexes = vec![
        Index {
            name: "a".to_owned(),
            route: "a".to_owned(),
            policy: Policy::default(),
            acl: IndexAcl::default(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
        },
        Index {
            name: "ab".to_owned(),
            route: "a/b".to_owned(),
            policy: Policy::default(),
            acl: crate::tests::writer_acl("s3cret".to_owned()),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
        },
    ];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));

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
#[rstest]
#[case::index("/pypi/simple", "/pypi/simple/")]
#[case::project("/pypi/simple/flask", "/pypi/simple/flask/")]
#[case::hosted("/hosted/simple/Flask.Test", "/hosted/simple/flask-test/")]
#[case::nested_route("/root/pypi/simple/flask", "/root/pypi/simple/flask/")]
#[case::normalized_with_query("/pypi/simple/Flask.Test?extra=1", "/pypi/simple/flask-test/?extra=1")]
#[tokio::test]
async fn test_simple_url_without_trailing_slash_redirects(#[case] requested: &str, #[case] location: &str) {
    let h = harness().await;
    let (status, headers, _) = get(&h.state, requested, None).await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(headers.get(header::LOCATION).unwrap(), location);
}
#[rstest]
#[case::empty("/pypi/simple//", StatusCode::NOT_FOUND)]
#[case::nested("/pypi/simple/flask/bad/", StatusCode::NOT_FOUND)]
#[case::invalid_name("/pypi/simple/-flask/", StatusCode::NOT_FOUND)]
#[case::invalid_utf8("/pypi/simple/%FF/", StatusCode::BAD_REQUEST)]
#[case::encoded_slash("/pypi/simple/flask%2Fbad/", StatusCode::BAD_REQUEST)]
#[case::encoded_slash_without_trailing_slash("/pypi/simple/flask%2Fbad", StatusCode::BAD_REQUEST)]
#[case::encoded_backslash("/pypi/simple/flask%5Cbad/", StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn test_simple_project_path_rejects_invalid_segments_before_upstream_or_storage(
    #[case] uri: &str,
    #[case] expected_status: StatusCode,
) {
    let h = harness().await;
    let metadata_keys_before = h.state.serving.meta.driver_prefix_keys("").unwrap();

    let (status, ..) = get(&h.state, uri, Some("application/json")).await;

    assert_eq!(status, expected_status, "{uri}");
    assert!(h.server.received_requests().await.unwrap().is_empty(), "{uri}");
    assert_eq!(
        h.state.serving.meta.driver_prefix_keys("").unwrap(),
        metadata_keys_before,
        "{uri}"
    );
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
#[case::missing(None, "application/vnd.pypi.simple.v1+json")]
#[case::wildcard(Some("*/*"), "application/vnd.pypi.simple.v1+json")]
#[case::html_preferred(
    Some("text/html, application/vnd.pypi.simple.v1+json;q=0.001"),
    "text/html; charset=utf-8"
)]
#[case::json_preferred(
    Some("text/html;q=0.001, application/vnd.pypi.simple.v1+json"),
    "application/vnd.pypi.simple.v1+json"
)]
#[case::specific_refusal_overrides_wildcard(
    Some("*/*, application/vnd.pypi.simple.v1+json;q=0"),
    "text/html; charset=utf-8"
)]
#[case::application_refusal_overrides_range(
    Some("application/*;q=0.8, application/vnd.pypi.simple.v1+json;q=0"),
    "text/html; charset=utf-8"
)]
#[case::text_refusal_leaves_json(Some("text/*;q=0, */*"), "application/vnd.pypi.simple.v1+json")]
#[case::text_range_outweighs_wildcard(Some("text/*, */*"), "text/html; charset=utf-8")]
#[case::specific_html_quality_overrides_wildcard(
    Some("application/*, text/html;q=0.5"),
    "application/vnd.pypi.simple.v1+json"
)]
#[case::specificity_breaks_quality_tie(Some("application/*;q=0.8, text/html;q=0.8"), "text/html; charset=utf-8")]
#[case::mixed_case_json(Some("Application/Vnd.Pypi.Simple.V1+Json"), "application/vnd.pypi.simple.v1+json")]
#[case::latest_html_preferred(
    Some("application/vnd.pypi.simple.latest+json;q=0.2, application/vnd.pypi.simple.latest+html;q=0.3"),
    "text/html; charset=utf-8"
)]
#[case::utf8_html_parameter(
    Some("text/html;charset=\"utf-8\";q=0.6, application/json;q=0.5"),
    "text/html; charset=utf-8"
)]
#[case::json_charset_is_not_supported(
    Some("application/json;charset=utf-8, text/html;q=0.5"),
    "text/html; charset=utf-8"
)]
#[case::uppercase_quality(
    Some("application/json;Q=0.9, text/html;q=0.8"),
    "application/vnd.pypi.simple.v1+json"
)]
#[case::maximum_quality(Some("application/json;q=1.000"), "application/vnd.pypi.simple.v1+json")]
#[case::equal_quality_uses_json(
    Some("application/vnd.pypi.simple.v1+json, text/html"),
    "application/vnd.pypi.simple.v1+json"
)]
#[case::duplicate_range_uses_higher_quality(
    Some("application/json;q=0.1, application/json;q=0.8, text/html;q=0.5"),
    "application/vnd.pypi.simple.v1+json"
)]
#[case::duplicate_charset(
    Some("text/html;charset=utf-8;charset=utf-8, application/json;q=0.5"),
    "application/vnd.pypi.simple.v1+json"
)]
#[case::unsupported_charset(
    Some("text/html;charset=iso-8859-1, application/json;q=0.5"),
    "application/vnd.pypi.simple.v1+json"
)]
#[case::parameterized_refusal_is_more_specific(
    Some("text/html, text/html;charset=utf-8;q=0, application/json;q=0.5"),
    "application/vnd.pypi.simple.v1+json"
)]
#[case::empty_media_range(Some(";q=1, application/json;q=0.5"), "application/vnd.pypi.simple.v1+json")]
#[tokio::test]
async fn test_simple_negotiation_selects_supported_representation(
    #[case] accept: Option<&str>,
    #[case] expected_content_type: &str,
) {
    let h = harness().await;

    let (status, headers, _) = get(&h.state, "/pypi/simple/", accept).await;

    assert_eq!(
        (status, headers[header::CONTENT_TYPE].to_str().unwrap()),
        (StatusCode::OK, expected_content_type)
    );
}
#[rstest]
#[case::json_refused("application/vnd.pypi.simple.v1+json;q=0")]
#[case::all_supported_refused("application/vnd.pypi.simple.v1+json;q=0, text/html;q=0")]
#[case::unsupported("image/png")]
#[case::invalid_quality("application/json;q=1.001")]
#[case::overprecise_quality("application/json;q=0.0001")]
#[case::nonnumeric_quality("application/json;q=0.x")]
#[case::missing_quality_value("application/json;q")]
#[case::duplicate_quality("application/json;q=1;q=0.5")]
#[case::unknown_parameter("application/json;version=1")]
#[tokio::test]
async fn test_simple_negotiation_rejects_unacceptable_field(#[case] accept: &str) {
    let h = harness().await;

    let (status, headers, body) = get(&h.state, "/pypi/simple/", Some(accept)).await;

    assert_eq!(
        (status, headers[header::VARY].to_str().unwrap(), body.as_str()),
        (
            StatusCode::NOT_ACCEPTABLE,
            "Accept",
            "no acceptable Simple API representation"
        )
    );
}
#[tokio::test]
async fn test_simple_detail_rejects_unacceptable_field_before_resolution() {
    let h = harness().await;

    let (status, _, _) = get(&h.state, "/pypi/simple/flask/", Some("image/png")).await;

    assert_eq!(status, StatusCode::NOT_ACCEPTABLE);
    assert!(h.server.received_requests().await.unwrap().is_empty());
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
