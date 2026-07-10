use std::sync::Arc;
use std::sync::atomic::Ordering;

use peryx_storage::blob::Digest;
use peryx_storage::meta::CachedIndex;
use wiremock::matchers::{header as match_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::http_tests::{detail_json, get, harness, harness_with_policies};
use super::{LogCapture, field};
use crate::cache::refresh_stale_pages;
use peryx_policy::{Policy, PolicyConfig};

async fn mount_page(server: &MockServer, body: String, template: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(template.set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(server)
        .await;
}

fn drilled(state: &Arc<peryx_driver::state::AppState>, field: &str) -> u64 {
    let totals = state.metrics.drill(Some("pypi"), None);
    ["base", "cached", "hosted", "ecosystem"]
        .into_iter()
        .find_map(|group| totals["totals"][group][field].as_u64())
        .unwrap_or(0)
}

fn settle(state: &Arc<peryx_driver::state::AppState>, field: &str, want: u64) {
    for _ in 0..500 {
        if drilled(state, field) >= want {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("metric {field} never reached {want}");
}

fn policy(configure: impl FnOnce(&mut PolicyConfig)) -> Policy {
    let mut config = PolicyConfig::default();
    configure(&mut config);
    Policy::compile(&config)
}

#[tokio::test]
async fn test_upstream_max_age_cannot_outlive_the_configured_ttl() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let first = Digest::of(b"wheel-v1");
    mount_page(
        &h.server,
        detail_json(first.as_str(), &file_url),
        ResponseTemplate::new(200).insert_header("cache-control", "public, max-age=31536000"),
    )
    .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    h.server.reset().await;
    let second = Digest::of(b"wheel-v2");
    mount_page(
        &h.server,
        detail_json(second.as_str(), &file_url),
        ResponseTemplate::new(200),
    )
    .await;
    h.clock.fetch_add(61, Ordering::Relaxed);

    // The upstream granted a year. The configured ttl is 60s, and it is the ceiling: the sweep must
    // still find this page stale and revalidate it.
    let summary = refresh_stale_pages(&h.state).await.unwrap();
    assert_eq!((summary.checked, summary.changed), (1, 1));
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

#[tokio::test]
async fn test_serving_refresh_stale_reports_the_sweep() {
    use peryx_driver::serving::EcosystemServing as _;

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

    let sweep = crate::serving::PypiServing
        .refresh_stale(h.state.clone())
        .await
        .unwrap();
    assert_eq!((sweep.checked, sweep.changed), (1, 1));
}

#[tokio::test]
async fn test_serving_refresh_stale_surfaces_errors_as_strings() {
    use peryx_driver::serving::EcosystemServing as _;

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
    mount_page(&h.server, "invalid".to_owned(), ResponseTemplate::new(200)).await;
    h.clock.fetch_add(61, Ordering::Relaxed);

    let err = crate::serving::PypiServing
        .refresh_stale(h.state.clone())
        .await
        .unwrap_err();
    assert!(err.contains("simple API document could not be parsed"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_refresh_sweep_skips_policy_denied_project() {
    let mirror_policy = policy(|policy| {
        policy.block_projects = vec!["flask".to_owned()];
    });
    let h = harness_with_policies(true, true, mirror_policy, Policy::default(), Policy::default()).await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    h.state
        .meta
        .put_index(
            "pypi/flask",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 0,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: Some(1),
                body: detail_json(digest.as_str(), &file_url).into_bytes(),
            },
        )
        .unwrap();
    let logs = LogCapture::default();
    let guard = logs.install();

    let summary = refresh_stale_pages(&h.state).await.unwrap();

    drop(guard);
    assert_eq!(summary, crate::cache::RefreshSummary::default());
    let events = logs.security_events();
    let sync = events
        .iter()
        .find(|event| field(event, "action") == Some("mirror_sync") && field(event, "result") == Some("denied"))
        .unwrap();
    assert_eq!(field(sync, "index"), Some("pypi"));
    assert_eq!(field(sync, "project"), Some("flask"));
    assert_eq!(field(sync, "reason"), Some("project \"flask\" is blocked"));
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
    assert_eq!(field(sync, "index"), Some("pypi"));
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
    assert_eq!(field(sync, "index"), Some("pypi"));
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
    assert_eq!(field(sync, "index"), Some("pypi"));
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
    let record = peryx_storage::meta::CachedIndex {
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
