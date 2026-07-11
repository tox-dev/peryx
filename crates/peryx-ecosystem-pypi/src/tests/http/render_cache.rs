//! The rendered-page cache and what retires an entry from it.

use super::support::*;

/// The HTML page and the legacy JSON API are rendered from the stored page, so both are cached under
/// their own key. These pin the two things that keeps honest: the render is served again, and it stops
/// being served the moment anything the page depends on changes.
#[tokio::test]
async fn test_html_page_is_rendered_once_and_then_served_from_cache() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, Digest::of(b"wheel").as_str(), &file_url, None).await;

    let (status, _, first) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    // moka applies a write lazily, so a read straight after an insert may not see it yet.
    h.state.cache.hot.run_pending_tasks();
    assert!(
        h.state
            .hot_fresh(&h.state.hot_key("pypi", "flask", crate::cache::SIMPLE_HTML))
            .is_some()
    );

    // Serving the second one from the cache must not change a byte of it.
    let (_, _, second) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(first, second);
}
#[tokio::test]
async fn test_a_mutation_retires_the_cached_html_render() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, Digest::of(b"wheel-v1").as_str(), &file_url, None).await;
    let (_, _, before) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert!(before.contains(Digest::of(b"wheel-v1").as_str()));

    // Whatever the mutation was, it bumped the epoch the key carries. The next request may not answer
    // with a page rendered before it.
    let record = h.state.meta.get_index("pypi/flask").unwrap().unwrap();
    let body = detail_json(Digest::of(b"wheel-v2").as_str(), &file_url);
    h.state
        .meta
        .put_index(
            "pypi/flask",
            &CachedIndex {
                body: body.into_bytes(),
                ..record
            },
        )
        .unwrap();
    h.state.bump_epoch();

    let (_, _, after) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert!(after.contains(Digest::of(b"wheel-v2").as_str()), "{after}");
}
#[tokio::test]
async fn test_a_policy_filtered_page_still_serves_json() {
    // An active policy sends the JSON page down the buffered path instead of the streaming one, since
    // the stream cannot filter. That path renders the JSON itself.
    let mirror_policy = policy(|neutral, _pypi| {
        neutral.max_file_size_bytes = Some(1);
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
        neutral.max_file_size_bytes = Some(1);
    });
    let h = harness_with_policies(true, true, mirror_policy, Policy::default(), Policy::default()).await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, Digest::of(b"wheel").as_str(), &file_url, None).await;

    let (status, _, _) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    h.state.cache.hot.run_pending_tasks();
    // The bytes a policy filtered are not the bytes the page renders to, and the key does not say
    // which policy produced them.
    assert!(
        h.state
            .hot_fresh(&h.state.hot_key("pypi", "flask", crate::cache::SIMPLE_HTML))
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

    // The upstream is gone; the render is cached, so the page still answers.
    h.server.reset().await;
    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(first.as_str()));

    // Past the freshness of the page it was rendered from, the render must not answer for it.
    let second = Digest::of(b"wheel-v2");
    mount_detail(&h.server, second.as_str(), &file_url, None).await;
    h.clock.fetch_add(61, Ordering::Relaxed);
    let (_, _, body) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;
    assert!(body.contains(second.as_str()), "a stale render outlived its page");
}
