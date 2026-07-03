use std::sync::Arc;
use std::sync::atomic::Ordering;

use velodex_storage::blob::Digest;
use wiremock::matchers::{header as match_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::http_tests::{detail_json, get, harness};
use super::{LogCapture, field};
use crate::cache::refresh_stale_pages;

async fn mount_page(server: &MockServer, body: String, template: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(template.set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(server)
        .await;
}

fn drilled(state: &Arc<crate::state::AppState>, field: &str) -> u64 {
    state.metrics.drill(Some("pypi"), None)["totals"][field]
        .as_u64()
        .unwrap_or(0)
}

fn settle(state: &Arc<crate::state::AppState>, field: &str, want: u64) {
    for _ in 0..500 {
        if drilled(state, field) >= want {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("metric {field} never reached {want}");
}

#[tokio::test]
async fn test_refresh_sweep_detects_changed_page() {
    let h = harness().await;
    let digest = Digest::of(b"wheel-v1");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_page(
        &h.server,
        detail_json(digest.as_str(), &file_url),
        ResponseTemplate::new(200),
    )
    .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    h.server.reset().await;
    let new_digest = Digest::of(b"wheel-v2");
    mount_page(
        &h.server,
        detail_json(new_digest.as_str(), &file_url),
        ResponseTemplate::new(200),
    )
    .await;
    h.clock.fetch_add(61, Ordering::Relaxed);

    let summary = refresh_stale_pages(&h.state).await.unwrap();
    assert_eq!((summary.checked, summary.changed), (1, 1));
    settle(&h.state, "changed", 1);
    assert!(drilled(&h.state, "refreshes") >= 1);

    // The refreshed page serves without another upstream fetch.
    h.server.reset().await;
    let (_, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(body.contains(new_digest.as_str()));
}

#[tokio::test(flavor = "current_thread")]
async fn test_refresh_sweep_logs_mirror_sync_event() {
    let h = harness().await;
    let digest = Digest::of(b"wheel-v1");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_page(
        &h.server,
        detail_json(digest.as_str(), &file_url),
        ResponseTemplate::new(200),
    )
    .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    h.server.reset().await;
    let new_digest = Digest::of(b"wheel-v2");
    mount_page(
        &h.server,
        detail_json(new_digest.as_str(), &file_url),
        ResponseTemplate::new(200),
    )
    .await;
    h.clock.fetch_add(61, Ordering::Relaxed);
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(refresh_stale_pages(&h.state).await.unwrap().changed, 1);

    drop(guard);
    let events = logs.security_events();
    let sync = events
        .iter()
        .find(|event| field(event, "action") == Some("mirror_sync") && field(event, "result") == Some("success"))
        .unwrap();
    assert_eq!(field(sync, "repository"), Some("pypi"));
    assert_eq!(field(sync, "project"), Some("flask"));
    assert_eq!(sync["fields"]["changed"], true);
    assert_eq!(sync["fields"]["count"], 1);
}

#[tokio::test(flavor = "current_thread")]
async fn test_refresh_sweep_logs_mirror_sync_not_found() {
    let h = harness().await;
    let digest = Digest::of(b"wheel-v1");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_page(
        &h.server,
        detail_json(digest.as_str(), &file_url),
        ResponseTemplate::new(200),
    )
    .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    h.server.reset().await;
    mount_page(&h.server, "{}".to_owned(), ResponseTemplate::new(404)).await;
    h.clock.fetch_add(61, Ordering::Relaxed);
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(refresh_stale_pages(&h.state).await.unwrap().checked, 1);

    drop(guard);
    let events = logs.security_events();
    let sync = events
        .iter()
        .find(|event| field(event, "action") == Some("mirror_sync") && field(event, "result") == Some("noop"))
        .unwrap();
    assert_eq!(field(sync, "repository"), Some("pypi"));
    assert_eq!(field(sync, "project"), Some("flask"));
    assert_eq!(field(sync, "reason"), Some("project not found upstream"));
    assert_eq!(sync["fields"]["changed"], false);
}

#[tokio::test(flavor = "current_thread")]
async fn test_refresh_sweep_logs_mirror_sync_failure() {
    let h = harness().await;
    let digest = Digest::of(b"wheel-v1");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_page(
        &h.server,
        detail_json(digest.as_str(), &file_url),
        ResponseTemplate::new(200),
    )
    .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    h.server.reset().await;
    mount_page(&h.server, "invalid".to_owned(), ResponseTemplate::new(200)).await;
    h.clock.fetch_add(61, Ordering::Relaxed);
    let logs = LogCapture::default();
    let guard = logs.install();

    let err = refresh_stale_pages(&h.state).await.unwrap_err();

    drop(guard);
    assert!(
        err.user_message()
            .starts_with("simple API document could not be parsed")
    );
    let events = logs.security_events();
    let sync = events
        .iter()
        .find(|event| field(event, "action") == Some("mirror_sync") && field(event, "result") == Some("failure"))
        .unwrap();
    assert_eq!(field(sync, "repository"), Some("pypi"));
    assert_eq!(field(sync, "project"), Some("flask"));
    assert!(field(sync, "reason").is_some_and(|reason| reason.starts_with("simple API document could not be parsed")));
    assert_eq!(sync["fields"]["changed"], false);
}

#[tokio::test]
async fn test_refresh_sweep_revalidates_unchanged_via_etag() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = ResponseTemplate::new(200).insert_header("etag", "\"v1\"");
    mount_page(&h.server, detail_json(digest.as_str(), &file_url), page).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(match_header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&h.server)
        .await;
    h.clock.fetch_add(61, Ordering::Relaxed);

    let summary = refresh_stale_pages(&h.state).await.unwrap();
    assert_eq!((summary.checked, summary.changed), (1, 0));
    settle(&h.state, "refreshes", 1);
    assert_eq!(drilled(&h.state, "changed"), 0);
}

#[tokio::test]
async fn test_refresh_sweep_skips_fresh_pages() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_page(
        &h.server,
        detail_json(digest.as_str(), &file_url),
        ResponseTemplate::new(200),
    )
    .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let summary = refresh_stale_pages(&h.state).await.unwrap();
    assert_eq!(summary.checked, 0);
}

#[tokio::test]
async fn test_upstream_max_age_shortens_freshness() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    // Upstream grants 5 seconds; the configured fallback is 60.
    let page = ResponseTemplate::new(200).insert_header("cache-control", "public, max-age=5");
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(page.set_body_raw(
            detail_json(digest.as_str(), &file_url).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(2)
        .mount(&h.server)
        .await;

    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    h.clock.fetch_add(6, Ordering::Relaxed);
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
}

#[tokio::test]
async fn test_no_cache_header_falls_back_to_configured_ttl() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = ResponseTemplate::new(200).insert_header("cache-control", "no-cache");
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(page.set_body_raw(
            detail_json(digest.as_str(), &file_url).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(1)
        .mount(&h.server)
        .await;

    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    // Well within the 60 second fallback: served from cache, no second upstream fetch.
    h.clock.fetch_add(6, Ordering::Relaxed);
    let (_, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(body.contains(digest.as_str()));
}

#[tokio::test]
async fn test_stale_serve_records_metric() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_page(
        &h.server,
        detail_json(digest.as_str(), &file_url),
        ResponseTemplate::new(200),
    )
    .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    h.clock.fetch_add(61, Ordering::Relaxed);

    let (_, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(body.contains(digest.as_str()));
    settle(&h.state, "stale_served", 1);
}

#[tokio::test]
async fn test_refresh_skips_keys_without_a_mirror() {
    let h = harness().await;
    let record = velodex_storage::meta::CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 0,
        content_type: None,
        fresh_secs: None,
        body: b"{}".to_vec(),
    };
    h.state.meta.put_index("ghost/thing", &record).unwrap();
    h.clock.fetch_add(3600, Ordering::Relaxed);
    let summary = refresh_stale_pages(&h.state).await.unwrap();
    assert_eq!(summary.checked, 0);
}

#[tokio::test]
async fn test_refresh_sweep_full_fetch_with_identical_body_is_unchanged() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    // No ETag anywhere: the sweep refetches the whole page and compares bodies.
    mount_page(
        &h.server,
        detail_json(digest.as_str(), &file_url),
        ResponseTemplate::new(200),
    )
    .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    h.clock.fetch_add(61, Ordering::Relaxed);
    let summary = refresh_stale_pages(&h.state).await.unwrap();
    assert_eq!((summary.checked, summary.changed), (1, 0));
}
