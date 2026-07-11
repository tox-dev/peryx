//! Cross-cutting serving behavior.

use super::support::*;

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

    // Past freshness: both racers revalidate; a 304 refills the hot cache without an epoch bump,
    // so the gate waiter's post-gate hot check hits.
    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(304).set_delay(std::time::Duration::from_millis(150)))
        .mount(&h.server)
        .await;
    h.clock.fetch_add(61, Ordering::Relaxed);
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
    cache::persist_page(&h.state, "pypi/flask", "pypi", "flask", &fresh_record(body)).unwrap();
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
    assert!(!h.state.blobs.exists(&digest));
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
    use peryx_driver::serving::EcosystemDriver as _;

    let h = harness().await;
    let err = crate::serving::PypiServing
        .artifact_path(h.state.serving.clone(), 0, "not-hex".to_owned(), "flask.whl".to_owned())
        .await
        .unwrap_err();
    assert!(err.contains("invalid sha256 digest"), "{err}");
}

#[tokio::test]
async fn test_artifact_path_reports_an_unfetchable_file() {
    use peryx_driver::serving::EcosystemDriver as _;

    let h = harness().await;
    let digest = Digest::of(b"never stored");
    let err = crate::serving::PypiServing
        .artifact_path(
            h.state.serving.clone(),
            0,
            digest.as_str().to_owned(),
            "flask.whl".to_owned(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("artifact on index"), "{err}");
}

#[tokio::test]
async fn test_project_page_surfaces_a_resolve_error() {
    use peryx_driver::serving::EcosystemDriver as _;

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
    use peryx_driver::serving::EcosystemDriver as _;

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
    use peryx_driver::serving::EcosystemDriver as _;

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
async fn test_upload_to_an_unresolvable_or_non_root_path_is_rejected() {
    use axum::extract::FromRequest as _;
    use peryx_driver::serving::EcosystemDriver as _;

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
