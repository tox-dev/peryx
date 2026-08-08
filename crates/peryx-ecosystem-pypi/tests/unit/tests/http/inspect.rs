//! Browsing inside a stored archive: members, nested containers, and their limits.

use super::support::*;

#[tokio::test]
async fn test_inspect_lists_wheel_members() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!("/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let listing: serde_json::Value = serde_json::from_str(&body).unwrap();
    let paths: Vec<&str> = listing["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        [
            "peryxpkg-1.0.dist-info/METADATA",
            "peryxpkg-1.0.dist-info/RECORD",
            "peryxpkg-1.0.dist-info/WHEEL",
            "peryxpkg/__init__.py"
        ]
    );
}
#[tokio::test]
async fn test_inspect_reads_member_content() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!(
        "/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl/peryxpkg-1.0.dist-info/METADATA",
        digest.as_str()
    );
    let (status, headers, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/plain; charset=utf-8");
    assert!(body.starts_with("Metadata-Version: 2.1"));
}
#[tokio::test]
async fn test_inspect_reads_query_member_content() {
    let h = harness().await;
    let digest = put_local_file(&h.state, "peryxpkg 1.0#x?.whl", &fixture_wheel(), "1.0");
    let uri = format!(
        "/hosted/inspect/{}/peryxpkg%201.0%23x%3F.whl?member=peryxpkg-1.0.dist-info%2FMETADATA",
        digest.as_str()
    );
    let (status, headers, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/plain; charset=utf-8");
    assert!(body.starts_with("Metadata-Version: 2.1"));
}
#[tokio::test]
async fn test_inspect_query_without_member_lists_archive() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!(
        "/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl?ignored=1",
        digest.as_str()
    );
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("peryxpkg-1.0.dist-info/METADATA"));
}
#[tokio::test]
async fn test_inspect_legacy_member_rejects_invalid_encoding() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!("/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl/%FF", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid percent-encoded path segment"));
}
#[tokio::test]
async fn test_inspect_missing_member_is_not_found() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!(
        "/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl/nope.py",
        digest.as_str()
    );
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_inspect_rejects_bad_member_chunk_parameters() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!(
        "/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl?member=peryxpkg-1.0.dist-info%2FMETADATA",
        digest.as_str()
    );

    let (status, _, body) = get(&h.state, &format!("{uri}&limit=0"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("limit must be between 1 and"));

    let (status, _, body) = get(&h.state, &format!("{uri}&limit=nope"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("limit must be an integer between 1 and 1048576"));

    let (status, _, body) = get(&h.state, &format!("{uri}&offset=nope"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("offset must be a non-negative integer"));

    let (status, headers, body) = get(&h.state, &format!("{uri}&limit=8"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "Metadata");
    assert_eq!(headers.get("x-peryx-next-offset").unwrap(), "8");

    let (status, _, body) = get(&h.state, &format!("{uri}&offset=999999"), None).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(body.contains("offset 999999 is beyond member size"));
}
#[tokio::test]
async fn test_inspect_unsupported_type() {
    let h = harness().await;
    let digest = put_local_file(&h.state, "peryxpkg-1.0.txt", b"not an archive", "1.0");
    let uri = format!("/hosted/inspect/{}/peryxpkg-1.0.txt", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}
#[tokio::test]
async fn test_inspect_corrupt_archive_is_unprocessable() {
    let h = harness().await;
    let digest = put_local_file(&h.state, "peryxpkg-1.0-py3-none-any.whl", b"PK corrupt bytes", "1.0");
    let uri = format!("/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
#[tokio::test]
async fn test_inspect_tarball_and_size_limit() {
    let h = harness().await;
    // A gzipped tarball with one small file and one over the inline limit.
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let small = b"print()\n";
        let mut head = tar::Header::new_gnu();
        head.set_size(small.len() as u64);
        head.set_cksum();
        builder
            .append_data(&mut head, "peryxpkg-1.0/setup.py", &small[..])
            .unwrap();
        let big = vec![b'a'; usize::try_from(crate::archive::DEFAULT_MEMBER_CHUNK + 1).unwrap()];
        let mut head = tar::Header::new_gnu();
        head.set_size(big.len() as u64);
        head.set_cksum();
        builder
            .append_data(&mut head, "peryxpkg-1.0/big.txt", big.as_slice())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    let digest = put_local_file(&h.state, "peryxpkg-1.0.tar.gz", &tarball, "1.0");

    let uri = format!("/hosted/inspect/{}/peryxpkg-1.0.tar.gz", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("setup.py"));

    let (status, _, content) = get(&h.state, &format!("{uri}/peryxpkg-1.0/setup.py"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content, "print()\n");

    let (status, headers, content) = get(&h.state, &format!("{uri}/peryxpkg-1.0/big.txt"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        content.len(),
        usize::try_from(crate::archive::DEFAULT_MEMBER_CHUNK).unwrap()
    );
    assert_eq!(
        headers.get("x-peryx-next-offset").unwrap(),
        crate::archive::DEFAULT_MEMBER_CHUNK.to_string().as_str()
    );

    let (status, headers, content) = get(
        &h.state,
        &format!(
            "{uri}/peryxpkg-1.0/big.txt?offset={}",
            crate::archive::DEFAULT_MEMBER_CHUNK
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content.len(), 1);
    assert!(!headers.contains_key("x-peryx-next-offset"));
}
#[tokio::test]
async fn test_inspect_binary_member_rejected_for_inline_preview() {
    let h = harness().await;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("data.bin", options).unwrap();
        zip.write_all(&[0xff, 0xfe, 0x00]).unwrap();
        zip.finish().unwrap();
    }
    let digest = put_local_file(&h.state, "peryxpkg-1.0-py3-none-any.whl", &buf, "1.0");
    let uri = format!(
        "/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl/data.bin",
        digest.as_str()
    );
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(body.contains("cannot be previewed inline"));
}
#[tokio::test]
async fn test_inspect_nested_archive_lists_selected_container_only() {
    let h = harness().await;
    let inner = {
        let mut buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("pkg/mod.py", options).unwrap();
        zip.write_all(b"x = 1\n").unwrap();
        zip.finish().unwrap();
        buf
    };
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("vendor/inner.zip", options).unwrap();
        zip.write_all(&inner).unwrap();
        zip.finish().unwrap();
    }
    let digest = put_local_file(&h.state, "peryxpkg-1.0-py3-none-any.whl", &buf, "1.0");
    let uri = format!(
        "/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl?container=vendor%2Finner.zip",
        digest.as_str()
    );

    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("pkg/mod.py"));

    let (status, _, content) = get(&h.state, &format!("{uri}&member=pkg%2Fmod.py"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content, "x = 1\n");
}
#[tokio::test]
async fn test_inspect_nested_archive_depth_limit_is_bad_request() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let mut uri = format!("/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl?", digest.as_str());
    for position in 0..=crate::archive::MAX_CONTAINER_DEPTH {
        if position > 0 {
            uri.push('&');
        }
        uri.push_str("container=inner.zip");
    }

    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("exceeds the configured limit"));
}
#[tokio::test]
async fn test_inspect_archive_listing_limit_is_payload_too_large() {
    let h = harness().await;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        for position in 0..=crate::archive::MAX_LISTED_ENTRIES {
            zip.start_file(format!("pkg/file-{position}.py"), options).unwrap();
            zip.write_all(b"").unwrap();
        }
        zip.finish().unwrap();
    }
    let digest = put_local_file(&h.state, "peryxpkg-1.0-py3-none-any.whl", &buf, "1.0");
    let uri = format!("/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl", digest.as_str());

    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body.contains("archive listing exceeds"));
}
#[tokio::test]
async fn test_inspect_bad_digest_and_missing_paths() {
    let h = harness().await;
    let (status, _, body) = get(&h.state, "/hosted/inspect/nothex/x.whl", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("expected 64 lowercase hex sha256"));
    let (status, ..) = get(&h.state, "/hosted/inspect/onlyonesegment", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let uri = format!("/hosted/inspect/{}/pkg%2Fname.whl", "a".repeat(64));
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("filenames must be relative path segments"));
    let uri = format!("/hosted/inspect/{}/ghost.whl", "a".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
