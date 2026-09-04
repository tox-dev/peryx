use super::support::*;

const FILENAME: &str = "peryxpkg-1.0-py3-none-any.whl";

#[tokio::test]
async fn test_revoked_digest_is_removed_from_each_project_representation() {
    let h = harness().await;
    let revoked = Digest::of(b"revoked wheel");
    let clear = Digest::of(b"clear wheel");
    let file_url = format!("{}/files/pkg.whl", h.server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\
         \"versions\":[\"1.0\",\"2.0\",\"3.0\"],\"files\":[\
         {{\"filename\":\"flask-1.0.whl\",\"size\":11,\"url\":\"{file_url}\",\"hashes\":{{\"sha256\":\"{revoked}\"}}}},\
         {{\"filename\":\"flask-2.0.whl\",\"size\":11,\"url\":\"{file_url}\",\"hashes\":{{\"sha256\":\"{clear}\"}}}},\
         {{\"filename\":\"flask-3.0.whl\",\"size\":11,\"url\":\"{file_url}\",\"hashes\":{{}}}}]}}",
        revoked = revoked.as_str(),
        clear = clear.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(page, "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;
    let (_, _, unfiltered) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(unfiltered.contains(revoked.as_str()));
    revoke_digest(&h.state, &revoked);

    for (uri, accept) in [
        ("/pypi/simple/flask/", "application/json"),
        ("/pypi/simple/flask/", "text/html"),
        ("/pypi/flask/json", "application/json"),
    ] {
        let (status, _, body) = get(&h.state, uri, Some(accept)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(!body.contains(revoked.as_str()), "{uri}: {body}");
        assert!(body.contains(clear.as_str()), "{uri}: {body}");
        assert!(body.contains("flask-3.0.whl"), "{uri}: {body}");
    }

    let access = peryx_driver::access::ReadAccess::from_headers(&h.state.serving, &axum::http::HeaderMap::new());
    let page = peryx_driver::serving::BrowseDriver::browse(
        &crate::PypiServing,
        peryx_driver::serving::BrowseRequest {
            state: h.state.serving.clone(),
            position: 0,
            raw_query: "index=pypi&project=flask".to_owned(),
            access: &access,
            base: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let body = serde_json::to_string(&page).unwrap();
    assert!(!body.contains(revoked.as_str()));
    assert!(body.contains(clear.as_str()));
    assert!(body.contains("flask-3.0.whl"));
}

#[tokio::test]
async fn test_project_remains_discoverable_when_revocation_removes_its_last_file() {
    let h = harness().await;
    let revoked = Digest::of(b"only wheel");
    let file_url = format!("{}/files/pkg.whl", h.server.uri());
    mount_detail(&h.server, revoked.as_str(), &file_url, None).await;
    revoke_digest(&h.state, &revoked);

    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["versions"], serde_json::json!(["1.0"]));
    assert_eq!(detail["files"], serde_json::json!([]));
}

#[tokio::test]
async fn test_revoked_digest_is_not_read_through_any_byte_route() {
    let h = harness().await;
    let artifact = put_local_file(&h.state, FILENAME, &fixture_wheel(), "1.0");
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\n";
    let metadata_digest = h.state.serving.blobs.put_bytes(metadata).await.unwrap();
    h.state
        .serving
        .meta
        .put_metadata(artifact.as_str(), metadata_digest.as_str())
        .unwrap();
    let provenance = br#"{"version":1,"attestation_bundles":[]}"#;
    let provenance_digest = h.state.serving.blobs.put_bytes(provenance).await.unwrap();
    h.state
        .serving
        .meta
        .put_provenance(
            "hosted",
            "peryxpkg",
            artifact.as_str(),
            FILENAME,
            crate::store::ProvenanceSibling {
                provenance_sha256: provenance_digest.as_str(),
                size: provenance.len() as u64,
            },
        )
        .unwrap();
    revoke_digest(&h.state, &artifact);
    let base = format!("/hosted/files/{}/{FILENAME}", artifact.as_str());
    let inspect = format!("/hosted/inspect/{}/{FILENAME}", artifact.as_str());
    let etag = format!("\"{}\"", artifact.as_str());
    let routes = [
        ("GET", base.clone(), Vec::new()),
        ("HEAD", base.clone(), Vec::new()),
        ("GET", base.clone(), vec![("range", "bytes=0-3")]),
        ("GET", base.clone(), vec![("if-none-match", etag.as_str())]),
        ("GET", format!("{base}.metadata"), Vec::new()),
        ("GET", format!("{base}.provenance"), Vec::new()),
        ("GET", inspect.clone(), Vec::new()),
        (
            "GET",
            format!("{inspect}?member=peryxpkg-1.0.dist-info%2FMETADATA"),
            Vec::new(),
        ),
        (
            "GET",
            format!("/root/pypi/files/{}/{FILENAME}", artifact.as_str()),
            Vec::new(),
        ),
        (
            "GET",
            format!("/pypi/files/{}/{FILENAME}", artifact.as_str()),
            Vec::new(),
        ),
    ];

    for (method, uri, headers) in routes {
        let (status, response_headers, body) = send_bytes(&h.state, method, &uri, &headers).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(response_headers[header::CACHE_CONTROL], "no-store", "{method} {uri}");
        assert!(
            !String::from_utf8_lossy(&body).contains("compromised builder"),
            "{method} {uri}"
        );
    }
}

#[tokio::test]
async fn test_active_revocation_does_not_block_a_clear_digest() {
    let h = harness().await;
    let artifact = put_local_file(&h.state, FILENAME, &fixture_wheel(), "1.0");
    revoke_digest(&h.state, &Digest::of(b"other wheel"));
    let uri = format!("/hosted/files/{}/{FILENAME}", artifact.as_str());

    let (status, _, body) = get_bytes(&h.state, &uri, None).await;

    assert_eq!((status, body), (StatusCode::OK, fixture_wheel()));
}

#[tokio::test]
async fn test_lift_restores_yanked_content_without_changing_its_state() {
    let h = authority_harness().await;
    let artifact = put_local_file(&h.state, FILENAME, &fixture_wheel(), "1.0");
    revoke_digest(&h.state, &artifact);
    let uri = format!("/hosted/files/{}/{FILENAME}", artifact.as_str());
    assert_eq!(get(&h.state, &uri, None).await.0, StatusCode::NOT_FOUND);
    let auth = upload_auth();
    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/1.0/yank", Some(&auth)).await,
        StatusCode::OK
    );

    lift_digest(&h.state, &artifact);

    let (status, headers, body) = get_bytes(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], cache_policy("public"));
    assert_eq!(body, fixture_wheel());
    let (_, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("\"yanked\":true"), "{detail}");
}

#[tokio::test]
async fn test_pypi_read_cache_headers_bound_prior_content_and_redirects() {
    let h = harness().await;
    let artifact = put_local_file(&h.state, FILENAME, &fixture_wheel(), "1.0");
    let file = format!("/hosted/files/{}/{FILENAME}", artifact.as_str());
    let (_, public, _) = get(&h.state, &file, None).await;
    assert_eq!(public[header::CACHE_CONTROL], cache_policy("public"));
    let (_, simple, _) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(simple[header::CACHE_CONTROL], cache_policy("public"));
    let (_, private, _) = get_bytes_with_headers(&h.state, &file, &[("authorization", "Basic dW51c2Vk")]).await;
    assert_eq!(private[header::CACHE_CONTROL], cache_policy("private"));

    let (redirect, headers, _) = get(&h.state, "/hosted/simple/peryxpkg", None).await;
    assert_eq!(redirect, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

/// RFC 9111 s4.3.4 replaces a stored response's fields with the ones its `304` carries, so a validated
/// response states the policy of the `200` it refreshes, down to the scope the credential selects. A
/// `no-store` would evict the artifact the client just revalidated, and the `immutable` a download
/// starts from would outlive the revocation bound that download settles on.
#[rstest]
#[case::if_none_match_anonymous(header::IF_NONE_MATCH, header::ETAG, None, "public")]
#[case::if_none_match_authorized(header::IF_NONE_MATCH, header::ETAG, Some("Basic dW51c2Vk"), "private")]
#[case::if_modified_since_anonymous(header::IF_MODIFIED_SINCE, header::LAST_MODIFIED, None, "public")]
#[case::if_modified_since_authorized(
    header::IF_MODIFIED_SINCE,
    header::LAST_MODIFIED,
    Some("Basic dW51c2Vk"),
    "private"
)]
#[tokio::test]
async fn test_validated_artifact_states_the_cache_policy_of_the_download_it_refreshes(
    #[case] condition: header::HeaderName,
    #[case] validator: header::HeaderName,
    #[case] credential: Option<&str>,
    #[case] scope: &str,
) {
    let h = harness().await;
    let artifact = put_local_file(&h.state, FILENAME, &fixture_wheel(), "1.0");
    let uri = format!("/hosted/files/{}/{FILENAME}", artifact.as_str());
    let mut request: Vec<(&str, &str)> = credential.map(|value| ("authorization", value)).into_iter().collect();
    let (status, download, body) = get_bytes_with_headers(&h.state, &uri, &request).await;
    assert_eq!((status, body), (StatusCode::OK, fixture_wheel()));
    assert_eq!(download[header::CACHE_CONTROL], cache_policy(scope));
    request.push((condition.as_str(), download[&validator].to_str().unwrap()));

    let (status, validated, body) = get_bytes_with_headers(&h.state, &uri, &request).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(validated[header::CACHE_CONTROL], cache_policy(scope));
    assert_eq!(validated[header::ETAG], download[header::ETAG]);
    assert_eq!(validated[header::ACCEPT_RANGES], download[header::ACCEPT_RANGES]);
    assert!(body.is_empty());
}

#[tokio::test]
async fn test_revocation_store_failure_denies_artifact_bytes() {
    let (_dir, state, artifact) = state_with_broken_revocation_index();
    let uri = format!("/hosted/files/{}/{FILENAME}", artifact.as_str());

    let (status, headers, body) = get_bytes(&state, &uri, None).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert!(
        !body
            .windows(b"blocked on store fault".len())
            .any(|window| window == b"blocked on store fault")
    );
}

#[rstest]
#[case::simple_html("/hosted/simple/peryxpkg/", "text/html")]
#[case::simple_json("/hosted/simple/peryxpkg/", "application/json")]
#[case::legacy_json("/hosted/peryxpkg/json", "application/json")]
#[tokio::test]
async fn test_revocation_store_failure_denies_project_discovery(#[case] uri: &str, #[case] accept: &str) {
    let (_dir, state, _) = state_with_broken_revocation_index();

    let (status, headers, _) = get(&state, uri, Some(accept)).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn test_revoked_uncached_digest_is_rejected_before_upstream_fetch() {
    let h = harness().await;
    let artifact = Digest::of(b"never fetched");
    h.state
        .serving
        .meta
        .put_file_url(
            "pypi",
            &crate::project_of_filename(FILENAME),
            artifact.as_str(),
            &format!("{}/files/{FILENAME}", h.server.uri()),
            "pypi",
        )
        .unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/files/{FILENAME}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"never fetched"))
        .expect(0)
        .mount(&h.server)
        .await;
    revoke_digest(&h.state, &artifact);
    let uri = format!("/pypi/files/{}/{FILENAME}", artifact.as_str());

    let (status, headers, _) = get_bytes(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

fn cache_policy(scope: &str) -> String {
    format!(
        "{scope}, max-age={}, must-revalidate, no-transform",
        peryx_driver::revocations::DECISION_CACHE_TTL_SECS,
    )
}

fn state_with_broken_revocation_index() -> (tempfile::TempDir, Arc<AppState>, Digest) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("digest_revocation"))
        .unwrap();
    txn.commit().unwrap();
    drop(database);
    let meta = MetaStore::open_existing(path).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let artifact = blobs.blocking().put_bytes(b"blocked on store fault").unwrap();
    let index = Index {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Hosted { volatile: true },
        policy: Policy::default(),
        acl: peryx_identity::IndexAcl::default(),
    };
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, vec![index]));
    (dir, state, artifact)
}
