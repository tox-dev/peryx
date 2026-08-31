use super::support::*;
use peryx_identity::IndexAcl;

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
    let digest = put_local_file(&h.state, "peryxpkg-1.0 x#?.whl", &fixture_wheel(), "1.0");
    let uri = format!(
        "/hosted/inspect/{}/peryxpkg-1.0%20x%23%3F.whl?member=peryxpkg-1.0.dist-info%2FMETADATA",
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
async fn test_inspect_truncated_member_is_unprocessable() {
    let h = harness().await;
    let mut header = tar::Header::new_gnu();
    header.set_path("file.txt").unwrap();
    header.set_mode(0o644);
    header.set_size(5);
    header.set_cksum();
    let mut archive = header.as_bytes().to_vec();
    archive.extend_from_slice(b"abc");
    let digest = put_local_file(&h.state, "peryxpkg-1.0.tar", &archive, "1.0");
    let uri = format!("/hosted/inspect/{}/peryxpkg-1.0.tar/file.txt?limit=5", digest.as_str());

    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("ended after 3 bytes but declares 5 bytes"));
}

#[tokio::test]
async fn test_inspect_decompression_limit_is_payload_too_large() {
    let h = harness().await;
    let mut archive = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut archive));
        zip.start_file(
            "file.txt",
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        zip.write_all(b"x").unwrap();
        zip.finish().unwrap();
    }
    let central = archive.windows(4).position(|window| window == b"PK\x01\x02").unwrap();
    archive[central + 24..central + 28].copy_from_slice(&0x6000_0000_u32.to_le_bytes());
    let digest = put_local_file(&h.state, "peryxpkg-1.0-py3-none-any.whl", &archive, "1.0");
    let uri = format!(
        "/hosted/inspect/{}/peryxpkg-1.0-py3-none-any.whl/file.txt?offset=536870913&limit=1",
        digest.as_str()
    );

    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body.contains("decompressed byte limit of 536870912"));
}

#[tokio::test]
async fn test_inspect_tarball_and_size_limit() {
    let h = harness().await;

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
    assert_eq!(
        body,
        "invalid artifact name \"pkg/name.whl\": artifact names must be relative path segments without separators, traversal, or control characters"
    );
    let uri = format!("/hosted/inspect/{}/ghost.whl", "a".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
/// The file route's download gate stands in front of archive inspection too, or quarantining a
/// project would leave every text member of its artifacts readable. The refusal is the file route's
/// own, and it lands before the wheel is pulled from upstream. See #1524.
#[tokio::test]
async fn test_inspect_refuses_a_quarantined_project_without_fetching_it() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel);
    mount_inspectable_project(&h, "quarantined", &wheel, &digest, 0).await;
    get(&h.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;

    let uri = format!("/pypi/inspect/{}/peryxpkg-1.0-py3-none-any.whl", digest.as_str());
    let (status, headers, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        "project for file \"peryxpkg-1.0-py3-none-any.whl\" is quarantined; downloads are disabled"
    );
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/plain; charset=utf-8");
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_none());
}
/// The same refusal answers a member read, not only the listing: `?member=` is the form that hands
/// back the artifact's contents.
#[tokio::test]
async fn test_inspect_refuses_a_quarantined_project_member_read() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel);
    mount_inspectable_project(&h, "quarantined", &wheel, &digest, 0).await;
    get(&h.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;

    let uri = format!(
        "/pypi/inspect/{}/peryxpkg-1.0-py3-none-any.whl?member=peryxpkg-1.0.dist-info%2FMETADATA",
        digest.as_str()
    );
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        "project for file \"peryxpkg-1.0-py3-none-any.whl\" is quarantined; downloads are disabled"
    );
}
/// The refusal says nothing about whether the digest names anything: an unknown artifact on a
/// quarantined project answers exactly what a cached one does.
#[tokio::test]
async fn test_inspect_refusal_does_not_disclose_whether_the_artifact_exists() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel);
    mount_inspectable_project(&h, "quarantined", &wheel, &digest, 0).await;
    get(&h.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;

    let known = format!("/pypi/inspect/{}/peryxpkg-1.0-py3-none-any.whl", digest.as_str());
    let unknown = format!("/pypi/inspect/{}/peryxpkg-1.0-py3-none-any.whl", "b".repeat(64));

    assert_eq!(get(&h.state, &known, None).await, get(&h.state, &unknown, None).await);
}
/// The gate is a gate, not a wall: the same project inspects normally while its status is active.
#[tokio::test]
async fn test_inspect_lists_members_of_an_active_project() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel);
    mount_inspectable_project(&h, "active", &wheel, &digest, 1).await;
    get(&h.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;

    let uri = format!("/pypi/inspect/{}/peryxpkg-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    let listing: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listing["members"][0]["path"], "peryxpkg-1.0.dist-info/METADATA");
}
/// A cached `peryxpkg` whose upstream page carries `status`, with its wheel downloadable exactly
/// `fetches` times - `0` proves the gate refuses before reaching for the bytes.
async fn mount_inspectable_project(h: &Harness, status: &str, wheel: &[u8], digest: &Digest, fetches: u64) {
    let file_url = format!("{}/files/peryxpkg.whl", h.server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\"}},\
         \"project-status\":{{\"status\":\"{status}\",\"reason\":\"malware\"}},\
         \"name\":\"peryxpkg\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"peryxpkg-1.0-py3-none-any.whl\",\"size\":{},\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{}\"}}}}]}}",
        wheel.len(),
        digest.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(page.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/peryxpkg.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel.to_vec()))
        .expect(fetches)
        .mount(&h.server)
        .await;
}
/// A store that cannot answer the project's status refuses inspection rather than falling through
/// to the archive.
#[tokio::test]
async fn test_inspect_download_status_store_error_is_server_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("peryx.redb");
    MetaStore::open(&db_path).unwrap();
    put_raw_project_status(&db_path, "pypi/peryxpkg", b"not json");
    let meta = MetaStore::open(&db_path).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Cached {
            client: upstream,
            offline: false,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));

    let uri = format!(
        "/pypi/inspect/{}/peryxpkg-1.0-py3-none-any.whl",
        Digest::of(b"wheel").as_str()
    );
    let (status, _, body) = get(&state, &uri, None).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("file download on index \"pypi\""));
    assert!(body.contains("metadata store error"));
}
