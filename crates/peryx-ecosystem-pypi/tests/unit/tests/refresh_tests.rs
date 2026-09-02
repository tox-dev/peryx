use peryx_driver::serving::CacheRefresher as _;
use peryx_driver::state::ServingState;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use peryx_storage::blob::Digest;
use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamRouter};
use wiremock::matchers::{header as match_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::http::{detail_json, get, harness, harness_with_policies, routed_state};
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

fn drilled(state: &Arc<ServingState>, field: &str) -> u64 {
    let totals = state.metrics.drill(Some("pypi"), None);
    ["base", "cached", "hosted", "ecosystem"]
        .into_iter()
        .find_map(|group| totals["totals"][group][field].as_u64())
        .unwrap_or(0)
}

fn settle(state: &Arc<ServingState>, field: &str, want: u64) {
    state.metrics.flush().unwrap();
    let got = drilled(state, field);
    assert!(got >= want, "metric {field} settled at {got}, want >= {want}");
}

fn policy(configure: impl FnOnce(&mut PolicyConfig)) -> Policy {
    let mut config = PolicyConfig::default();
    configure(&mut config);
    Policy::compile(&config, crate::normalize_name)
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

    // Configured TTLs cap longer upstream freshness grants.
    let summary = refresh_stale_pages(&h.state.serving).await.unwrap();
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

    let summary = refresh_stale_pages(&h.state.serving).await.unwrap();
    assert_eq!((summary.checked, summary.changed), (1, 1));
    settle(&h.state.serving, "changed", 1);
    assert!(drilled(&h.state.serving, "refreshes") >= 1);

    h.server.reset().await;
    let (_, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(body.contains(new_digest.as_str()));
}

#[tokio::test]
async fn test_serving_refresh_stale_reports_the_sweep() {
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
        .refresh_stale(h.state.serving.clone())
        .await
        .unwrap();
    assert_eq!((sweep.checked, sweep.changed), (1, 1));
}

#[tokio::test]
async fn test_serving_refresh_stale_surfaces_errors_as_strings() {
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
        .refresh_stale(h.state.serving.clone())
        .await
        .unwrap_err();
    assert!(err.contains("simple API document could not be parsed"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_refresh_sweep_skips_policy_denied_project() {
    let mirror_policy = policy(|policy| {
        policy.block_resources = vec!["flask".to_owned()];
    });
    let h = harness_with_policies(true, true, mirror_policy, Policy::default(), Policy::default()).await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    h.state
        .serving
        .meta
        .put_index(
            "pypi/flask",
            &CachedIndex {
                source: None,
                last_modified: None,
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

    let summary = refresh_stale_pages(&h.state.serving).await.unwrap();

    drop(guard);
    assert_eq!(summary, crate::cache::RefreshSummary::default());
    let events = logs.security_events();
    let sync = events
        .iter()
        .find(|event| field(event, "action") == Some("mirror_sync") && field(event, "result") == Some("denied"))
        .unwrap();
    assert_eq!(field(sync, "index"), Some("pypi"));
    assert_eq!(field(sync, "resource"), Some("flask"));
    assert_eq!(field(sync, "reason"), Some("resource \"flask\" is blocked"));
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

    assert_eq!(refresh_stale_pages(&h.state.serving).await.unwrap().changed, 1);

    drop(guard);
    let events = logs.security_events();
    let sync = events
        .iter()
        .find(|event| field(event, "action") == Some("mirror_sync") && field(event, "result") == Some("success"))
        .unwrap();
    assert_eq!(field(sync, "index"), Some("pypi"));
    assert_eq!(field(sync, "resource"), Some("flask"));
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

    assert_eq!(refresh_stale_pages(&h.state.serving).await.unwrap().checked, 1);

    drop(guard);
    let events = logs.security_events();
    let sync = events
        .iter()
        .find(|event| field(event, "action") == Some("mirror_sync") && field(event, "result") == Some("noop"))
        .unwrap();
    assert_eq!(field(sync, "index"), Some("pypi"));
    assert_eq!(field(sync, "resource"), Some("flask"));
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

    let err = refresh_stale_pages(&h.state.serving).await.unwrap_err();

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
    assert_eq!(field(sync, "resource"), Some("flask"));
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

    let summary = refresh_stale_pages(&h.state.serving).await.unwrap();
    assert_eq!((summary.checked, summary.changed), (1, 0));
    settle(&h.state.serving, "refreshes", 1);
    assert_eq!(drilled(&h.state.serving, "changed"), 0);
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

    let summary = refresh_stale_pages(&h.state.serving).await.unwrap();
    assert_eq!(summary.checked, 0);
}

#[tokio::test]
async fn test_upstream_max_age_shortens_freshness() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());

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
    drop(
        peryx_index::serving::flight_gate(&h.state.serving.cache.inflight, "pypi/flask")
            .lock_owned()
            .await,
    );
    let refreshed = h.state.serving.meta.get_index("pypi/flask").unwrap().unwrap();
    assert_eq!(refreshed.fetched_at_unix, 1006);
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
    // Barriers make the background refresh and metric aggregation deterministic.
    drop(
        peryx_index::serving::flight_gate(&h.state.serving.cache.inflight, "pypi/flask")
            .lock_owned()
            .await,
    );
    h.state.serving.metrics.flush().unwrap();
    assert_eq!(drilled(&h.state.serving, "stale_served"), 1);
}

#[tokio::test]
async fn test_refresh_skips_keys_without_a_mirror() {
    let h = harness().await;
    let record = CachedIndex {
        source: None,
        last_modified: None,
        etag: None,
        last_serial: None,
        fetched_at_unix: 0,
        content_type: None,
        fresh_secs: None,
        body: b"{}".to_vec(),
    };
    h.state.serving.meta.put_index("ghost/thing", &record).unwrap();
    h.clock.fetch_add(3600, Ordering::Relaxed);
    let summary = refresh_stale_pages(&h.state.serving).await.unwrap();
    assert_eq!(summary.checked, 0);
}

#[tokio::test]
async fn test_refresh_sweep_full_fetch_with_identical_body_is_unchanged() {
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
    h.clock.fetch_add(61, Ordering::Relaxed);
    let summary = refresh_stale_pages(&h.state.serving).await.unwrap();
    assert_eq!((summary.checked, summary.changed), (1, 0));
}

const LAST_MODIFIED: &str = "Tue, 01 Sep 2026 00:00:00 GMT";

#[tokio::test]
async fn test_refresh_sweep_revalidates_unchanged_via_last_modified() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_page(
        &h.server,
        detail_json(digest.as_str(), &file_url),
        ResponseTemplate::new(200).insert_header("last-modified", LAST_MODIFIED),
    )
    .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(304))
        .mount(&h.server)
        .await;
    h.clock.fetch_add(61, Ordering::Relaxed);

    let summary = refresh_stale_pages(&h.state.serving).await.unwrap();

    assert_eq!((summary.checked, summary.changed), (1, 0));
    let sent = h.server.received_requests().await.unwrap();
    assert_eq!(sent[0].headers["if-modified-since"], LAST_MODIFIED);
    let stored = h.state.serving.meta.get_index("pypi/flask").unwrap().unwrap();
    assert_eq!(stored.last_modified.as_deref(), Some(LAST_MODIFIED));
}

async fn mount_routed_page(server: &MockServer, body: String) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .insert_header("last-modified", LAST_MODIFIED)
                .insert_header("cache-control", "public, max-age=0")
                .set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(server)
        .await;
}

async fn mount_get_404(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

/// A routed `pypi` whose stale cached page came from `second`, the fallback behind a 404ing `first`.
async fn routed_page_from_second(
    dir: &tempfile::TempDir,
    first: &MockServer,
    second: &MockServer,
) -> Arc<peryx_driver::state::AppState> {
    let file_url = format!("{}/files/flask.whl", second.uri());
    mount_get_404(first).await;
    mount_routed_page(second, detail_json(Digest::of(b"wheel").as_str(), &file_url)).await;
    let primary = UpstreamClient::new(&format!("{}/simple/", first.uri())).unwrap();
    let router = UpstreamRouter::new(vec![
        NamedUpstream::new("first", primary.clone()),
        NamedUpstream::new(
            "second",
            UpstreamClient::new(&format!("{}/simple/", second.uri())).unwrap(),
        ),
    ])
    .unwrap();
    let state = routed_state(dir, primary, router);
    get(&state, "/pypi/simple/flask/", Some("application/json")).await;
    first.reset().await;
    second.reset().await;
    state
}

async fn mount_not_modified(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(304))
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_a_routed_200_stores_the_source_that_answered_and_its_validators() {
    let (first, second) = (MockServer::start().await, MockServer::start().await);
    let dir = tempfile::tempdir().unwrap();
    let state = routed_page_from_second(&dir, &first, &second).await;

    let stored = state.serving.meta.get_index("pypi/flask").unwrap().unwrap();

    assert_eq!(
        (
            stored.source.as_deref(),
            stored.etag.as_deref(),
            stored.last_modified.as_deref()
        ),
        (Some("second"), Some("\"v1\""), Some(LAST_MODIFIED))
    );
}

#[tokio::test]
async fn test_refresh_sweep_sends_validators_only_to_the_source_that_answered() {
    let (first, second) = (MockServer::start().await, MockServer::start().await);
    let dir = tempfile::tempdir().unwrap();
    let state = routed_page_from_second(&dir, &first, &second).await;
    let stored = state.serving.meta.get_index("pypi/flask").unwrap().unwrap();
    mount_get_404(&first).await;
    mount_not_modified(&second).await;

    let summary = refresh_stale_pages(&state.serving).await.unwrap();

    assert_eq!((summary.checked, summary.changed), (1, 0));
    let revalidated = second.received_requests().await.unwrap();
    assert_eq!(revalidated[0].headers["if-none-match"], "\"v1\"");
    let unvalidated = first.received_requests().await.unwrap();
    assert_eq!(unvalidated.len(), 1);
    assert!(!unvalidated[0].headers.contains_key("if-none-match"));
    assert!(!unvalidated[0].headers.contains_key("if-modified-since"));
    let refreshed = state.serving.meta.get_index("pypi/flask").unwrap().unwrap();
    assert_eq!(
        (refreshed.source.as_deref(), refreshed.body),
        (Some("second"), stored.body)
    );
}

#[tokio::test]
async fn test_refresh_sweep_rejects_a_304_from_a_source_that_never_answered() {
    let (first, second) = (MockServer::start().await, MockServer::start().await);
    let dir = tempfile::tempdir().unwrap();
    let state = routed_page_from_second(&dir, &first, &second).await;
    let stored = state.serving.meta.get_index("pypi/flask").unwrap().unwrap();
    // `first` was asked unconditionally, so its "not modified" is about no page peryx holds.
    mount_not_modified(&first).await;

    let error = refresh_stale_pages(&state.serving).await.unwrap_err();

    assert!(matches!(error, crate::cache::CacheError::Unavailable));
    let untouched = state.serving.meta.get_index("pypi/flask").unwrap().unwrap();
    assert_eq!(untouched, stored);
}
