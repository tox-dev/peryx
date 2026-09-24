use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use peryx_driver::state::AppState;
use peryx_index::{Index, IndexKind};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{MetaError, MetaStore};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::store::PypiStore as _;

const JSON: &str = "application/vnd.pypi.simple.v1+json";

/// A mirror app on `server` whose clock reads `now`, so a test can move past a page's freshness window.
fn mirror(server: &MockServer, now: Arc<AtomicI64>) -> (tempfile::TempDir, Arc<AppState>, UpstreamClient) {
    mirror_with(server, now, peryx_policy::Policy::default(), None)
}

/// A mirror app with `policy`, and with `upstream_limit` concurrent upstream requests when one is given.
fn mirror_with(
    server: &MockServer,
    now: Arc<AtomicI64>,
    policy: peryx_policy::Policy,
    upstream_limit: Option<usize>,
) -> (tempfile::TempDir, Arc<AppState>, UpstreamClient) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let index = Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Cached {
            client: client.clone(),
            offline: false,
        },
        policy,
        acl: peryx_identity::IndexAcl::default(),
    };
    let mut app = AppState::with_clock(
        meta,
        blobs,
        60,
        vec![index],
        Arc::new(move || now.load(Ordering::SeqCst)),
    );
    if let Some(limit) = upstream_limit {
        Arc::get_mut(&mut app.serving).unwrap().upstream_limits =
            peryx_driver::rate_limit::UpstreamLimits::new([("pypi".to_owned(), limit)]);
    }
    crate::tests::install(&mut app);
    (dir, Arc::new(app), client)
}

fn detail(version: &str) -> String {
    format!(
        r#"{{"meta":{{"api-version":"1.1"}},"versions":["{version}"],"name":"flask","files":[{{"filename":"flask-{version}.tar.gz","url":"https://files.example/flask-{version}.tar.gz","hashes":{{"sha256":"{sha}"}},"size":10}}]}}"#,
        sha = "a".repeat(64),
    )
}

/// Serves `body` for `/simple/flask/` once, at `priority`, so later requests fall through to the next mock.
async fn answer_once(server: &MockServer, response: ResponseTemplate, priority: u8) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(response)
        .up_to_n_times(1)
        .with_priority(priority)
        .expect(1)
        .mount(server)
        .await;
}

fn page(version: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("etag", "v1")
        .set_body_raw(detail(version), JSON)
}

#[tokio::test]
async fn test_refresh_stores_the_first_page_as_a_change() {
    let server = MockServer::start().await;
    answer_once(&server, page("1.0"), 1).await;
    let (_dir, app, client) = mirror(&server, Arc::new(AtomicI64::new(1_000)));

    let outcome = refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap();

    assert_eq!(
        (outcome, app.serving.meta.get_index("pypi/flask").unwrap().is_some()),
        (ProjectSyncOutcome::Changed, true)
    );
}

/// A page inside its freshness window stands in for a request: a request stored it, so this refresh
/// changes nothing and asks upstream for nothing.
#[tokio::test]
async fn test_refresh_reuses_a_fresh_page_without_a_request() {
    let server = MockServer::start().await;
    answer_once(&server, page("1.0"), 1).await;
    let (_dir, app, client) = mirror(&server, Arc::new(AtomicI64::new(1_000)));
    refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap();

    let again = refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap();

    assert_eq!(again, ProjectSyncOutcome::Unchanged);
}

#[rstest::rstest]
#[case::revalidated(ResponseTemplate::new(304), ProjectSyncOutcome::Unchanged)]
#[case::resent(page("1.0"), ProjectSyncOutcome::Unchanged)]
#[case::replaced(page("2.0"), ProjectSyncOutcome::Changed)]
#[tokio::test]
async fn test_refresh_past_the_freshness_window_reports_what_upstream_changed(
    #[case] answer: ResponseTemplate,
    #[case] expected: ProjectSyncOutcome,
) {
    let server = MockServer::start().await;
    answer_once(&server, page("1.0"), 1).await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header("if-none-match", "v1"))
        .respond_with(answer)
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1_000));
    let (_dir, app, client) = mirror(&server, now.clone());
    refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap();
    now.store(5_000, Ordering::SeqCst);

    let outcome = refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap();

    assert_eq!(outcome, expected);
}

/// A `404` retires the page, and the negative answer then stands in for the next refresh.
#[tokio::test]
async fn test_refresh_retires_a_project_upstream_no_longer_has() {
    let server = MockServer::start().await;
    answer_once(&server, ResponseTemplate::new(404), 1).await;
    let (_dir, app, client) = mirror(&server, Arc::new(AtomicI64::new(1_000)));

    let retired = refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap();
    let remembered = refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap();

    assert_eq!(
        (retired, remembered, app.serving.meta.get_index("pypi/flask").unwrap()),
        (ProjectSyncOutcome::Missing, ProjectSyncOutcome::Missing, None)
    );
}

/// A request would serve the stale page through an upstream failure, but the job reports the failure: its
/// report would otherwise count a page upstream never confirmed.
#[tokio::test]
async fn test_refresh_reports_an_upstream_failure_behind_a_stale_page() {
    let server = MockServer::start().await;
    answer_once(&server, page("1.0"), 1).await;
    // The upstream client retries a `503`, so the failure has to persist past its first answer.
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(2)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1_000));
    let (_dir, app, client) = mirror(&server, now.clone());
    refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap();
    now.store(1_100, Ordering::SeqCst);

    let error = refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap_err();

    assert!(matches!(error, ProjectSyncError::Status(503)));
}

#[tokio::test]
async fn test_refresh_reports_an_unreachable_upstream() {
    let closed = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let server = MockServer::start().await;
    let (_dir, app, _) = mirror(&server, Arc::new(AtomicI64::new(1_000)));
    let client = UpstreamClient::new(&format!("http://{}/simple/", closed.local_addr().unwrap())).unwrap();
    drop(closed);

    let error = refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap_err();

    assert!(matches!(error, ProjectSyncError::Upstream(_)));
}

/// A page the cache refuses is a failure of that project alone, and nothing is stored.
#[tokio::test]
async fn test_refresh_reports_a_page_the_cache_refuses() {
    let server = MockServer::start().await;
    answer_once(&server, ResponseTemplate::new(200).set_body_raw("not json", JSON), 1).await;
    let (_dir, app, client) = mirror(&server, Arc::new(AtomicI64::new(1_000)));

    let error = refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap_err();

    assert_eq!(
        (
            matches!(error, ProjectSyncError::Page(_)),
            app.serving.meta.get_index("pypi/flask").unwrap()
        ),
        (true, None)
    );
}

#[test]
fn test_project_sync_error_routes_each_cache_failure() {
    assert_eq!(
        [
            ProjectSyncError::from(CacheError::Upstream(UpstreamError::DeadlineExceeded)),
            ProjectSyncError::from(CacheError::Meta(MetaError::DriverPrecondition("gone".to_owned()))),
            ProjectSyncError::from(CacheError::Unavailable),
            ProjectSyncError::from(CacheError::FileNotFound),
        ]
        .map(|error| {
            [
                matches!(error, ProjectSyncError::Upstream(_)),
                matches!(error, ProjectSyncError::Store(_)),
                matches!(error, ProjectSyncError::Page(_)),
                matches!(error, ProjectSyncError::Internal(_)),
            ]
        }),
        [
            [true, false, false, false],
            [false, true, false, false],
            [false, false, true, false],
            [false, false, false, true],
        ]
    );
}

/// A project the policy refuses to cache is skipped without a request rather than failing the job.
#[tokio::test]
async fn test_refresh_skips_a_project_the_policy_denies() {
    let server = MockServer::start().await;
    let policy = peryx_policy::Policy::compile(
        &peryx_policy::PolicyConfig {
            block_resources: vec!["flask".to_owned()],
            ..peryx_policy::PolicyConfig::default()
        },
        crate::normalize_name,
    );
    let (_dir, app, client) = mirror_with(&server, Arc::new(AtomicI64::new(1_000)), policy, None);

    let outcome = refresh_project_page(&app.serving, "pypi", "flask", &client)
        .await
        .unwrap();

    assert_eq!(
        (outcome, server.received_requests().await.unwrap().len()),
        (ProjectSyncOutcome::Denied, 0)
    );
}

// paused-clock-safe: the saturated limit refuses before any upstream call, and the retry finds the page this
// test stores, so the mock server records no request
#[tokio::test(start_paused = true)]
async fn test_refresh_waits_out_a_full_upstream_limit_and_retries() {
    let server = MockServer::start().await;
    let (_dir, app, client) = mirror_with(
        &server,
        Arc::new(AtomicI64::new(1_000)),
        peryx_policy::Policy::default(),
        Some(1),
    );
    let held = app.serving.upstream_limits.acquire("pypi").await.unwrap();
    let refresh = tokio::spawn({
        let app = app.clone();
        async move { refresh_project_page(&app.serving, "pypi", "flask", &client).await }
    });
    while app.serving.upstream_limits.totals().denied == 0 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    crate::store::put_index(
        &app.serving.meta,
        "pypi/flask",
        &crate::store::CachedIndex {
            source: None,
            etag: None,
            last_modified: None,
            last_serial: None,
            fetched_at_unix: 1_000,
            content_type: Some(JSON.to_owned()),
            fresh_secs: None,
            body: detail("1.0").into_bytes(),
        },
    )
    .unwrap();
    drop(held);

    assert_eq!(
        (
            refresh.await.unwrap().unwrap(),
            server.received_requests().await.unwrap().len()
        ),
        (ProjectSyncOutcome::Unchanged, 0)
    );
}
