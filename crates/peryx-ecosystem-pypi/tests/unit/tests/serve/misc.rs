use super::support::*;
use peryx_driver::serving::{BrowseDriver as _, BrowseRequest, IndexedProtocolDriver as _};
use peryx_ha::ArtifactPlacement;
use rstest::rstest;

#[tokio::test]
async fn test_negative_cache_expires_by_clock() {
    let h = harness().await;

    h.state.serving.remember_negative("missing".to_owned(), 30);
    assert!(h.state.serving.negative_fresh("missing"));
    h.clock.fetch_add(31, Ordering::Relaxed);

    assert!(!h.state.serving.negative_fresh("missing"));
    assert!(!h.state.serving.negative_fresh("missing"));
}
#[tokio::test]
async fn test_gate_waiter_finds_the_hot_entry_after_a_revalidation() {
    let dir = tempfile::tempdir().unwrap();
    let digest = Digest::of(b"wheel");
    let body = detail_json(digest.as_str(), "https://files.example/flask.whl");
    let mut stalled = revalidating_upstream();
    let state = cached_state(&dir, &stalled.upstream);
    let mut record = fresh_record(body.as_bytes());
    record.etag = Some("\"v1\"".to_owned());
    record.fetched_at_unix = 0;
    state.serving.meta.put_index("pypi/flask", &record).unwrap();

    // A 304 must wake concurrent revalidators without advancing the epoch.
    let first = tokio::spawn({
        let state = state.clone();
        async move { get(&state, "/pypi/simple/flask/", Some("application/json")).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), stalled.wait_until_entered())
        .await
        .expect("the first revalidation reaches upstream");
    let second = tokio::spawn({
        let state = state.clone();
        async move { get(&state, "/pypi/simple/flask/", Some("application/json")).await }
    });
    stalled.release();
    let (a, b) = tokio::join!(first, second);
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
    assert_eq!(a.2, b.2);
}
#[tokio::test]
async fn test_corrupt_cached_page_falls_back_and_fails_loudly() {
    let h = harness().await;
    h.state
        .serving
        .meta
        .put_index("pypi/flask", &fresh_record(br#"{"files":[{"bad": }]}"#))
        .unwrap();
    let (status, ..) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_broken_upstream_transfer_forwards_the_error() {
    let h = harness().await;
    let server = response_server(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort");
    let digest = Digest::of(b"never arrives");
    h.state
        .serving
        .meta
        .put_file_url(
            "pypi",
            &crate::project_of_filename("x.whl"),
            digest.as_str(),
            &server.upstream,
            "pypi",
        )
        .unwrap();
    let mut stream = live_stream(
        cache::stream_file(
            h.state.serving.clone(),
            "pypi".to_owned(),
            digest.clone(),
            "pypi".to_owned(),
            "x.whl".to_owned(),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        saw_error |= item.is_err();
    }
    assert!(saw_error);
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_cached_file_stream_returns_no_live_body() {
    let h = harness().await;
    let digest = h.state.serving.blobs.put_bytes(b"cached wheel").await.unwrap();

    let outcome = cache::stream_file(
        h.state.serving.clone(),
        "pypi".to_owned(),
        digest,
        "pypi".to_owned(),
        "cached.whl".to_owned(),
    )
    .await
    .unwrap();

    assert!(live_stream(outcome).is_none());
}
#[tokio::test]
async fn test_buffered_fetch_registers_metadata_siblings() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let meta_digest = Digest::of(b"meta");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}},\"core-metadata\":{{\"sha256\":\"{meta}\"}}}}]}}",
        digest = digest.as_str(),
        meta = meta_digest.as_str(),
    );
    mount_json_page(&h.server, &page).await;

    let (status, ..) = get(&h.state, "/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::OK);
    let publication = h
        .state
        .serving
        .meta
        .get_file_publication("pypi", "flask", digest.as_str(), "flask-1.0-py3-none-any.whl")
        .unwrap();
    assert_eq!(
        publication,
        Some(crate::store::FilePublication::Claimed(crate::store::MetadataClaim {
            url: format!("{file_url}.metadata"),
            metadata_sha256: meta_digest.as_str().to_owned(),
            source: "pypi".to_owned(),
            upstream: None,
        }))
    );
}

fn detail_with_metadata(wheel: &str, url: &str, meta: &str) -> String {
    format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{url}\",\
         \"hashes\":{{\"sha256\":\"{wheel}\"}},\"core-metadata\":{{\"sha256\":\"{meta}\"}}}}]}}"
    )
}

#[tokio::test]
async fn test_artifact_path_rejects_an_invalid_digest() {
    let h = harness().await;
    let err = browse_artifact(h.state.serving.clone(), 0, "pypi", "not-hex", "flask.whl")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid sha256 digest"), "{err}");
}

/// A digest the index never published is refused in the words it answers an unknown digest with,
/// so browsing cannot report on what another index holds. See #1308.
#[tokio::test]
async fn test_artifact_path_rejects_a_digest_that_is_not_a_project_member() {
    let h = harness().await;
    let listed = Digest::of(b"listed wheel");
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{}/files/flask.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}}}}]}}",
        h.server.uri(),
        listed.as_str(),
    );
    mount_json_page(&h.server, &page).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let foreign = Digest::of(b"another project's wheel");
    let refused = browse_artifact(
        h.state.serving.clone(),
        0,
        "pypi",
        foreign.as_str(),
        "flask-1.0-py3-none-any.whl",
    )
    .await
    .unwrap_err();
    let unknown = Digest::of(b"nothing ever stored this");
    let absent = browse_artifact(
        h.state.serving.clone(),
        0,
        "pypi",
        unknown.as_str(),
        "flask-1.0-py3-none-any.whl",
    )
    .await
    .unwrap_err();

    assert_eq!(refused.to_string(), browse_not_found(foreign.as_str()));
    assert_eq!(absent.to_string(), browse_not_found(unknown.as_str()));
}

/// The browse query's project is what the caller's read was authorized against, so a file naming
/// another project is refused there even when the index publishes it.
#[tokio::test]
async fn test_artifact_path_rejects_a_file_from_another_project() {
    let h = harness().await;
    let digest = Digest::of(b"listed wheel");
    crate::tests::register_publication(
        &h.state.serving.meta,
        "pypi",
        "flask-1.0-py3-none-any.whl",
        digest.as_str(),
        None,
    );

    let err = browse_artifact_in(
        h.state.serving.clone(),
        0,
        "pypi",
        "django",
        digest.as_str(),
        "flask-1.0-py3-none-any.whl",
    )
    .await
    .unwrap_err();

    assert_eq!(err.to_string(), browse_not_found(digest.as_str()));
}

fn browse_not_found(digest: &str) -> String {
    format!(
        "artifact on index \"pypi\" for file \"flask-1.0-py3-none-any.whl\" with digest {digest}: \
         no matching cached file or upstream source was found"
    )
}

#[tokio::test]
async fn test_artifact_path_reports_an_unfetchable_member_file() {
    let h = harness().await;
    let digest = Digest::of(b"never stored");
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{}/files/flask.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}}}}]}}",
        h.server.uri(),
        digest.as_str(),
    );
    mount_json_page(&h.server, &page).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let err = browse_artifact(
        h.state.serving.clone(),
        0,
        "pypi",
        digest.as_str(),
        "flask-1.0-py3-none-any.whl",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("artifact on index"), "{err}");
}

#[rstest]
#[case::pep440(&["2.0", "1!1.0rc1", "10.0", "1!1.0.post01", "1!1.0.post1", "1.0"], "1!1.0.post1")]
#[case::legacy(&["legacy-z", "legacy-a"], "legacy-z")]
#[tokio::test]
async fn test_project_page_selects_latest_version(#[case] versions: &[&str], #[case] expected: &str) {
    let h = harness().await;
    let page = crate::to_json(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "flask",
        "versions": versions,
        "files": [],
    }));
    mount_json_page(&h.server, &page).await;
    let page = browse_project(h.state.serving.clone(), 0, "pypi")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.subtitle.as_deref(), Some(expected));
}

fn detail_with_yanks(versions: &[&str], files: &[(&str, bool)]) -> String {
    let files = files
        .iter()
        .map(|(version, yanked)| {
            serde_json::json!({
                "filename": format!("flask-{version}-py3-none-any.whl"),
                "url": format!("/files/flask-{version}-py3-none-any.whl"),
                "size": 11,
                "hashes": {"sha256": Digest::of(version.as_bytes()).as_str()},
                "yanked": yanked,
            })
        })
        .collect::<Vec<_>>();
    crate::to_json(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "flask",
        "versions": versions,
        "files": files,
    }))
}

#[rstest]
#[case::active_beats_yanked(&["2.0", "4.0"], &[("2.0", false), ("4.0", true)], "2.0")]
#[case::stable_beats_prerelease(&["2.0", "3.0rc1"], &[("2.0", false), ("3.0rc1", false)], "2.0")]
#[case::greatest_active_stable(&["2.0", "3.0"], &[("2.0", false), ("3.0", false)], "3.0")]
#[case::one_active_file_keeps_the_release(&["2.0", "4.0"], &[("2.0", false), ("4.0", true), ("4.0", false)], "4.0")]
#[case::active_prerelease_beats_yanked_stable(&["2.0", "3.0rc1"], &[("2.0", true), ("3.0rc1", false)], "3.0rc1")]
#[case::all_yanked_falls_back_to_greatest(&["2.0", "4.0"], &[("2.0", true), ("4.0", true)], "4.0")]
#[tokio::test]
async fn test_project_page_prefers_an_active_stable_release(
    #[case] versions: &[&str],
    #[case] files: &[(&str, bool)],
    #[case] expected: &str,
) {
    let h = harness().await;
    mount_json_page(&h.server, &detail_with_yanks(versions, files)).await;
    let page = browse_project(h.state.serving.clone(), 0, "pypi")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.subtitle.as_deref(), Some(expected));
}

fn release_wheel(index: usize, version: &str) -> String {
    format!("flask-{version}-{index}-py3-none-any.whl")
}

fn release_metadata(index: usize, version: &str) -> String {
    format!(
        "Metadata-Version: 2.1\nName: flask\nVersion: {version}\nSummary: {}\n",
        release_wheel(index, version)
    )
}

fn detail_with_release_metadata(server: &MockServer, versions: &[&str], files: &[(&str, bool, bool)]) -> String {
    let files = files
        .iter()
        .enumerate()
        .map(|(index, (version, yanked, sibling))| {
            let wheel = release_wheel(index, version);
            let mut file = serde_json::json!({
                "filename": wheel,
                "url": format!("{}/files/{wheel}", server.uri()),
                "size": 11,
                "hashes": {"sha256": Digest::of(wheel.as_bytes()).as_str()},
                "yanked": yanked,
            });
            if *sibling {
                let digest = Digest::of(release_metadata(index, version).as_bytes());
                file["core-metadata"] = serde_json::json!({"sha256": digest.as_str()});
            }
            file
        })
        .collect::<Vec<_>>();
    crate::to_json(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "flask",
        "versions": versions,
        "files": files,
    }))
}

#[rstest]
#[case::default_release_over_a_later_sibling(
    &["1.0", "2.0"],
    &[("2.0", false, true), ("1.0", false, true)],
    "2.0",
    Some("flask-2.0-0-py3-none-any.whl"),
)]
#[case::pep440_equal_release(&["2.0.0"], &[("2.0", false, true)], "2.0.0", Some("flask-2.0-0-py3-none-any.whl"))]
#[case::active_sibling_over_a_yanked_one(
    &["2.0"],
    &[("2.0", true, true), ("2.0", false, true)],
    "2.0",
    Some("flask-2.0-1-py3-none-any.whl"),
)]
#[case::first_filename_settles_a_tie(
    &["2.0"],
    &[("2.0", false, true), ("2.0", false, true)],
    "2.0",
    Some("flask-2.0-0-py3-none-any.whl"),
)]
#[case::release_without_a_sibling(
    &["1.0", "2.0"],
    &[("2.0", false, false), ("1.0", false, true)],
    "2.0",
    None,
)]
#[case::no_versions_listed(
    &[],
    &[("1.0", false, true), ("2.0", false, true)],
    "2.0",
    Some("flask-2.0-1-py3-none-any.whl"),
)]
#[tokio::test]
async fn test_project_page_reads_metadata_from_the_default_release(
    #[case] versions: &[&str],
    #[case] files: &[(&str, bool, bool)],
    #[case] version: &str,
    #[case] summary: Option<&str>,
) {
    let h = harness().await;
    mount_json_page(&h.server, &detail_with_release_metadata(&h.server, versions, files)).await;

    get(&h.state, "/pypi/simple/flask/", None).await;
    for (index, (release, ..)) in files.iter().enumerate() {
        h.state
            .serving
            .blobs
            .put_bytes(release_metadata(index, release).as_bytes())
            .await
            .unwrap();
    }
    let page = browse_project(h.state.serving.clone(), 0, "pypi")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (page.subtitle.as_deref(), page.summary.as_deref()),
        (Some(version), summary)
    );
}

#[tokio::test]
async fn test_project_page_surfaces_a_resolve_error() {
    let h = harness().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    let err = browse_project(h.state.serving.clone(), 0, "pypi").await.unwrap_err();
    assert!(err.to_string().contains("project detail on index"), "{err}");
}

#[tokio::test]
async fn test_an_upper_case_upstream_digest_serves_a_downloadable_file() {
    let h = harness().await;
    let wheel = b"wheel bytes";
    let digest = Digest::of(wheel);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(
        &h.server,
        &detail_json(&digest.as_str().to_ascii_uppercase(), &file_url),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(wheel.to_vec(), "application/octet-stream"))
        .mount(&h.server)
        .await;
    let (_, _, page) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let served = serde_json::from_str::<serde_json::Value>(&page).unwrap()["files"][0]["url"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, _, body) = get(&h.state, &served, None).await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, "wheel bytes"));
}

#[tokio::test]
async fn test_served_page_drops_a_wheel_digest_that_is_not_a_content_address() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(&h.server, &detail_with_metadata("not-a-digest", &file_url, "also-bad")).await;

    let (_, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["files"],
        serde_json::json!([{
            "filename": "flask-1.0-py3-none-any.whl",
            "size": 11,
            "url": file_url,
            "hashes": {},
            "yanked": false,
            "core-metadata": false,
        }])
    );
}

#[tokio::test]
async fn test_project_page_renders_a_wheel_digest_that_is_not_a_content_address() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(&h.server, &detail_with_metadata("not-a-digest", &file_url, "also-bad")).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let page = browse_project(h.state.serving.clone(), 0, "pypi").await.unwrap();

    assert!(page.is_some());
}

#[tokio::test]
async fn test_project_page_reports_an_unfetchable_metadata_sibling() {
    let h = harness().await;
    let wheel = Digest::of(b"the wheel");
    let meta = Digest::of(b"the metadata");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(
        &h.server,
        &detail_with_metadata(wheel.as_str(), &file_url, meta.as_str()),
    )
    .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let err = browse_project(h.state.serving.clone(), 0, "pypi").await.unwrap_err();
    assert!(err.to_string().contains("metadata fetch on index"), "{err}");
}

#[tokio::test]
async fn test_project_page_reports_a_malformed_metadata_sibling() {
    let h = harness().await;
    let wheel = Digest::of(b"the wheel");
    let sibling = b"Metadata-Version: 2.4\nName: flask\nmalformed header\nVersion: 1.0\n";
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(
        &h.server,
        &detail_with_metadata(wheel.as_str(), &file_url, Digest::of(sibling).as_str()),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sibling.to_vec(), "application/octet-stream"))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let err = browse_project(h.state.serving.clone(), 0, "pypi").await.unwrap_err();

    assert!(
        err.to_string()
            .contains("metadata parse on index \"pypi\" for file \"flask-1.0-py3-none-any.whl\": header line"),
        "{err}"
    );
}

#[tokio::test]
async fn test_project_page_is_absent_for_an_unknown_hosted_project() {
    let h = harness().await;

    let page = browse_project(h.state.serving.clone(), 1, "hosted").await.unwrap();
    assert!(page.is_none());
}

#[tokio::test]
async fn test_project_page_reads_cached_and_remote_only_placements() {
    use peryx_storage::meta::ArtifactSource;

    let h = placement_harness().await;
    let cached = Digest::of(b"cached wheel bytes");

    h.state
        .serving
        .meta
        .put_artifact_placement(cached.as_str(), &ArtifactPlacement::record(ArtifactSource::Proxy, true))
        .unwrap();
    let remote = Digest::of(b"a wheel this proxy has never downloaded");
    let page = crate::to_json(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "flask",
        "versions": ["1.0", "2.0"],
        "files": [
            {
                "filename": "flask-1.0-py3-none-any.whl",
                "size":11,"url": "https://files.example/flask-1.0-py3-none-any.whl",
                "hashes": {"sha256": cached.as_str()},
            },
            {
                "filename": "flask-2.0-py3-none-any.whl",
                "size":11,"url": "https://files.example/flask-2.0-py3-none-any.whl",
                "hashes": {"sha256": remote.as_str()},
            },
        ],
    }));
    mount_json_page(&h.server, &page).await;

    let page = browse_project(h.state.serving.clone(), 0, "pypi")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        file_source_and_availability(&page, "flask-1.0-py3-none-any.whl"),
        ("proxy", "local")
    );
    assert_eq!(
        file_source_and_availability(&page, "flask-2.0-py3-none-any.whl"),
        ("proxy", "remote_only")
    );
}

#[tokio::test]
async fn test_project_page_maps_each_placement_source_and_availability() {
    use peryx_storage::meta::ArtifactSource;

    let h = placement_harness().await;
    let generated = Digest::of(b"a generated sibling");
    let remote = Digest::of(b"a proxied catalog entry");
    let orphan = Digest::of(b"a hosted digest with lost bytes");
    for (digest, source, present) in [
        (&generated, ArtifactSource::Generated, true),
        (&remote, ArtifactSource::Proxy, false),
        (&orphan, ArtifactSource::Hosted, false),
    ] {
        h.state
            .serving
            .meta
            .put_artifact_placement(digest.as_str(), &ArtifactPlacement::record(source, present))
            .unwrap();
    }
    let page = crate::to_json(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "flask",
        "versions": ["1.0"],
        "files": [
            {"filename": "flask-1.0.tar.gz", "size":11,"url": "https://files.example/flask-1.0.tar.gz", "hashes": {"sha256": generated.as_str()}},
            {"filename": "flask-1.0-py3-none-any.whl", "size":11,"url": "https://files.example/flask-1.0-py3-none-any.whl", "hashes": {"sha256": remote.as_str()}},
            {"filename": "flask-1.0-py2-none-any.whl", "size":11,"url": "https://files.example/flask-1.0-py2-none-any.whl", "hashes": {"sha256": orphan.as_str()}},
        ],
    }));
    mount_json_page(&h.server, &page).await;

    let page = browse_project(h.state.serving.clone(), 0, "pypi")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        file_source_and_availability(&page, "flask-1.0.tar.gz"),
        ("generated", "local")
    );
    assert_eq!(
        file_source_and_availability(&page, "flask-1.0-py3-none-any.whl"),
        ("proxy", "remote_only")
    );
    assert_eq!(
        file_source_and_availability(&page, "flask-1.0-py2-none-any.whl"),
        ("hosted", "unavailable")
    );
}

#[tokio::test]
async fn test_project_page_reads_a_hosted_upload_with_lost_bytes_as_unavailable() {
    use peryx_storage::meta::ArtifactSource;

    let h = placement_harness().await;
    let filename = "flask-1.0-py3-none-any.whl";
    let digest = Digest::of(b"a hosted upload whose bytes were lost");
    let uploaded = serde_json::json!({
        "version": "1.0",
        "file": {
            "filename": filename,
            "url": format!("/hosted/files/{}/{filename}", digest.as_str()),
            "hashes": {"sha256": digest.as_str()},
        },
    });
    h.state
        .serving
        .meta
        .put_upload("hosted", "flask", filename, crate::to_json(&uploaded).as_bytes())
        .unwrap();

    h.state
        .serving
        .meta
        .put_artifact_placement(
            digest.as_str(),
            &ArtifactPlacement::record(ArtifactSource::Hosted, false),
        )
        .unwrap();

    let page = browse_project(h.state.serving.clone(), 2, "root/pypi")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(file_source_and_availability(&page, filename), ("hosted", "unavailable"));
}

#[tokio::test]
async fn test_project_page_marks_a_hosted_upload_over_its_cached_blob() {
    use peryx_storage::meta::ArtifactSource;

    let h = harness().await;
    let filename = "flask-1.0-py3-none-any.whl";
    // Hosted membership outranks stale proxied placement metadata.
    let digest = h.state.serving.blobs.put_bytes(b"hosted wheel bytes").await.unwrap();
    h.state
        .serving
        .meta
        .put_artifact_placement(
            digest.as_str(),
            &ArtifactPlacement::record(ArtifactSource::Proxy, false),
        )
        .unwrap();
    let uploaded = serde_json::json!({
        "version": "1.0",
        "file": {
            "filename": filename,
            "url": format!("/hosted/files/{}/{filename}", digest.as_str()),
            "hashes": {"sha256": digest.as_str()},
        },
    });
    h.state
        .serving
        .meta
        .put_upload("hosted", "flask", filename, crate::to_json(&uploaded).as_bytes())
        .unwrap();

    let page = browse_project(h.state.serving.clone(), 2, "root/pypi")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(file_source_and_availability(&page, filename), ("hosted", "local"));
}

async fn browse_project(
    state: Arc<peryx_driver::state::ServingState>,
    position: usize,
    index: &str,
) -> Result<Option<peryx_core::BrowsePage>, peryx_driver::serving::BrowseError> {
    let access = peryx_driver::access::ReadAccess::from_headers(&state, &axum::http::HeaderMap::new());
    crate::serving::PypiServing
        .browse(BrowseRequest {
            state,
            position,
            raw_query: format!("index={index}&project=flask"),
            access: &access,
            base: None,
        })
        .await
}

async fn browse_artifact(
    state: Arc<peryx_driver::state::ServingState>,
    position: usize,
    index: &str,
    digest: &str,
    filename: &str,
) -> Result<Option<peryx_core::BrowsePage>, peryx_driver::serving::BrowseError> {
    browse_artifact_in(state, position, index, "flask", digest, filename).await
}

async fn browse_artifact_in(
    state: Arc<peryx_driver::state::ServingState>,
    position: usize,
    index: &str,
    project: &str,
    digest: &str,
    filename: &str,
) -> Result<Option<peryx_core::BrowsePage>, peryx_driver::serving::BrowseError> {
    let access = peryx_driver::access::ReadAccess::from_headers(&state, &axum::http::HeaderMap::new());
    crate::serving::PypiServing
        .browse(BrowseRequest {
            state,
            position,
            raw_query: format!("index={index}&project={project}&sha256={digest}&file={filename}"),
            access: &access,
            base: None,
        })
        .await
}

fn file_source_and_availability<'a>(page: &'a peryx_core::BrowsePage, filename: &str) -> (&'a str, &'a str) {
    let row = page
        .sections
        .iter()
        .find_map(|section| match section {
            peryx_core::BrowseSection::Table { heading, rows, .. } if heading == "Files" => rows
                .iter()
                .find(|row| row.cells.first().is_some_and(|cell| cell.text == filename)),
            _ => None,
        })
        .unwrap();
    assert_eq!(row.cells.len(), 6);
    (&row.cells[4].text, &row.cells[5].text)
}

fn live_stream(
    outcome: cache::FileOutcome,
) -> Option<futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>>> {
    match outcome {
        cache::FileOutcome::Live(stream) => Some(stream),
        cache::FileOutcome::Cached(_) => None,
    }
}

#[rstest]
#[case::unresolved("nowhere")]
#[case::non_root("pypi/extra")]
#[tokio::test]
async fn test_upload_to_an_unresolvable_or_non_root_path_is_rejected(#[case] path: &str) {
    let h = harness().await;
    let request = axum::http::Request::builder()
        .header("content-type", "multipart/form-data; boundary=x")
        .body(axum::body::Body::from("--x--\r\n"))
        .unwrap();
    assert_eq!(
        crate::serving::PypiServing
            .post(h.state.serving.clone(), path.to_owned(), request)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}
use crate::tests::http::placement_harness;
