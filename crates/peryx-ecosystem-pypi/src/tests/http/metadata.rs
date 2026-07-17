//! The PEP 658 `.metadata` sibling: ranged reads, sidecars, and background backfill.

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
         \"files\":[{{\"filename\":\"flask-1.0.whl\",\"url\":\"{}\",\"hashes\":{{\"sha256\":\"{}\"}},\
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
    let (status2, _, body2) = get(&h.state, &uri, None).await; // cached
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body2, body);

    // Metadata counters are folded in by the off-thread aggregator, so poll until both siblings land
    // before reading `/metrics`; a bare read races the aggregator and flakes on slow runners.
    for _ in 0..500 {
        if h.state
            .metrics
            .index_totals()
            .get("pypi")
            .and_then(|totals| totals.ecosystem.get("metadata").copied())
            == Some(2)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
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
         \"files\":[{{\"filename\":\"flask-1.0.whl\",\"url\":\"{wheel_url}\",\
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
                .insert_header("content-length", wheel_size),
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
        etag: None,
        last_serial: None,
        fetched_at_unix: 1000,
        content_type: None,
        fresh_secs: None,
        body: Vec::new(),
    };
    state
        .meta
        .put_cached_page(
            "project:pypi/peryxpkg",
            &record,
            "pypi",
            "peryxpkg",
            "peryxpkg",
            "pypi",
            Some("mirror"),
            None,
            None,
            &[(digest.as_str().to_owned(), file_url, Some(wheel_size as u64))],
            &[],
        )
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
    h.state
        .meta
        .put_metadata(
            artifact.as_str(),
            &oversized_metadata_server(),
            metadata.as_str(),
            "pypi",
        )
        .unwrap();

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

    // An HTML request resolves through the buffered persist path, not the streaming transformer.
    let (status, ..) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);

    let (url, ..) = h
        .state
        .meta
        .get_metadata(wheel_digest.as_str())
        .unwrap()
        .expect("metadata sibling registered");
    assert_eq!(url, format!("{}/files/flask.whl.metadata?token=abc", h.server.uri()));
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
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    let file_url = format!("{}/files/{filename}", h.server.uri());
    h.state.meta.put_file_url(digest.as_str(), &file_url, "pypi").unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", wheel.len()),
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

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
    let (url, metadata_sha256, source) = h
        .state
        .meta
        .get_metadata(digest.as_str())
        .unwrap()
        .expect("generated metadata registered");
    assert_eq!(url, "peryx:generated");
    assert_eq!(metadata_sha256, Digest::of(metadata).as_str());
    assert_eq!(source, "pypi");
}
#[tokio::test]
async fn test_metadata_backfill_upstream_range_error_is_bad_gateway() {
    let h = harness().await;
    let wheel = fixture_wheel_with_metadata(b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n");
    let digest = Digest::of(&wheel);
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
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
    let digest = h.state.blobs.put_bytes(&wheel).await.unwrap();
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();

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
    h.state.meta.put_file_url(digest.as_str(), &file_url, "pypi").unwrap();
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
    assert!(h.state.blobs.head(&digest).await.unwrap().is_some());
}
#[tokio::test]
async fn test_metadata_backfill_downloads_sdist_without_ranges() {
    let h = harness().await;
    let sdist = fixture_sdist();
    let digest = Digest::of(&sdist);
    let filename = "peryxpkg-1.0.tar.gz";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
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
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", wheel.len()),
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
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", "0"),
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
async fn test_metadata_backfill_downloads_when_range_is_unusable() {
    struct Case {
        label: &'static str,
        build_ranged: fn(&[u8], &[u8]) -> Vec<u8>,
    }
    let metadata = b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
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
            label: "deflate is invalid",
            build_ranged: |metadata, _wheel| wheel_with_invalid_deflated_metadata(metadata),
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

    for case in cases {
        let h = harness().await;
        let ranged = (case.build_ranged)(metadata, &wheel);

        assert_metadata_range_fallback(&h, case.label, ranged, wheel.clone(), metadata).await;
    }
}
#[tokio::test]
async fn test_metadata_backfill_skips_ranges_after_disable() {
    let h = harness().await;
    let first = fixture_wheel_with_metadata(b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\n");
    let first_digest = Digest::of(&first);
    let first_filename = "peryxpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(
            first_digest.as_str(),
            &format!("{}/files/{first_filename}", h.server.uri()),
            "pypi",
        )
        .unwrap();
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
    h.state
        .meta
        .put_file_url(
            second_digest.as_str(),
            &format!("{}/files/{second_filename}", h.server.uri()),
            "pypi",
        )
        .unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/files/{second_filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(second))
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
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", wheel.len()),
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
    h.state
        .meta
        .put_metadata(artifact.as_str(), &metadata_url, metadata.as_str(), "pypi")
        .unwrap();
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

fn oversized_metadata_server() -> String {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
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
    format!("http://{addr}/pkg.whl.metadata")
}
