use super::support::*;

#[tokio::test]
async fn test_html_page_is_rendered_once_and_then_served_from_cache() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, Digest::of(b"wheel").as_str(), &file_url, None).await;

    let (status, _, first) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    // Moka defers write visibility until pending tasks run.
    h.state.serving.cache.hot.run_pending_tasks();
    assert!(
        h.state
            .serving
            .hot_fresh(
                &h.state
                    .serving
                    .representation_key("pypi", "flask", crate::cache::SIMPLE_HTML)
            )
            .is_some()
    );

    let (_, _, second) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(first, second);
}
#[rstest]
#[case::simple_html("/pypi/simple/flask/", "text/html")]
#[case::legacy_json("/pypi/flask/json", "application/json")]
#[tokio::test]
async fn test_revocation_bypasses_a_preexisting_render(#[case] uri: &str, #[case] accept: &str) {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    let (_, _, before) = get(&h.state, uri, Some(accept)).await;
    assert!(before.contains(digest.as_str()));
    revoke_digest(&h.state, &digest);

    let (_, _, after) = get(&h.state, uri, Some(accept)).await;

    assert!(!after.contains(digest.as_str()), "{after}");
}
#[rstest]
#[case::simple_html("/pypi/simple/flask/", "text/html")]
#[case::legacy_json("/pypi/flask/json", "application/json")]
#[tokio::test]
async fn test_revocation_filtered_render_is_not_cached_after_lift(#[case] uri: &str, #[case] accept: &str) {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    revoke_digest(&h.state, &digest);
    let (_, _, blocked) = get(&h.state, uri, Some(accept)).await;
    assert!(!blocked.contains(digest.as_str()), "{blocked}");

    lift_digest(&h.state, &digest);
    let (_, _, restored) = get(&h.state, uri, Some(accept)).await;

    assert!(restored.contains(digest.as_str()), "{restored}");
}
#[tokio::test]
async fn test_a_mutation_retires_the_cached_html_render() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, Digest::of(b"wheel-v1").as_str(), &file_url, None).await;
    let (_, _, before) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert!(before.contains(Digest::of(b"wheel-v1").as_str()));

    let record = h.state.serving.meta.get_index("pypi/flask").unwrap().unwrap();
    let body = detail_json(Digest::of(b"wheel-v2").as_str(), &file_url);
    h.state
        .serving
        .meta
        .put_index(
            "pypi/flask",
            &CachedIndex {
                body: body.into_bytes(),
                ..record
            },
        )
        .unwrap();
    h.state.serving.invalidate_resource("flask");

    let (_, _, after) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert!(after.contains(Digest::of(b"wheel-v2").as_str()), "{after}");
}
#[tokio::test]
async fn test_a_mutation_spares_other_projects_cached_renders() {
    let h = harness().await;
    let page = bytes::Bytes::from_static(b"render");
    h.state.serving.cache.store_hot(
        h.state
            .serving
            .representation_key("pypi", "flask", crate::cache::SIMPLE_HTML),
        page.clone(),
        2000,
    );
    h.state.serving.cache.store_hot(
        h.state
            .serving
            .representation_key("pypi", "django", crate::cache::SIMPLE_HTML),
        page.clone(),
        2000,
    );
    h.state.serving.cache.hot.run_pending_tasks();

    h.state.serving.invalidate_resource("flask");

    assert!(
        h.state
            .serving
            .hot_fresh(
                &h.state
                    .serving
                    .representation_key("pypi", "flask", crate::cache::SIMPLE_HTML)
            )
            .is_none()
    );
    assert_eq!(
        h.state.serving.hot_fresh(
            &h.state
                .serving
                .representation_key("pypi", "django", crate::cache::SIMPLE_HTML)
        ),
        Some(page)
    );
}
#[tokio::test]
async fn test_a_policy_filtered_page_still_serves_json() {
    // An active policy sends the JSON page down the buffered path instead of the streaming one, since
    // the stream cannot filter. That path renders the JSON itself.
    let mirror_policy = policy(|neutral, _pypi| {
        neutral.max_artifact_size_bytes = Some(1);
    });
    let h = harness_with_policies(true, true, mirror_policy, Policy::default(), Policy::default()).await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, Digest::of(b"wheel").as_str(), &file_url, None).await;

    let (status, headers, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.pypi.simple.v1+json");
    assert!(body.contains("flask"));
}
#[tokio::test]
async fn test_a_policy_filtered_page_is_never_cached_as_a_render() {
    let mirror_policy = policy(|neutral, _pypi| {
        neutral.max_artifact_size_bytes = Some(1);
    });
    let h = harness_with_policies(true, true, mirror_policy, Policy::default(), Policy::default()).await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, Digest::of(b"wheel").as_str(), &file_url, None).await;

    let (status, _, _) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    h.state.serving.cache.hot.run_pending_tasks();

    assert!(
        h.state
            .serving
            .hot_fresh(
                &h.state
                    .serving
                    .representation_key("pypi", "flask", crate::cache::SIMPLE_HTML)
            )
            .is_none()
    );
}
#[tokio::test]
async fn test_html_page_is_cached_then_expires_with_the_page_it_renders() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let first = Digest::of(b"wheel-v1");
    mount_detail(&h.server, first.as_str(), &file_url, None).await;
    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(first.as_str()));

    h.server.reset().await;
    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(first.as_str()));

    let second = Digest::of(b"wheel-v2");
    mount_detail(&h.server, second.as_str(), &file_url, None).await;
    h.clock.fetch_add(61, Ordering::Relaxed);
    let (_, _, body) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert!(body.contains(second.as_str()), "a stale render outlived its page");
}
