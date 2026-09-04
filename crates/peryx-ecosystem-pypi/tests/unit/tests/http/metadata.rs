use super::support::*;

#[tokio::test]
async fn test_metadata_served_verified_and_counted() {
    let h = harness().await;
    let wheel_digest = Digest::of(b"wheel-bytes");
    let metadata = b"Metadata-Version: 2.1\nName: flask\n";
    let meta_digest = Digest::of(metadata);
    let wheel_url = format!("{}/files/flask.whl", h.server.uri());
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0.whl\",\"size\":11,\"url\":\"{}\",\"hashes\":{{\"sha256\":\"{}\"}},\
         \"core-metadata\":{{\"sha256\":\"{}\"}}}}]}}",
        wheel_url,
        wheel_digest.as_str(),
        meta_digest.as_str()
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(metadata.to_vec()))
        .expect(1)
        .mount(&h.server)
        .await;

    let (_, _, detail) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(detail.contains(&format!(
        "\"core-metadata\":{{\"sha256\":\"{}\"}}",
        meta_digest.as_str()
    )));

    let uri = format!("/pypi/files/{}/flask-1.0.whl.metadata", wheel_digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "Metadata-Version: 2.1\nName: flask\n");
    let (status2, _, body2) = get(&h.state, &uri, None).await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body2, body);

    // Metadata counters are folded in by the off-thread aggregator, so drain it through the barrier
    // before reading `/metrics`; a bare read races the aggregator and flakes on slow runners.
    h.state.serving.metrics.flush().unwrap();
    let (_, _, metrics) = get(&h.state, "/metrics", None).await;
    assert!(
        metrics.contains("peryx_metadata_served_total{ecosystem=\"pypi\",role=\"cached\"} 2"),
        "metadata counter never reached 2:\n{metrics}"
    );
}

#[tokio::test]
async fn test_routed_metadata_sidecar_uses_the_advertising_source_credentials() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&first)
        .await;
    let wheel_digest = Digest::of(b"wheel bytes");
    let metadata = b"Metadata-Version: 2.1\nName: flask\n";
    let metadata_digest = Digest::of(metadata);
    let wheel_url = format!("{}/files/flask.whl", second.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0.whl\",\"size\":11,\"url\":\"{wheel_url}\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"core-metadata\":{{\"sha256\":\"{}\"}}}}]}}",
        wheel_digest.as_str(),
        metadata_digest.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(page, "application/vnd.pypi.simple.v1+json"))
        .mount(&second)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl.metadata"))
        .and(match_header("authorization", "Bearer second-token"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(metadata.to_vec()))
        .expect(1)
        .mount(&second)
        .await;
    let primary = UpstreamClient::new(&format!("{}/simple/", first.uri())).unwrap();
    let router = UpstreamRouter::new(vec![
        NamedUpstream::new("first", primary.clone()),
        NamedUpstream::new(
            "second",
            UpstreamClient::with_auth(
                &format!("{}/simple/", second.uri()),
                Auth::Bearer("second-token".to_owned()),
            )
            .unwrap(),
        ),
    ])
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = routed_state(&dir, primary, router);

    get(&state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/files/{}/flask-1.0.whl.metadata", wheel_digest.as_str());
    let (status, _, body) = get(&state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, String::from_utf8_lossy(metadata));
}

#[tokio::test]
async fn test_routed_metadata_ranges_use_the_advertising_source_credentials() {
    let server = MockServer::start().await;
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let wheel_size = wheel.len();
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    let file_url = format!("{}/files/{filename}", server.uri());
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .and(match_header("authorization", "Bearer mirror-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", wheel_size)
                .insert_header("etag", WHEEL_ETAG),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(match_header("authorization", "Bearer mirror-token"))
        .respond_with(range_response(wheel))
        .mount(&server)
        .await;
    let client = UpstreamClient::with_auth(
        &format!("{}/simple/", server.uri()),
        Auth::Bearer("mirror-token".to_owned()),
    )
    .unwrap();
    let router = UpstreamRouter::new(vec![NamedUpstream::new("mirror", client.clone())]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = routed_state(&dir, client, router);
    let record = CachedIndex {
        source: None,
        last_modified: None,
        etag: None,
        last_serial: None,
        fetched_at_unix: 1000,
        content_type: None,
        fresh_secs: None,
        body: Vec::new(),
    };
    state
        .serving
        .meta
        .put_cached_page(crate::store::CachedPageWrite {
            key: "project:pypi/peryxpkg",
            record: &record,
            index: "pypi",
            normalized: "peryxpkg",
            display: "peryxpkg",
            source: "pypi",
            upstream: Some("mirror"),
            project_status: None,
            project_status_reason: None,
            files: &[crate::store::PublishedFileWrite {
                sha256: digest.as_str().to_owned(),
                filename: filename.to_owned(),
                url: file_url,
                size: Some(wheel_size as u64),
                metadata: None,
            }],
            attestations: &[],
        })
        .unwrap();

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, String::from_utf8_lossy(metadata));
}

#[tokio::test]
async fn test_metadata_rejects_sidecar_over_size_limit() {
    let h = harness().await;
    let artifact = Digest::of(b"artifact");
    let metadata = Digest::of(b"metadata");
    let server = oversized_metadata_server();
    crate::tests::register_publication(
        &h.state.serving.meta,
        "pypi",
        "pkg.whl",
        artifact.as_str(),
        Some((&server.url, metadata.as_str())),
    );

    let uri = format!("/pypi/files/{}/pkg.whl.metadata", artifact.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("upstream response exceeds the 16777216-byte limit"));
}
#[tokio::test]
async fn test_buffered_persist_inserts_metadata_before_url_query() {
    let h = harness().await;
    let wheel_digest = Digest::of(b"wheel-bytes");
    let meta_digest = Digest::of(b"meta-bytes");
    // A signed file URL: `.metadata` must land on the path, ahead of the token query, not after it.
    let wheel_url = format!("{}/files/flask.whl?token=abc", h.server.uri());
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0.whl\",\"url\":\"{wheel_url}\",\"size\":10,\
         \"hashes\":{{\"sha256\":\"{}\"}},\"core-metadata\":{{\"sha256\":\"{}\"}}}}]}}",
        wheel_digest.as_str(),
        meta_digest.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;

    let (status, ..) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);

    let publication = h
        .state
        .serving
        .meta
        .get_file_publication("pypi", "flask", wheel_digest.as_str(), "flask-1.0.whl")
        .unwrap();
    assert_eq!(
        publication,
        Some(crate::store::FilePublication::Claimed(crate::store::MetadataClaim {
            url: format!("{}/files/flask.whl.metadata?token=abc", h.server.uri()),
            metadata_sha256: meta_digest.as_str().to_owned(),
            source: "pypi".to_owned(),
            upstream: None,
        }))
    );
}
const RECOVERED_WHEEL: &str = "peryxpkg-1.0-py3-none-any.whl";
const RECOVERED_METADATA: &[u8] = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";

struct MissingSidecar {
    h: Harness,
    artifact: Digest,
    uri: String,
}

/// A cached index that advertised `advertised` as the sidecar digest for a wheel whose `.metadata`
/// sibling answers `404`. `reachable` mounts the wheel for a ranged read; without it the artifact
/// route answers `404` too and recovery has nothing to read. `siblings` bounds how often the route
/// may ask upstream for the vanished sidecar, which is what pins whether peryx cached the outcome.
async fn missing_sidecar(wheel: &[u8], advertised: &Digest, reachable: bool, siblings: u64) -> MissingSidecar {
    let h = harness().await;
    let artifact = Digest::of(wheel);
    let file_url = format!("{}/files/{RECOVERED_WHEEL}", h.server.uri());
    crate::tests::register_publication(
        &h.state.serving.meta,
        "pypi",
        RECOVERED_WHEEL,
        artifact.as_str(),
        Some((&format!("{file_url}.metadata"), advertised.as_str())),
    );
    h.state
        .serving
        .meta
        .put_file_url(
            "pypi",
            &crate::project_of_filename(RECOVERED_WHEEL),
            artifact.as_str(),
            &file_url,
            "pypi",
        )
        .unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/files/{RECOVERED_WHEEL}.metadata")))
        .respond_with(ResponseTemplate::new(404))
        .expect(siblings)
        .mount(&h.server)
        .await;
    if reachable {
        Mock::given(method("HEAD"))
            .and(path(format!("/files/{RECOVERED_WHEEL}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("accept-ranges", "bytes")
                    .insert_header("content-length", wheel.len())
                    .insert_header("etag", WHEEL_ETAG),
            )
            .mount(&h.server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/files/{RECOVERED_WHEEL}")))
            .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
            .respond_with(range_response(wheel.to_vec()))
            .mount(&h.server)
            .await;
    }
    let uri = format!("/pypi/files/{}/{RECOVERED_WHEEL}.metadata", artifact.as_str());
    MissingSidecar { h, artifact, uri }
}

#[tokio::test]
async fn test_a_missing_advertised_sidecar_is_recovered_from_the_wheel() {
    let advertised = Digest::of(RECOVERED_METADATA);
    let fixture = missing_sidecar(&fixture_wheel_with_metadata(RECOVERED_METADATA), &advertised, true, 1).await;

    let (status, _, body) = get(&fixture.h.state, &fixture.uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), RECOVERED_METADATA);
    assert_eq!(
        fixture
            .h
            .state
            .serving
            .meta
            .get_metadata_digest(fixture.artifact.as_str())
            .unwrap(),
        Some(advertised.as_str().to_owned()),
    );
    // The sidecar mock allows one call, so a repeat answered from the blob proves recovery committed
    // the bytes under the digest the page advertises.
    let (repeated, _, repeated_body) = get(&fixture.h.state, &fixture.uri, None).await;
    assert_eq!(repeated, StatusCode::OK);
    assert_eq!(repeated_body.as_bytes(), RECOVERED_METADATA);
}

#[tokio::test]
async fn test_recovered_metadata_that_contradicts_the_advertisement_is_refused() {
    let advertised = Digest::of(b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 9.9\n");
    let fixture = missing_sidecar(&fixture_wheel_with_metadata(RECOVERED_METADATA), &advertised, true, 2).await;

    let (status, _, body) = get(&fixture.h.state, &fixture.uri, None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body.contains("the index advertised a metadata digest the artifact's own metadata does not match"),
        "{body}"
    );
    assert!(fixture.h.state.serving.blobs.head(&advertised).await.unwrap().is_none());
    assert_eq!(
        fixture
            .h
            .state
            .serving
            .meta
            .get_metadata_digest(fixture.artifact.as_str())
            .unwrap(),
        None,
    );
    // Nothing caches the refusal, so the second request reaches upstream again. The sidecar mock's
    // count of two pins that.
    assert_eq!(
        get(&fixture.h.state, &fixture.uri, None).await.0,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn test_a_wheel_carrying_no_metadata_leaves_the_advertised_sidecar_absent() {
    let advertised = Digest::of(RECOVERED_METADATA);
    let fixture = missing_sidecar(&fixture_wheel_without_metadata(), &advertised, true, 1).await;

    let (status, _, body) = get(&fixture.h.state, &fixture.uri, None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.contains("no matching cached file or upstream source was found"),
        "{body}"
    );
    assert!(fixture.h.state.serving.blobs.head(&advertised).await.unwrap().is_none());
    assert_eq!(
        fixture
            .h
            .state
            .serving
            .meta
            .get_metadata_digest(fixture.artifact.as_str())
            .unwrap(),
        None,
    );
    assert_eq!(get(&fixture.h.state, &fixture.uri, None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_an_unrecoverable_sidecar_does_not_suppress_another_index_metadata() {
    let wheel = fixture_wheel_with_metadata(RECOVERED_METADATA);
    let fixture = missing_sidecar(&wheel, &Digest::of(RECOVERED_METADATA), false, 1).await;
    assert_eq!(get(&fixture.h.state, &fixture.uri, None).await.0, StatusCode::NOT_FOUND);

    upload_wheel(&fixture.h.state, RECOVERED_WHEEL, &wheel).await;
    let uri = format!("/hosted/files/{}/{RECOVERED_WHEEL}.metadata", fixture.artifact.as_str());
    let (status, _, body) = get(&fixture.h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), RECOVERED_METADATA);
}

#[tokio::test]
async fn test_a_sidecar_server_error_is_not_recovered_from_the_wheel() {
    let h = harness().await;
    let wheel = fixture_wheel_with_metadata(RECOVERED_METADATA);
    let artifact = Digest::of(&wheel);
    let file_url = format!("{}/files/{RECOVERED_WHEEL}", h.server.uri());
    crate::tests::register_publication(
        &h.state.serving.meta,
        "pypi",
        RECOVERED_WHEEL,
        artifact.as_str(),
        Some((&format!("{file_url}.metadata"), Digest::of(RECOVERED_METADATA).as_str())),
    );
    h.state
        .serving
        .meta
        .put_file_url(
            "pypi",
            &crate::project_of_filename(RECOVERED_WHEEL),
            artifact.as_str(),
            &file_url,
            "pypi",
        )
        .unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/files/{RECOVERED_WHEEL}.metadata")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    Mock::given(path(format!("/files/{RECOVERED_WHEEL}")))
        .respond_with(range_response(wheel))
        .expect(0)
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{RECOVERED_WHEEL}.metadata", artifact.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_metadata_not_found_when_unregistered() {
    let h = harness().await;
    let uri = format!("/pypi/files/{}/x.whl.metadata", "a".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_metadata_backfill_reads_wheel_ranges() {
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    for (label, wheel) in [
        ("classic", fixture_wheel_with_metadata(metadata)),
        (
            "encrypted nonmetadata entry",
            wheel_with_encrypted_nonmetadata(metadata),
        ),
        ("streamed metadata entry", wheel_with_streamed_metadata(metadata)),
    ] {
        let h = harness().await;
        let digest = Digest::of(&wheel);
        let filename = "peryxpkg-1.0-py3-none-any.whl";
        let file_url = format!("{}/files/{filename}", h.server.uri());
        publish_file(&h.state, "pypi", filename, &digest, &file_url);
        Mock::given(method("HEAD"))
            .and(path(format!("/files/{filename}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("accept-ranges", "bytes")
                    .insert_header("content-length", wheel.len())
                    .insert_header("etag", WHEEL_ETAG),
            )
            .mount(&h.server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/files/{filename}")))
            .and(match_header("accept-encoding", "identity"))
            .respond_with(range_response(wheel))
            .mount(&h.server)
            .await;

        let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
        let (status, _, body) = get(&h.state, &uri, None).await;

        assert_eq!(status, StatusCode::OK, "{label}");
        assert_eq!(body.as_bytes(), metadata, "{label}");
        assert_eq!(
            h.state
                .serving
                .meta
                .get_metadata_digest(digest.as_str())
                .unwrap()
                .expect("generated metadata registered"),
            Digest::of(metadata).as_str(),
            "{label}"
        );
    }
}
#[tokio::test]
async fn test_metadata_backfill_upstream_range_error_is_bad_gateway() {
    let h = harness().await;
    let wheel = fixture_wheel_with_metadata(b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n");
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    publish_file(
        &h.state,
        "pypi",
        filename,
        &digest,
        &format!("{}/files/{filename}", h.server.uri()),
    );
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("upstream returned 500 Internal Server Error"));
}
#[tokio::test]
async fn test_metadata_backfill_reads_cached_wheel_blob() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let digest = h.state.serving.blobs.put_bytes(&wheel).await.unwrap();
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    publish_file(
        &h.state,
        "pypi",
        filename,
        &digest,
        &format!("{}/files/{filename}", h.server.uri()),
    );

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
}
#[tokio::test]
async fn test_metadata_backfill_downloads_when_ranges_fail() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    let file_url = format!("{}/files/{filename}", h.server.uri());
    publish_file(&h.state, "pypi", filename, &digest, &file_url);
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(405))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_some());
}
/// Ranged extraction must not stitch a directory read of one generation onto a tail read of
/// another: the mismatch abandons the ranged path for a full verified download.
#[tokio::test]
async fn test_metadata_backfill_downloads_when_the_artifact_changes_between_ranges() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    publish_file(
        &h.state,
        "pypi",
        filename,
        &digest,
        &format!("{}/files/{filename}", h.server.uri()),
    );
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", wheel.len())
                .insert_header("etag", WHEEL_ETAG),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(rotating_range_response(wheel.clone()))
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .with_priority(10)
        .expect(1)
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
}
#[tokio::test]
async fn test_metadata_backfill_downloads_sdist_without_ranges() {
    let h = harness().await;
    let sdist = fixture_sdist();
    let digest = Digest::of(&sdist);
    let filename = "peryxpkg-1.0.tar.gz";
    publish_file(
        &h.state,
        "pypi",
        filename,
        &digest,
        &format!("{}/files/{filename}", h.server.uri()),
    );
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(sdist))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "Metadata-Version: 2.2\nName: peryxpkg\nVersion: 1.0\n");
}
#[tokio::test]
async fn test_metadata_backfill_missing_wheel_metadata_is_not_found() {
    let h = harness().await;
    let wheel = fixture_wheel_without_metadata();
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    publish_file(
        &h.state,
        "pypi",
        filename,
        &digest,
        &format!("{}/files/{filename}", h.server.uri()),
    );
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", wheel.len())
                .insert_header("etag", WHEEL_ETAG),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(wheel))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_zip_is_unsupported() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    publish_file(
        &h.state,
        "pypi",
        filename,
        &digest,
        &format!("{}/files/{filename}", h.server.uri()),
    );
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", "0")
                .insert_header("etag", WHEEL_ETAG),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
}
#[tokio::test]
async fn test_metadata_backfill_does_not_request_a_directory_span_outside_the_artifact() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    let file_path = format!("/files/{filename}");
    let (head_len, directory_len, directory_offset) = (200_usize, 100_u32, 150_u32);
    publish_file(
        &h.state,
        "pypi",
        filename,
        &digest,
        &format!("{}{file_path}", h.server.uri()),
    );
    Mock::given(method("HEAD"))
        .and(path(&file_path))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", head_len)
                .insert_header("etag", WHEEL_ETAG),
        )
        .mount(&h.server)
        .await;
    let directory_end = u64::from(directory_offset) + u64::from(directory_len) - 1;
    Mock::given(method("GET"))
        .and(path(&file_path))
        .and(match_header(
            "range",
            format!("bytes={directory_offset}-{directory_end}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(416))
        .expect(0)
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(&file_path))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(zip_tail_with_directory_span(
            head_len,
            directory_len,
            directory_offset,
        )))
        .with_priority(10)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(&file_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .with_priority(20)
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!((status, body.as_bytes()), (StatusCode::OK, metadata.as_slice()));
    h.server.verify().await;
}
struct Case {
    label: &'static str,
    build_ranged: fn(&[u8], &[u8]) -> Vec<u8>,
}

async fn assert_every_case_falls_back(cases: &[Case]) {
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    for case in cases {
        let h = harness().await;
        let ranged = (case.build_ranged)(metadata, &wheel);

        assert_metadata_range_fallback_preserves_other_resources(&h, case.label, ranged, wheel.clone(), metadata).await;
    }
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_is_unusable() {
    let cases = [
        Case {
            label: "tail is not zip",
            build_ranged: |_metadata, _wheel| vec![0; 128],
        },
        Case {
            label: "directory is empty",
            build_ranged: |_metadata, _wheel| empty_zip(),
        },
        Case {
            label: "directory is invalid",
            build_ranged: |_metadata, wheel| {
                let mut ranged = wheel.to_vec();
                overwrite_metadata_central_signature(&mut ranged, [0, 0, 0, 0]);
                ranged
            },
        },
        Case {
            label: "metadata is too large",
            build_ranged: |metadata, _wheel| {
                wheel_with_metadata_uncompressed_size(
                    metadata,
                    u32::try_from(crate::archive::MAX_WHEEL_METADATA_BYTES).unwrap() + 1,
                )
            },
        },
        Case {
            label: "metadata is encrypted",
            build_ranged: |metadata, _wheel| wheel_with_encrypted_metadata(metadata),
        },
        Case {
            label: "metadata has a ZIP64 compressed size",
            build_ranged: |metadata, _wheel| wheel_with_metadata_central_u32(metadata, 20, u32::MAX),
        },
        Case {
            label: "metadata has a ZIP64 uncompressed size",
            build_ranged: |metadata, _wheel| wheel_with_metadata_central_u32(metadata, 24, u32::MAX),
        },
        Case {
            label: "metadata has a ZIP64 local offset",
            build_ranged: |metadata, _wheel| wheel_with_metadata_central_u32(metadata, 42, u32::MAX),
        },
        Case {
            label: "deflate is invalid",
            build_ranged: |metadata, _wheel| wheel_with_invalid_deflated_metadata(metadata),
        },
        Case {
            label: "deflated output exceeds its declaration",
            build_ranged: |metadata, _wheel| {
                wheel_with_metadata_output_excess(metadata, zip::CompressionMethod::Deflated)
            },
        },
        Case {
            label: "stored output exceeds its declaration",
            build_ranged: |metadata, _wheel| {
                wheel_with_metadata_output_excess(metadata, zip::CompressionMethod::Stored)
            },
        },
        Case {
            label: "compression is unsupported",
            build_ranged: |metadata, _wheel| wheel_with_metadata_compression_method(metadata, 99),
        },
        Case {
            label: "size mismatches",
            build_ranged: |metadata, _wheel| {
                wheel_with_metadata_uncompressed_size(metadata, u32::try_from(metadata.len()).unwrap() + 1)
            },
        },
        Case {
            label: "local header is invalid",
            build_ranged: |_metadata, wheel| {
                let mut ranged = wheel.to_vec();
                overwrite_metadata_local_signature(&mut ranged, [0, 0, 0, 0]);
                ranged
            },
        },
    ];

    assert_every_case_falls_back(&cases).await;
}
#[tokio::test]
async fn test_metadata_backfill_downloads_when_the_zip_records_disagree() {
    let cases = [
        Case {
            label: "local header names another member",
            build_ranged: |metadata, _wheel| wheel_with_metadata_local_name(metadata),
        },
        Case {
            label: "local header declares another compression method",
            build_ranged: |metadata, _wheel| wheel_with_metadata_local_u16(metadata, 8, 0),
        },
        Case {
            label: "local header declares another name length",
            build_ranged: |metadata, _wheel| wheel_with_metadata_local_u16(metadata, 26, 3),
        },
        Case {
            label: "local header declares flags the directory does not",
            build_ranged: |metadata, _wheel| wheel_with_metadata_local_u16(metadata, 6, 1 << 3),
        },
        Case {
            label: "local header declares another CRC-32",
            build_ranged: |metadata, _wheel| wheel_with_metadata_local_u32(metadata, 14, 1),
        },
        Case {
            label: "local header declares another compressed size",
            build_ranged: |metadata, _wheel| wheel_with_metadata_local_u32(metadata, 18, 1),
        },
        Case {
            label: "local header declares another uncompressed size",
            build_ranged: |metadata, _wheel| wheel_with_metadata_local_u32(metadata, 22, 1),
        },
        Case {
            label: "stored member declares unequal sizes",
            build_ranged: |metadata, _wheel| {
                wheel_with_stored_metadata_uncompressed_size(metadata, u32::try_from(metadata.len()).unwrap() - 1)
            },
        },
        Case {
            label: "compressed span holds more than its stream",
            build_ranged: |metadata, _wheel| wheel_with_metadata_compressed_span(metadata, 8),
        },
        Case {
            label: "compressed span cuts its stream short",
            build_ranged: |metadata, _wheel| wheel_with_metadata_compressed_span(metadata, -2),
        },
        Case {
            label: "stored metadata bytes are corrupt",
            build_ranged: |metadata, _wheel| wheel_with_corrupt_stored_metadata(metadata),
        },
    ];

    assert_every_case_falls_back(&cases).await;
}
#[tokio::test]
async fn test_metadata_backfill_scopes_ignored_ranges_to_one_artifact() {
    let h = harness().await;
    let first = fixture_wheel_with_metadata(b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n");
    let first_digest = Digest::of(&first);
    let first_filename = "peryxpkg-1.0-py3-none-any.whl";
    publish_file(
        &h.state,
        "pypi",
        first_filename,
        &first_digest,
        &format!("{}/files/{first_filename}", h.server.uri()),
    );
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{first_filename}")))
        .respond_with(ResponseTemplate::new(405))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{first_filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(first))
        .mount(&h.server)
        .await;

    let first_uri = format!("/pypi/files/{}/{first_filename}.metadata", first_digest.as_str());
    assert_eq!(get(&h.state, &first_uri, None).await.0, StatusCode::OK);

    let second_metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 2.0\n";
    let second = fixture_wheel_with_body_and_metadata("2.0", b"VALUE = 2\n", Some(second_metadata));
    let second_digest = Digest::of(&second);
    let second_filename = "peryxpkg-2.0-py3-none-any.whl";
    publish_file(
        &h.state,
        "pypi",
        second_filename,
        &second_digest,
        &format!("{}/files/{second_filename}", h.server.uri()),
    );
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{second_filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-length", second.len())
                .insert_header("etag", WHEEL_ETAG),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{second_filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(second))
        .mount(&h.server)
        .await;

    let second_uri = format!("/pypi/files/{}/{second_filename}.metadata", second_digest.as_str());
    let (status, _, body) = get(&h.state, &second_uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), second_metadata);
}
#[tokio::test]
async fn test_metadata_backfill_reads_empty_stored_range_metadata() {
    let h = harness().await;
    let wheel = fixture_wheel_with_metadata_compression(b"", zip::CompressionMethod::Stored);
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    publish_file(
        &h.state,
        "pypi",
        filename,
        &digest,
        &format!("{}/files/{filename}", h.server.uri()),
    );
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", wheel.len())
                .insert_header("etag", WHEEL_ETAG),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(wheel))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}
#[tokio::test]
async fn test_metadata_digest_mismatch_is_server_error() {
    let h = harness().await;
    let artifact = Digest::of(b"artifact");
    let metadata = Digest::of(b"expected");
    let metadata_url = format!("{}/files/pkg.whl.metadata", h.server.uri());
    crate::tests::register_publication(
        &h.state.serving.meta,
        "pypi",
        "pkg.whl",
        artifact.as_str(),
        Some((&metadata_url, metadata.as_str())),
    );
    Mock::given(method("GET"))
        .and(path("/files/pkg.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wrong".to_vec()))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/pkg.whl.metadata", artifact.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("metadata fetch on index \"pypi\" for file \"pkg.whl.metadata\""));
    assert!(
        body.contains(&format!(
            "blob store error: filesystem blob backend commit for {}: digest mismatch",
            metadata.as_str()
        )),
        "{body}"
    );
}

struct MetadataServer {
    url: String,
    address: std::net::SocketAddr,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MetadataServer {
    fn drop(&mut self) {
        let _ = std::net::TcpStream::connect(self.address);
        let joined = self.handle.take().unwrap().join();
        if !std::thread::panicking() {
            joined.expect("metadata fixture panicked");
        }
    }
}

fn oversized_metadata_server() -> MetadataServer {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut socket = listener.accept().unwrap().0;
        let mut buffer = [0; 1024];
        let _ = socket.read(&mut buffer);
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            crate::archive::MAX_WHEEL_METADATA_BYTES + 1
        )
        .unwrap();
    });
    MetadataServer {
        url: format!("http://{addr}/pkg.whl.metadata"),
        address: addr,
        handle: Some(handle),
    }
}
