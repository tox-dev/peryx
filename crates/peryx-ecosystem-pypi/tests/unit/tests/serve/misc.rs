//! Cross-cutting serving behavior.

use super::support::*;
use peryx_driver::serving::ArtifactPathDriver as _;
use peryx_driver::serving::EcosystemDriver as _;
use peryx_driver::serving::ProjectPageDriver as _;
use rstest::rstest;

#[tokio::test]
async fn test_negative_cache_expires_by_clock() {
    let h = harness().await;

    h.state.remember_negative("missing".to_owned(), 30);
    assert!(h.state.negative_fresh("missing"));
    h.clock.fetch_add(31, Ordering::Relaxed);

    assert!(!h.state.negative_fresh("missing"));
    assert!(!h.state.negative_fresh("missing"));
}
#[tokio::test]
async fn test_gate_waiter_finds_the_hot_entry_after_a_revalidation() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = ResponseTemplate::new(200).insert_header("etag", "\"v1\"");
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(page.set_body_raw(
            detail_json(digest.as_str(), &file_url).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    // Past the stale bound the page no longer serves stale-while-revalidate, so both racers take the
    // gate and revalidate; a 304 refills the hot cache without an epoch bump, so the gate waiter's
    // post-gate hot check hits.
    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(304).set_delay(std::time::Duration::from_millis(150)))
        .mount(&h.server)
        .await;
    h.clock.fetch_add(100_000, Ordering::Relaxed);
    let (a, b) = tokio::join!(
        get(&h.state, "/pypi/simple/flask/", Some("application/json")),
        get(&h.state, "/pypi/simple/flask/", Some("application/json")),
    );
    assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
    assert_eq!(a.2, b.2);
}
#[tokio::test]
async fn test_corrupt_cached_page_falls_back_and_fails_loudly() {
    let h = harness().await;
    h.state
        .meta
        .put_index("pypi/flask", &fresh_record(br#"{"files":[{"bad": }]}"#))
        .unwrap();
    let (status, ..) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_legacy_cached_record_registers_nothing() {
    let h = harness().await;
    let body = br#"{"meta":{"api-version":"1.1"},"name":"flask","versions":["1.0"],
        "files":[{"filename":"flask-1.0-py3-none-any.whl",
        "url":"/pypi/files/aaaa/flask-1.0-py3-none-any.whl","hashes":{"sha256":"aaaa"}}]}"#;
    cache::test_support::persist_page(&h.state, "pypi/flask", "pypi", "flask", &fresh_record(body)).unwrap();
    assert!(h.state.meta.get_file_url("aaaa").unwrap().is_none());
}
#[tokio::test]
async fn test_broken_upstream_transfer_forwards_the_error() {
    let h = harness().await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        if let Ok((mut socket, _)) = listener.accept() {
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer);
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort");
        }
    });
    let digest = Digest::of(b"never arrives");
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("http://{addr}/x.whl"), "pypi")
        .unwrap();
    let outcome = cache::stream_file(
        h.state.serving.clone(),
        digest.clone(),
        "pypi".to_owned(),
        "x.whl".to_owned(),
    )
    .await
    .unwrap();
    let cache::FileOutcome::Live(mut stream) = outcome else {
        panic!("expected a live stream");
    };
    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        saw_error |= item.is_err();
    }
    assert!(saw_error);
    assert!(h.state.blobs.head(&digest).await.unwrap().is_none());
}
#[tokio::test]
async fn test_buffered_fetch_registers_metadata_siblings() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let meta_digest = Digest::of(b"meta");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}},\"core-metadata\":{{\"sha256\":\"{meta}\"}}}}]}}",
        digest = digest.as_str(),
        meta = meta_digest.as_str(),
    );
    mount_json_page(&h.server, &page).await;
    // An HTML request takes the buffered path, whose persistence parses the raw page.
    let (status, ..) = get(&h.state, "/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::OK);
    let (url, meta_sha, _source) = h
        .state
        .meta
        .get_metadata(digest.as_str())
        .unwrap()
        .expect("metadata sibling registered");
    assert_eq!(url, format!("{file_url}.metadata"));
    assert_eq!(meta_sha, meta_digest.as_str());
}

/// A Simple-API detail whose one file carries a PEP 658 metadata sibling, so the web page builder
/// walks into `metadata_for`. `wheel` is the wheel's advertised sha256 (pass an invalid string to
/// exercise the digest-rejection path), `meta` the sibling's sha256.
fn detail_with_metadata(wheel: &str, url: &str, meta: &str) -> String {
    format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{url}\",\
         \"hashes\":{{\"sha256\":\"{wheel}\"}},\"core-metadata\":{{\"sha256\":\"{meta}\"}}}}]}}"
    )
}

#[tokio::test]
async fn test_artifact_path_rejects_an_invalid_digest() {
    let h = harness().await;
    let err = crate::serving::PypiServing
        .artifact_path_in_project(
            h.state.serving.clone(),
            0,
            "flask".to_owned(),
            "not-hex".to_owned(),
            "flask.whl".to_owned(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("invalid sha256 digest"), "{err}");
}

#[tokio::test]
async fn test_artifact_path_rejects_a_digest_that_is_not_a_project_member() {
    let h = harness().await;
    let listed = Digest::of(b"listed wheel");
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{}/files/flask.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}}}}]}}",
        h.server.uri(),
        listed.as_str(),
    );
    mount_json_page(&h.server, &page).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let foreign = Digest::of(b"another project's wheel");
    let err = crate::serving::PypiServing
        .artifact_path_in_project(
            h.state.serving.clone(),
            0,
            "flask".to_owned(),
            foreign.as_str().to_owned(),
            "flask-1.0-py3-none-any.whl".to_owned(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("is not a member of project"), "{err}");
}

#[tokio::test]
async fn test_artifact_path_reports_an_unfetchable_member_file() {
    let h = harness().await;
    let digest = Digest::of(b"never stored");
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{}/files/flask.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}}}}]}}",
        h.server.uri(),
        digest.as_str(),
    );
    mount_json_page(&h.server, &page).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let err = crate::serving::PypiServing
        .artifact_path_in_project(
            h.state.serving.clone(),
            0,
            "flask".to_owned(),
            digest.as_str().to_owned(),
            "flask-1.0-py3-none-any.whl".to_owned(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("artifact on index"), "{err}");
}

#[tokio::test]
async fn test_artifact_path_reports_a_project_detail_error() {
    let h = harness().await;
    h.state
        .meta
        .put_index("pypi/flask", &fresh_record(br#"{"files":[{"bad": }]}"#))
        .unwrap();
    let digest = Digest::of(b"wheel");
    let err = crate::serving::PypiServing
        .artifact_path_in_project(
            h.state.serving.clone(),
            0,
            "flask".to_owned(),
            digest.as_str().to_owned(),
            "flask-1.0-py3-none-any.whl".to_owned(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("for project"), "{err}");
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
    let (_, meta) = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meta.version.as_deref(), Some(expected));
}

/// A Simple-API detail listing one wheel per `(version, yanked)` pair, so the project page sees
/// which releases keep an active file.
fn detail_with_yanks(versions: &[&str], files: &[(&str, bool)]) -> String {
    let files = files
        .iter()
        .map(|(version, yanked)| {
            serde_json::json!({
                "filename": format!("flask-{version}-py3-none-any.whl"),
                "url": format!("/files/flask-{version}-py3-none-any.whl"),
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
    let (_, meta) = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meta.version.as_deref(), Some(expected));
}

/// The wheel of the `index`th file of `version`. The build tag keeps a release's wheels apart.
fn release_wheel(index: usize, version: &str) -> String {
    format!("flask-{version}-{index}-py3-none-any.whl")
}

/// The PEP 658 sibling of [`release_wheel`], summarizing itself so the summary the page renders names
/// the file its metadata came from.
fn release_metadata(index: usize, version: &str) -> String {
    format!(
        "Metadata-Version: 2.1\nName: flask\nVersion: {version}\nSummary: {}\n",
        release_wheel(index, version)
    )
}

/// A Simple-API detail whose files are `(version, yanked, has a metadata sibling)`, listed in the
/// order upstream serves them.
fn detail_with_release_metadata(server: &MockServer, versions: &[&str], files: &[(&str, bool, bool)]) -> String {
    let files = files
        .iter()
        .enumerate()
        .map(|(index, (version, yanked, sibling))| {
            let wheel = release_wheel(index, version);
            let mut file = serde_json::json!({
                "filename": wheel,
                "url": format!("{}/files/{wheel}", server.uri()),
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
    // The cached page registers the siblings; their blobs then answer the metadata read locally.
    get(&h.state, "/pypi/simple/flask/", None).await;
    for (index, (release, ..)) in files.iter().enumerate() {
        h.state
            .blobs
            .put_bytes(release_metadata(index, release).as_bytes())
            .await
            .unwrap();
    }
    let (_, meta) = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meta.version.as_deref(), Some(version));
    assert_eq!(meta.summary.as_deref(), summary);
}

#[tokio::test]
async fn test_project_page_surfaces_a_resolve_error() {
    let h = harness().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    let err = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap_err();
    assert!(err.contains("project detail on index"), "{err}");
}

#[tokio::test]
async fn test_project_page_rejects_a_bad_metadata_wheel_digest() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(&h.server, &detail_with_metadata("not-a-digest", &file_url, "also-bad")).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let err = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap_err();
    assert!(err.contains("invalid sha256 digest"), "{err}");
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
    // The sibling is never made fetchable, so metadata_for fails on the fetch.
    let err = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap_err();
    assert!(err.contains("metadata fetch on index"), "{err}");
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

    let err = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap_err();

    assert!(
        err.contains("metadata parse on index \"pypi\" for file \"flask-1.0-py3-none-any.whl\": header line"),
        "{err}"
    );
}

#[tokio::test]
async fn test_project_page_is_absent_for_an_unknown_hosted_project() {
    let h = harness().await;
    // The hosted store holds no uploads for `flask`, so its page resolves to nothing.
    let page = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 1, "flask".to_owned())
        .await
        .unwrap();
    assert!(page.is_none());
}

#[tokio::test]
async fn test_project_page_reads_cached_and_remote_only_placements() {
    use peryx_core::{UiArtifactSource, UiByteAvailability};
    use peryx_storage::meta::ArtifactSource;

    let h = harness().await;
    let cached = Digest::of(b"cached wheel bytes");
    // A proxied artifact whose bytes were fetched and verified projects as local; the second file
    // has no placement row, so it stays the default proxied remote-only catalog entry.
    h.state
        .meta
        .record_artifact_placement(cached.as_str(), ArtifactSource::Proxy, true)
        .unwrap();
    let remote = Digest::of(b"a wheel this proxy has never downloaded");
    let page = crate::to_json(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "flask",
        "versions": ["1.0", "2.0"],
        "files": [
            {
                "filename": "flask-1.0-py3-none-any.whl",
                "url": "https://files.example/flask-1.0-py3-none-any.whl",
                "hashes": {"sha256": cached.as_str()},
            },
            {
                "filename": "flask-2.0-py3-none-any.whl",
                "url": "https://files.example/flask-2.0-py3-none-any.whl",
                "hashes": {"sha256": remote.as_str()},
            },
        ],
    }));
    mount_json_page(&h.server, &page).await;

    let (project, _) = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
        .unwrap();

    let file = |filename: &str| {
        project
            .files
            .iter()
            .find(|file| file.filename == filename)
            .unwrap_or_else(|| panic!("no file {filename:?}"))
    };
    assert_eq!(file("flask-1.0-py3-none-any.whl").source, UiArtifactSource::Proxy);
    assert_eq!(
        file("flask-1.0-py3-none-any.whl").availability,
        UiByteAvailability::Local
    );
    assert_eq!(file("flask-2.0-py3-none-any.whl").source, UiArtifactSource::Proxy);
    assert_eq!(
        file("flask-2.0-py3-none-any.whl").availability,
        UiByteAvailability::RemoteOnly
    );
}

#[tokio::test]
async fn test_project_page_maps_each_placement_source_and_availability() {
    use peryx_core::{UiArtifactSource, UiByteAvailability};
    use peryx_storage::meta::ArtifactSource;

    let h = harness().await;
    let generated = Digest::of(b"a generated sibling");
    let remote = Digest::of(b"a proxied catalog entry");
    let orphan = Digest::of(b"a hosted digest with lost bytes");
    for (digest, source, present) in [
        (&generated, ArtifactSource::Generated, true),
        (&remote, ArtifactSource::Proxy, false),
        (&orphan, ArtifactSource::Hosted, false),
    ] {
        h.state
            .meta
            .record_artifact_placement(digest.as_str(), source, present)
            .unwrap();
    }
    let page = crate::to_json(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": "flask",
        "versions": ["1.0"],
        "files": [
            {"filename": "flask-1.0.tar.gz", "url": "https://files.example/flask-1.0.tar.gz", "hashes": {"sha256": generated.as_str()}},
            {"filename": "flask-1.0-py3-none-any.whl", "url": "https://files.example/flask-1.0-py3-none-any.whl", "hashes": {"sha256": remote.as_str()}},
            {"filename": "flask-1.0-py2-none-any.whl", "url": "https://files.example/flask-1.0-py2-none-any.whl", "hashes": {"sha256": orphan.as_str()}},
        ],
    }));
    mount_json_page(&h.server, &page).await;

    let (project, _) = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
        .unwrap();

    let file = |filename: &str| {
        project
            .files
            .iter()
            .find(|file| file.filename == filename)
            .unwrap_or_else(|| panic!("no file {filename:?}"))
    };
    assert_eq!(file("flask-1.0.tar.gz").source, UiArtifactSource::Generated);
    assert_eq!(file("flask-1.0.tar.gz").availability, UiByteAvailability::Local);
    assert_eq!(file("flask-1.0-py3-none-any.whl").source, UiArtifactSource::Proxy);
    assert_eq!(
        file("flask-1.0-py3-none-any.whl").availability,
        UiByteAvailability::RemoteOnly
    );
    assert_eq!(file("flask-1.0-py2-none-any.whl").source, UiArtifactSource::Hosted);
    assert_eq!(
        file("flask-1.0-py2-none-any.whl").availability,
        UiByteAvailability::Unavailable
    );
}

#[tokio::test]
async fn test_project_page_reads_a_hosted_upload_with_lost_bytes_as_unavailable() {
    use peryx_core::{UiArtifactSource, UiByteAvailability};
    use peryx_storage::meta::ArtifactSource;

    let h = harness().await;
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
        .meta
        .put_upload("hosted", "flask", filename, crate::to_json(&uploaded).as_bytes())
        .unwrap();
    // A hosted-source placement whose bytes were evicted marks the upload unavailable.
    h.state
        .meta
        .record_artifact_placement(digest.as_str(), ArtifactSource::Hosted, false)
        .unwrap();

    let (project, _) = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 2, "flask".to_owned())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(project.files.len(), 1);
    assert_eq!(project.files[0].source, UiArtifactSource::Hosted);
    assert_eq!(project.files[0].availability, UiByteAvailability::Unavailable);
}

#[tokio::test]
async fn test_project_page_marks_a_hosted_upload_over_its_cached_blob() {
    use peryx_core::{UiArtifactSource, UiByteAvailability};

    use peryx_storage::meta::ArtifactSource;

    let h = harness().await;
    let filename = "flask-1.0-py3-none-any.whl";
    // A same-named upstream file could carry a proxied placement; hosted-layer membership must win
    // and read the upload as hosted and locally served.
    let digest = h.state.blobs.put_bytes(b"hosted wheel bytes").await.unwrap();
    // A stale proxied placement on the shared digest must not override the hosted-layer verdict.
    h.state
        .meta
        .record_artifact_placement(digest.as_str(), ArtifactSource::Proxy, false)
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
        .meta
        .put_upload("hosted", "flask", filename, crate::to_json(&uploaded).as_bytes())
        .unwrap();

    // Index 2 is the virtual index layering the hosted store in front of the cache, so classification
    // walks its layers to find the upload.
    let (project, _) = crate::serving::PypiServing
        .project_page(h.state.serving.clone(), 2, "flask".to_owned())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(project.files.len(), 1);
    assert_eq!(project.files[0].source, UiArtifactSource::Hosted);
    assert_eq!(project.files[0].availability, UiByteAvailability::Local);
}

#[tokio::test]
async fn test_upload_to_an_unresolvable_or_non_root_path_is_rejected() {
    use axum::extract::FromRequest as _;

    async fn empty_multipart() -> axum::extract::Multipart {
        let request = axum::http::Request::builder()
            .header("content-type", "multipart/form-data; boundary=x")
            .body(axum::body::Body::from("--x--\r\n"))
            .unwrap();
        axum::extract::Multipart::from_request(request, &()).await.unwrap()
    }

    let h = harness().await;
    // A path under no configured index resolves to nothing.
    let unresolved = crate::serving::PypiServing
        .post(
            h.state.serving.clone(),
            "nowhere".to_owned(),
            axum::http::HeaderMap::new(),
            empty_multipart().await,
        )
        .await;
    assert_eq!(unresolved.status(), StatusCode::NOT_FOUND);
    // A path that resolves but carries a remainder must target the index root.
    let non_root = crate::serving::PypiServing
        .post(
            h.state.serving.clone(),
            "pypi/extra".to_owned(),
            axum::http::HeaderMap::new(),
            empty_multipart().await,
        )
        .await;
    assert_eq!(non_root.status(), StatusCode::NOT_FOUND);
}
