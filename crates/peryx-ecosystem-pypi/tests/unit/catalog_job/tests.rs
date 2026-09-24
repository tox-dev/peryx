use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use peryx_driver::PrometheusSource as _;
use peryx_driver::jobs::{
    JobFailure, JobLimits, JobReport, JobRunOutcome, JobScheduler, PluginScheduledJob, ScheduledJob, scheduled_job,
};
use peryx_driver::serving::{JobConfig, JobIndexConfig};
use peryx_driver::state::AppState;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{JobKind, JobState, MetaError, MetaStore};
use peryx_test_support::fault;
use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamRouter};
use rstest::rstest;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request as WiremockRequest, Respond, ResponseTemplate};

use super::{
    CatalogSyncFactory, CatalogSyncParameters, DEFAULT_CATALOG_CONCURRENCY, DEFAULT_CATALOG_PROJECTS,
    DEFAULT_CATALOG_TIMEOUT, MAX_CATALOG_CONCURRENCY, MAX_CATALOG_PROJECTS_PER_RUN, MAX_CATALOG_TIMEOUT,
    catalog_projects_or_error, compile, record_project_failure, scheduled_from_options, status_error,
};

const JSON: &str = "application/vnd.pypi.simple.v1+json";

fn parameters(repository: &str, max_projects: usize, concurrency: usize) -> CatalogSyncParameters {
    CatalogSyncParameters {
        repository: repository.to_owned(),
        source: None,
        max_projects: NonZeroUsize::new(max_projects).unwrap(),
        concurrency: NonZeroUsize::new(concurrency).unwrap(),
        timeout: Duration::from_secs(30),
    }
}

fn catalog_sync(parameters: CatalogSyncParameters) -> ScheduledJob {
    ScheduledJob::Plugin(PluginScheduledJob::new(
        crate::ECOSYSTEM,
        Arc::new(CatalogSyncFactory { parameters }),
    ))
}

fn index(name: &str, ecosystem: peryx_core::Ecosystem, kind: IndexKind) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem,
        kind,
        policy: Policy::default(),
        acl: peryx_identity::IndexAcl::default(),
    }
}

fn app(indexes: Vec<Index>) -> (tempfile::TempDir, Arc<AppState>) {
    app_with_routes(indexes, Vec::new())
}

fn app_with_routes(
    indexes: Vec<Index>,
    upstream_routes: Vec<(String, UpstreamRouter)>,
) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut app = AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 1_000));
    Arc::get_mut(&mut app.serving)
        .unwrap()
        .upstream_routes
        .extend(upstream_routes);
    crate::tests::install(&mut app);
    (dir, Arc::new(app))
}

/// An app whose clock reads `now`, so a test can move past a page's freshness window.
fn app_at(indexes: Vec<Index>, now: Arc<std::sync::atomic::AtomicI64>) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut app = AppState::with_clock(
        meta,
        blobs,
        60,
        indexes,
        Arc::new(move || now.load(std::sync::atomic::Ordering::SeqCst)),
    );
    crate::tests::install(&mut app);
    (dir, Arc::new(app))
}

fn app_with_store(meta: MetaStore, indexes: Vec<Index>) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut app = AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 1_000));
    crate::tests::install(&mut app);
    (dir, Arc::new(app))
}

async fn run(app: &Arc<AppState>, parameters: CatalogSyncParameters) -> Result<JobReport, String> {
    let scheduler = JobScheduler::new(app.serving.clone(), JobLimits::node_local());
    let job = scheduled_job(app, &catalog_sync(parameters)).unwrap();
    let result = scheduler.run(job).await;
    scheduler.shutdown().await;
    result.map(JobRunOutcome::report)
}

async fn mount_root(server: &MockServer, projects: &[&str]) {
    let projects = projects
        .iter()
        .map(|name| format!(r#"{{"name":"{name}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(r#"{{"meta":{{"api-version":"1.4"}},"projects":[{projects}]}}"#),
            JSON,
        ))
        .mount(server)
        .await;
}

async fn mount_project(server: &MockServer, project: &str, expected: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/simple/{project}/")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(r#"{{"meta":{{"api-version":"1.4"}},"versions":[],"name":"{project}","files":[]}}"#),
            JSON,
        ))
        .expect(expected)
        .mount(server)
        .await;
}

struct ArmStoreFault {
    fault: Arc<fault::Fault>,
    response: ResponseTemplate,
}

impl Respond for ArmStoreFault {
    fn respond(&self, _request: &WiremockRequest) -> ResponseTemplate {
        self.fault.arm(0);
        self.response.clone()
    }
}

struct StalledUpstream {
    client: UpstreamClient,
    entered: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

async fn stalled_upstream(stalled_path: &'static str, responses: Vec<(&'static str, &'static str)>) -> StalledUpstream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (entered_sender, entered) = oneshot::channel();
    let (release, release_receiver) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let mut entered_sender = Some(entered_sender);
        let mut release_receiver = Some(release_receiver);
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0, "request ended before headers");
                request.extend_from_slice(&chunk[..read]);
            }
            let path = String::from_utf8_lossy(&request)
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_owned();
            if path == stalled_path {
                entered_sender.take().unwrap().send(()).unwrap();
                release_receiver.take().unwrap().await.ok();
                return;
            }
            let body = responses
                .iter()
                .find(|(candidate, _)| *candidate == path)
                .map(|(_, body)| *body)
                .expect("the request has a configured response");
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {JSON}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    StalledUpstream {
        client: UpstreamClient::new(&format!("http://{address}/simple/")).unwrap(),
        entered,
        release,
        server,
    }
}

async fn await_stalled_request(upstream: &mut StalledUpstream) {
    tokio::time::timeout(Duration::from_secs(2), &mut upstream.entered)
        .await
        .expect("the upstream request starts")
        .unwrap();
}

async fn release_stalled_request(upstream: StalledUpstream) {
    upstream.release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), upstream.server)
        .await
        .expect("the upstream server exits")
        .unwrap();
}

thread_local! {
    static ACTIVE_PROGRESS_LOG: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct ProgressLogCapture(Arc<Mutex<Vec<u8>>>);

impl ProgressLogCapture {
    fn install(&self) -> ProgressLogGuard {
        let subscriber = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::INFO)
                .with_writer(ProgressLogWriter)
                .finish(),
        );
        ACTIVE_PROGRESS_LOG.with(|slot| *slot.borrow_mut() = Some(self.0.clone()));
        ProgressLogGuard {
            _subscriber: subscriber,
        }
    }

    fn progress_lines(&self) -> usize {
        std::io::Write::flush(&mut ProgressLogSink(Some(self.0.clone()))).unwrap();
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains("catalog sync progress"))
            .count()
    }
}

struct ProgressLogGuard {
    _subscriber: tracing::dispatcher::DefaultGuard,
}

impl Drop for ProgressLogGuard {
    fn drop(&mut self) {
        ACTIVE_PROGRESS_LOG.with(|slot| *slot.borrow_mut() = None);
    }
}

struct ProgressLogWriter;

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for ProgressLogWriter {
    type Writer = ProgressLogSink;

    fn make_writer(&'writer self) -> Self::Writer {
        ProgressLogSink(ACTIVE_PROGRESS_LOG.with(|slot| slot.borrow().clone()))
    }
}

struct ProgressLogSink(Option<Arc<Mutex<Vec<u8>>>>);

impl std::io::Write for ProgressLogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(bytes) = &self.0 {
            bytes.lock().unwrap().extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 101 projects with `MAX_PROGRESS_UPDATES == 100` gives a progress interval of 2, so the log
/// fires on every even `processed` count plus the final, odd one (51 lines total). Swapping the
/// interval check's `==` for `!=` logs on every count except the last (100 lines); swapping the
/// enclosing `||` for `&&` never logs at all, since 101 is never a multiple of 2 (0 lines).
#[tokio::test(flavor = "current_thread")]
async fn test_public_job_logs_progress_at_the_expected_intervals() {
    let server = MockServer::start().await;
    let projects = (0..101).map(|index| format!("Project{index}")).collect::<Vec<_>>();
    mount_root(&server, &projects.iter().map(String::as_str).collect::<Vec<_>>()).await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/project[0-9]+/$"))
        .respond_with(ResponseTemplate::new(404))
        .expect(101)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "progress-logging",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);

    let capture = ProgressLogCapture::default();
    let guard = capture.install();
    assert_eq!(
        run(&app, parameters("progress-logging", 101, 16)).await.unwrap(),
        JobReport {
            processed: 101,
            changed: 1,
            ..JobReport::default()
        }
    );
    drop(guard);

    assert_eq!(capture.progress_lines(), 51);
    server.verify().await;
}

#[test]
fn test_parameters_use_catalog_defaults() {
    assert_eq!(
        CatalogSyncParameters::new("packages"),
        CatalogSyncParameters {
            repository: "packages".to_owned(),
            source: None,
            max_projects: NonZeroUsize::new(DEFAULT_CATALOG_PROJECTS).unwrap(),
            concurrency: NonZeroUsize::new(DEFAULT_CATALOG_CONCURRENCY).unwrap(),
            timeout: DEFAULT_CATALOG_TIMEOUT,
        }
    );
}

#[test]
fn test_compile_ignores_other_job_kinds() {
    assert!(
        compile(JobConfig {
            kind: "other",
            settings: &toml::Table::new(),
            indexes: &[],
        })
        .is_none()
    );
}

#[test]
fn test_compile_preserves_valid_settings() {
    let settings = toml::from_str(
        r#"
repository = "packages"
source = "primary"
max_projects = 3
concurrency = 2
timeout_secs = 9
"#,
    )
    .unwrap();
    let indexes = [JobIndexConfig {
        name: "packages",
        ecosystem: crate::ECOSYSTEM,
        cached: true,
        offline: false,
        upstreams: vec!["primary"],
    }];
    let scheduled = compile(JobConfig {
        kind: "catalog_sync",
        settings: &settings,
        indexes: &indexes,
    })
    .unwrap()
    .unwrap();

    assert_eq!(
        (scheduled.ecosystem(), scheduled.kind(), scheduled.settings()),
        (crate::ECOSYSTEM, "catalog_sync", settings)
    );
}

#[rstest]
#[case::unknown_field("repository = 'packages'\nunknown = true", "unknown field `unknown`")]
#[case::missing_repository("", "catalog sync needs a non-empty `repository`")]
#[case::empty_repository("repository = ' '", "catalog sync needs a non-empty `repository`")]
#[case::empty_source("repository = 'packages'\nsource = ' '", "catalog sync `source` must not be empty")]
#[case::negative_projects(
    "repository = 'packages'\nmax_projects = -1",
    "`max_projects` must be a non-negative integer"
)]
#[case::zero_projects(
    "repository = 'packages'\nmax_projects = 0",
    "catalog sync `max_projects` must be positive"
)]
#[case::too_many_projects(
    "repository = 'packages'\nmax_projects = 100001",
    "catalog sync `max_projects` exceeds the per-run limit"
)]
#[case::maximum_toml_projects(
    "repository = 'packages'\nmax_projects = 9223372036854775807",
    "catalog sync `max_projects` exceeds the per-run limit"
)]
#[case::zero_concurrency(
    "repository = 'packages'\nconcurrency = 0",
    "catalog sync `concurrency` must be positive"
)]
#[case::too_much_concurrency(
    "repository = 'packages'\nconcurrency = 33",
    "catalog sync `concurrency` exceeds the per-run limit"
)]
#[case::zero_timeout(
    "repository = 'packages'\ntimeout_secs = 0",
    "catalog sync `timeout_secs` must be between 1 and 86400"
)]
#[case::long_timeout(
    "repository = 'packages'\ntimeout_secs = 86401",
    "catalog sync `timeout_secs` must be between 1 and 86400"
)]
#[case::unknown_source(
    "repository = 'packages'\nsource = 'other'",
    "catalog sync `source` must name a repository upstream"
)]
fn test_compile_rejects_invalid_settings(#[case] source: &str, #[case] expected: &str) {
    let settings = toml::from_str(source).unwrap();
    let indexes = [JobIndexConfig {
        name: "packages",
        ecosystem: crate::ECOSYSTEM,
        cached: true,
        offline: false,
        upstreams: vec!["primary"],
    }];

    assert_eq!(
        compile(JobConfig {
            kind: "catalog_sync",
            settings: &settings,
            indexes: &indexes,
        })
        .unwrap()
        .unwrap_err(),
        expected
    );
}

#[rstest]
#[case::missing(
    "other",
    crate::ECOSYSTEM,
    true,
    false,
    "catalog sync `repository` must name a configured index"
)]
#[case::not_cached(
    "packages",
    crate::ECOSYSTEM,
    false,
    false,
    "catalog sync `repository` must name a cached index"
)]
#[case::other_ecosystem(
    "packages",
    peryx_core::Ecosystem::new("other"),
    true,
    false,
    "catalog sync needs an online repository with catalog support"
)]
#[case::offline(
    "packages",
    crate::ECOSYSTEM,
    true,
    true,
    "catalog sync needs an online repository with catalog support"
)]
fn test_compile_rejects_invalid_repository(
    #[case] name: &str,
    #[case] ecosystem: peryx_core::Ecosystem,
    #[case] cached: bool,
    #[case] offline: bool,
    #[case] expected: &str,
) {
    let settings = toml::from_str("repository = 'packages'").unwrap();
    let indexes = [JobIndexConfig {
        name,
        ecosystem,
        cached,
        offline,
        upstreams: Vec::new(),
    }];

    assert_eq!(
        compile(JobConfig {
            kind: "catalog_sync",
            settings: &settings,
            indexes: &indexes,
        })
        .unwrap()
        .unwrap_err(),
        expected
    );
}

#[rstest]
#[case::empty_repository("", None, 1, 1, 1, "repository must not be empty")]
#[case::empty_source("packages", Some(" "), 1, 1, 1, "source must not be empty")]
#[case::zero_projects("packages", None, 0, 1, 1, "max-projects must be positive")]
#[case::too_many_projects("packages", None, 100_001, 1, 1, "max-projects exceeds the per-run limit")]
#[case::zero_concurrency("packages", None, 1, 0, 1, "concurrency must be positive")]
#[case::too_much_concurrency("packages", None, 1, 33, 1, "concurrency exceeds the per-run limit")]
#[case::zero_timeout("packages", None, 1, 1, 0, "timeout-secs must be positive")]
#[case::long_timeout("packages", None, 1, 1, 86_401, "timeout-secs exceeds the per-run limit")]
fn test_scheduled_options_reject_invalid_values(
    #[case] repository: &str,
    #[case] source: Option<&str>,
    #[case] max_projects: usize,
    #[case] concurrency: usize,
    #[case] timeout_secs: u64,
    #[case] expected: &str,
) {
    assert_eq!(
        scheduled_from_options(repository, source, max_projects, concurrency, timeout_secs).unwrap_err(),
        expected
    );
}

#[test]
fn test_scheduled_options_accept_boundary_maximums() {
    assert!(
        scheduled_from_options(
            "packages",
            None,
            MAX_CATALOG_PROJECTS_PER_RUN,
            MAX_CATALOG_CONCURRENCY,
            MAX_CATALOG_TIMEOUT.as_secs(),
        )
        .is_ok()
    );
}

#[test]
fn test_compile_accepts_boundary_maximum_settings() {
    let settings = toml::from_str(&format!(
        "repository = 'packages'\nmax_projects = {}\nconcurrency = {}\ntimeout_secs = {}",
        MAX_CATALOG_PROJECTS_PER_RUN,
        MAX_CATALOG_CONCURRENCY,
        MAX_CATALOG_TIMEOUT.as_secs(),
    ))
    .unwrap();
    let indexes = [JobIndexConfig {
        name: "packages",
        ecosystem: crate::ECOSYSTEM,
        cached: true,
        offline: false,
        upstreams: Vec::new(),
    }];

    assert!(
        compile(JobConfig {
            kind: "catalog_sync",
            settings: &settings,
            indexes: &indexes,
        })
        .unwrap()
        .is_ok()
    );
}

#[rstest]
#[case::just_below_rate_limited(428, "upstream")]
#[case::rate_limited(429, "retryable_upstream")]
#[case::just_below_server_error(499, "upstream")]
#[case::server_error_boundary(500, "retryable_upstream")]
fn test_status_error_categorizes_by_boundary(#[case] status: u16, #[case] expected_category: &str) {
    assert_eq!(status_error(status).code(), expected_category);
}

#[test]
fn test_scheduled_options_preserve_valid_values() {
    let scheduled = scheduled_from_options("packages", Some("primary"), 3, 2, 9).unwrap();
    assert_eq!(
        (scheduled.ecosystem(), scheduled.kind(), scheduled.settings()),
        (
            crate::ECOSYSTEM,
            "catalog_sync",
            toml::from_str::<toml::Table>(
                r#"
repository = "packages"
source = "primary"
max_projects = 3
concurrency = 2
timeout_secs = 9
"#,
            )
            .unwrap()
        )
    );
}

#[tokio::test]
async fn test_public_job_factory_runs_bounded_catalog_sync_and_persists_progress() {
    crate::tests::install_global_subscriber();
    let server = MockServer::start().await;
    mount_root(&server, &["Zulu", "Alpha"]).await;
    mount_project(&server, "alpha", 1).await;
    mount_project(&server, "zulu", 0).await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "bounded",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);
    assert!(matches!(
        scheduled_job(&app, &ScheduledJob::CacheMaintenance),
        Err(error) if error == "cache maintenance expands through installed drivers"
    ));

    assert_eq!(
        run(&app, parameters("bounded", 1, 1)).await.unwrap(),
        JobReport {
            processed: 1,
            changed: 2,
            ..JobReport::default()
        }
    );
    let runs = app.serving.meta.list_job_runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].kind, JobKind::new("catalog_sync").unwrap());
    assert_eq!(runs[0].state, JobState::Succeeded);
    assert_eq!(runs[0].items_processed, 1);
    assert_eq!(runs[0].items_changed, 2);
    server.verify().await;
}

#[test]
fn test_storage_errors_have_a_stable_category() {
    assert_eq!(
        catalog_projects_or_error(Err(MetaError::DriverPrecondition("catalog scan failed".to_owned())))
            .unwrap_err()
            .to_string(),
        "storage: driver precondition failed: catalog scan failed"
    );
}

#[tokio::test]
async fn test_public_job_bounds_progress_updates_for_large_catalog_slices() {
    crate::tests::install_global_subscriber();
    let server = MockServer::start().await;
    let projects = (0..101).map(|index| format!("Project{index}")).collect::<Vec<_>>();
    mount_root(&server, &projects.iter().map(String::as_str).collect::<Vec<_>>()).await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/project[0-9]+/$"))
        .respond_with(ResponseTemplate::new(404))
        .expect(101)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "progress",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);
    assert_eq!(
        run(&app, parameters("progress", 101, 16)).await.unwrap(),
        JobReport {
            processed: 101,
            changed: 1,
            ..JobReport::default()
        }
    );
    server.verify().await;
}

#[tokio::test]
async fn test_public_job_revalidates_root_and_project_generations_and_tolerates_missing_projects() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Missing"},{"name":"Stable"}]}"#,
            JSON,
        ))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/stable/"))
        .and(header("if-none-match", "stable-v1"))
        .respond_with(ResponseTemplate::new(304))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/stable/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "stable-v1")
                .set_body_raw(
                    r#"{"meta":{"api-version":"1.4"},"versions":[],"name":"stable","files":[]}"#,
                    JSON,
                ),
        )
        .with_priority(10)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/missing/"))
        .respond_with(ResponseTemplate::new(404))
        .expect(2)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let now = Arc::new(std::sync::atomic::AtomicI64::new(1_000));
    let (_dir, app) = app_at(
        vec![index(
            "revalidation",
            crate::ECOSYSTEM,
            IndexKind::Cached { client, offline: false },
        )],
        now.clone(),
    );

    assert_eq!(
        run(&app, parameters("revalidation", 2, 2)).await.unwrap(),
        JobReport {
            processed: 2,
            changed: 2,
            ..JobReport::default()
        }
    );
    // Within their freshness windows the page and the negative answer would stand in for a request.
    now.fetch_add(3_600, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        run(&app, parameters("revalidation", 2, 2)).await.unwrap(),
        JobReport {
            processed: 2,
            changed: 1,
            ..JobReport::default()
        }
    );
    server.verify().await;
}

/// A root the upstream answers `304` for is a run that changed nothing, and the job records it as
/// such. Every other root test here serves `200` on every request, so the job's `NotModified` arm was
/// reached only by the coalescing shortcut this change removes: nothing drove the job through a real
/// revalidation.
#[tokio::test]
async fn test_public_job_records_no_change_for_a_revalidated_root() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .and(header("if-none-match", "root-v1"))
        .respond_with(ResponseTemplate::new(304))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "root-v1")
                .set_body_raw(r#"{"meta":{"api-version":"1.4"},"projects":[]}"#, JSON),
        )
        .with_priority(10)
        .expect(1)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "revalidated-root",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);

    let published = run(&app, parameters("revalidated-root", 1, 1)).await.unwrap();
    let revalidated = run(&app, parameters("revalidated-root", 1, 1)).await.unwrap();

    assert_eq!(
        (published, revalidated),
        (
            JobReport {
                processed: 0,
                changed: 1,
                ..JobReport::default()
            },
            JobReport {
                processed: 0,
                changed: 0,
                ..JobReport::default()
            }
        )
    );
    server.verify().await;
}

struct HeldUpstream {
    client: UpstreamClient,
    entered: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
    held_requests: Arc<std::sync::atomic::AtomicUsize>,
}

const HELD_ROOT: &str = r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"flask"}]}"#;
const HELD_FLASK: &str = r#"{"meta":{"api-version":"1.4"},"versions":[],"name":"flask","files":[]}"#;

/// Serves the root and `flask` at once, except the first request for `held_path`, which answers `held_body`
/// only once released; later requests for it answer at once. Each
/// connection runs on its own task, so the held request never blocks the others.
async fn held_upstream(held_path: &'static str, held_body: &'static str) -> HeldUpstream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (entered, entered_receiver) = oneshot::channel();
    let (release, release_receiver) = oneshot::channel::<()>();
    let hold = Arc::new(tokio::sync::Mutex::new(Some((entered, release_receiver))));
    let held_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = held_requests.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let hold = hold.clone();
            let counted = counted.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(read, 0, "request ended before headers");
                    request.extend_from_slice(&chunk[..read]);
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                let body = if request.contains(&format!("GET {held_path} ")) {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let pending = hold.lock().await.take();
                    if let Some((entered, release)) = pending {
                        entered.send(()).unwrap();
                        release.await.unwrap();
                    }
                    held_body
                } else if request.contains("GET /simple/flask/ ") {
                    HELD_FLASK
                } else {
                    HELD_ROOT
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: {JSON}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
        }
    });
    HeldUpstream {
        client: UpstreamClient::new(&format!("http://{address}/simple/")).unwrap(),
        entered: entered_receiver,
        release,
        held_requests,
    }
}

fn held_app(upstream: &HeldUpstream) -> (tempfile::TempDir, Arc<AppState>) {
    app(vec![index(
        "held",
        crate::ECOSYSTEM,
        IndexKind::Cached {
            client: upstream.client.clone(),
            offline: false,
        },
    )])
}

/// Resolves `flask` on `app` the way a request does, leading the page fetch.
fn request_flask(app: &Arc<AppState>) -> tokio::task::JoinHandle<Result<Option<crate::ProjectDetail>, String>> {
    let app = app.clone();
    tokio::spawn(async move {
        let index = app.serving.index_at(0);
        crate::cache::resolve_detail(&app.serving, index, "flask", &index.route)
            .await
            .map_err(|err| err.to_string())
    })
}

/// Runs the catalog job once the test has confirmed it queued behind the flight for `key`.
async fn job_joining(app: &Arc<AppState>, key: &str) -> tokio::task::JoinHandle<Result<JobReport, String>> {
    let mut joins = app.serving.cache.inflight.subscribe(key).unwrap();
    let job = tokio::spawn({
        let app = app.clone();
        async move { run(&app, parameters("held", 1, 1)).await }
    });
    joins.next_join().await.unwrap();
    job
}

/// A request fetching a project's page and the job syncing it make one upstream request: the job queues on
/// the page's flight and finds the page the request stored. The request caused that change, so the job counts
/// only its root.
#[tokio::test]
async fn test_public_job_shares_a_page_fetch_with_a_request() {
    let upstream = held_upstream("/simple/flask/", HELD_FLASK).await;
    let (_dir, app) = held_app(&upstream);
    let request = request_flask(&app);
    let HeldUpstream {
        entered,
        release,
        held_requests,
        ..
    } = upstream;
    entered.await.unwrap();
    let job = job_joining(&app, "held/flask").await;

    release.send(()).unwrap();

    assert!(request.await.unwrap().unwrap().is_some());
    assert_eq!(
        (
            job.await.unwrap().unwrap(),
            held_requests.load(std::sync::atomic::Ordering::SeqCst)
        ),
        (
            JobReport {
                processed: 1,
                changed: 1,
                ..JobReport::default()
            },
            1
        )
    );
}

/// The job joins a root flight another caller leads, so only its own project publication counts.
#[tokio::test]
async fn test_public_job_does_not_count_a_root_it_joined() {
    let upstream = held_upstream("/simple/", HELD_ROOT).await;
    let (_dir, app) = held_app(&upstream);
    let leader = tokio::spawn({
        let app = app.clone();
        let client = upstream.client.clone();
        async move {
            crate::catalog::sync_catalog(
                &client,
                &app.serving.cache.inflight,
                &app.serving.meta,
                "held",
                client.base_url(),
            )
            .await
        }
    });
    let HeldUpstream { entered, release, .. } = upstream;
    entered.await.unwrap();
    let job = job_joining(&app, "pypi\0catalog\0held").await;

    release.send(()).unwrap();

    assert!(matches!(
        leader.await.unwrap(),
        crate::Synced::Led(Ok(crate::catalog::CatalogSyncOutcome::Published { projects: 1 }))
    ));
    assert_eq!(
        job.await.unwrap().unwrap(),
        JobReport {
            processed: 1,
            changed: 1,
            ..JobReport::default()
        }
    );
}

/// A request whose page fetch fails stores nothing, so the job queued behind it fetches again and reports its
/// own failure rather than a success it never observed (#1302).
#[tokio::test]
async fn test_public_job_refetches_after_a_request_fetch_failed() {
    let upstream = held_upstream("/simple/flask/", "not json").await;
    let (_dir, app) = held_app(&upstream);
    let request = request_flask(&app);
    let HeldUpstream {
        entered,
        release,
        held_requests,
        ..
    } = upstream;
    entered.await.unwrap();
    let job = job_joining(&app, "held/flask").await;

    release.send(()).unwrap();

    assert!(request.await.unwrap().is_err());
    assert_eq!(
        (
            job.await.unwrap().unwrap_err().contains("flask"),
            held_requests.load(std::sync::atomic::Ordering::SeqCst)
        ),
        (true, 2)
    );
}

#[tokio::test]
async fn test_concurrent_public_jobs_both_report_a_failed_project_refresh() {
    let server = MockServer::start().await;
    mount_root(&server, &["flask"]).await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("not json", JSON))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "failing",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);

    let first = tokio::spawn({
        let app = app.clone();
        async move { run(&app, parameters("failing", 1, 1)).await }
    });
    let second = tokio::spawn({
        let app = app.clone();
        async move { run(&app, parameters("failing", 1, 1)).await }
    });
    let (first, second) = tokio::join!(first, second);

    assert!(first.unwrap().unwrap_err().contains("flask"));
    assert!(second.unwrap().unwrap_err().contains("flask"));
}

#[rstest]
#[case::serial(1)]
#[case::parallel(2)]
#[tokio::test]
async fn test_public_job_continues_after_a_project_failure(#[case] concurrency: usize) {
    let server = MockServer::start().await;
    mount_root(&server, &["Broken", "Healthy"]).await;
    Mock::given(method("GET"))
        .and(path("/simple/broken/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("invalid", JSON))
        .expect(1)
        .mount(&server)
        .await;
    mount_project(&server, "healthy", 1).await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "continue-after-failure",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);

    let error = run(&app, parameters("continue-after-failure", 2, concurrency))
        .await
        .unwrap_err();

    assert!(error.starts_with("project_sync: 1 project sync failures; project \"broken\":"));
    assert!(
        crate::store::get_index(&app.serving.meta, "continue-after-failure/healthy")
            .unwrap()
            .is_some()
    );
    server.verify().await;
}

#[tokio::test]
async fn test_public_job_aborts_on_a_store_error_before_requesting_later_project() {
    let server = MockServer::start().await;
    mount_root(&server, &["Broken", "Later"]).await;
    let (backend, fault) = fault::backend();
    let meta = MetaStore::open_backend(fault::faulted(&backend, &fault)).unwrap();
    Mock::given(method("GET"))
        .and(path("/simple/broken/"))
        .respond_with(ArmStoreFault {
            fault: fault.clone(),
            response: ResponseTemplate::new(200).set_body_raw(
                r#"{"meta":{"api-version":"1.4"},"versions":[],"name":"broken","files":[]}"#,
                JSON,
            ),
        })
        .expect(1)
        .mount(&server)
        .await;
    mount_project(&server, "later", 0).await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app_with_store(
        meta,
        vec![index(
            "stop-on-store-error",
            crate::ECOSYSTEM,
            IndexKind::Cached { client, offline: false },
        )],
    );

    let error = run(&app, parameters("stop-on-store-error", 2, 1)).await.unwrap_err();

    assert!(fault.triggered());
    assert!(error.contains("Previous I/O error"));
    server.verify().await;
}

#[tokio::test]
async fn test_public_job_orders_failures_by_catalog_position() {
    let server = MockServer::start().await;
    mount_root(&server, &["Zulu", "Alpha"]).await;
    Mock::given(method("GET"))
        .and(path("/simple/alpha/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("invalid alpha", JSON)
                .set_delay(Duration::from_millis(50)),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/zulu/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("invalid zulu", JSON))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "ordered-failures",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);

    let error = run(&app, parameters("ordered-failures", 2, 2)).await.unwrap_err();

    assert!(error.find("project \"alpha\"").unwrap() < error.find("project \"zulu\"").unwrap());
}

/// Failures arrive in completion order, but the diagnostics keep the catalog's first ones: a failure
/// earlier in the catalog displaces the latest kept one, and one past every kept one changes nothing.
#[rstest]
#[case::earlier_displaces_the_latest(0, &["zero", "one", "three"])]
#[case::between_displaces_the_latest(4, &["one", "three", "four"])]
#[case::later_is_dropped(9, &["one", "three", "five"])]
fn test_record_project_failure_keeps_the_earliest_catalog_failures(#[case] ordinal: usize, #[case] kept: &[&str]) {
    let names = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    ];
    let mut diagnostics = Vec::new();
    for known in [1, 3, 5] {
        record_project_failure(
            &mut diagnostics,
            known,
            names[known],
            &JobFailure::new("sync", "failed"),
        );
    }

    record_project_failure(
        &mut diagnostics,
        ordinal,
        names[ordinal],
        &JobFailure::new("sync", "failed"),
    );

    assert_eq!(
        diagnostics
            .iter()
            .map(|(_, failure)| failure.message().to_owned())
            .collect::<Vec<_>>(),
        kept.iter()
            .map(|project| format!("project {project:?}: failed"))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn test_public_job_bounds_failure_diagnostics() {
    let server = MockServer::start().await;
    mount_root(&server, &["Alpha", "Bravo", "Charlie", "Delta"]).await;
    let invalid_version = "é".repeat(1_000);
    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/(alpha|bravo|charlie|delta)/$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(r#"{{"meta":{{"api-version":"{invalid_version}"}},"name":"package","files":[]}}"#),
            JSON,
        ))
        .expect(4)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "bounded-failures",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);

    let error = run(&app, parameters("bounded-failures", 4, 4)).await.unwrap_err();
    let (_, summary) = error.split_once(": ").unwrap();
    let diagnostics = summary.split("; ").skip(1).collect::<Vec<_>>();

    assert!(summary.starts_with("4 project sync failures"));
    assert!(error.len() < 2_048);
    assert_eq!(diagnostics.len(), 3);
    assert!(diagnostics.iter().all(|diagnostic| diagnostic.len() <= 512));
    assert!(error.contains("project \"alpha\""));
    assert!(error.contains("project \"bravo\""));
    assert!(error.contains("project \"charlie\""));
    assert!(!error.contains("project \"delta\""));
    server.verify().await;
}

#[tokio::test]
async fn test_public_job_factory_uses_the_selected_named_source() {
    let primary = MockServer::start().await;
    let selected = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&primary)
        .await;
    mount_root(&selected, &[]).await;
    let primary_client = UpstreamClient::new(&format!("{}/simple/", primary.uri())).unwrap();
    let selected_client = UpstreamClient::new(&format!("{}/simple/", selected.uri())).unwrap();
    let (_dir, app) = app_with_routes(
        vec![index(
            "selected",
            crate::ECOSYSTEM,
            IndexKind::Cached {
                client: primary_client.clone(),
                offline: false,
            },
        )],
        vec![(
            "selected".to_owned(),
            UpstreamRouter::new(vec![
                NamedUpstream::new("primary", primary_client),
                NamedUpstream::new("selected", selected_client),
            ])
            .unwrap(),
        )],
    );
    let mut parameters = parameters("selected", 1, 1);
    parameters.source = Some("selected".to_owned());

    assert_eq!(
        run(&app, parameters).await.unwrap(),
        JobReport {
            processed: 0,
            changed: 1,
            ..JobReport::default()
        }
    );
    primary.verify().await;
    selected.verify().await;
}

#[tokio::test]
async fn test_public_job_factory_uses_repository_routing_when_source_is_absent() {
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&primary)
        .await;
    mount_root(&fallback, &["Flask"]).await;
    mount_project(&fallback, "flask", 1).await;
    let primary_client = UpstreamClient::new(&format!("{}/simple/", primary.uri())).unwrap();
    let fallback_client = UpstreamClient::new(&format!("{}/simple/", fallback.uri())).unwrap();
    let (_dir, app) = app_with_routes(
        vec![index(
            "routed",
            crate::ECOSYSTEM,
            IndexKind::Cached {
                client: primary_client.clone(),
                offline: false,
            },
        )],
        vec![(
            "routed".to_owned(),
            UpstreamRouter::new(vec![
                NamedUpstream::new("primary", primary_client),
                NamedUpstream::new("fallback", fallback_client),
            ])
            .unwrap(),
        )],
    );

    assert_eq!(
        run(&app, parameters("routed", 1, 1)).await.unwrap(),
        JobReport {
            processed: 1,
            changed: 2,
            ..JobReport::default()
        }
    );
    primary.verify().await;
    fallback.verify().await;
}

#[tokio::test]
async fn test_catalog_job_uses_the_public_factory_and_scheduler_completion() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"meta":{"api-version":"1.4"},"projects":[]}"#, JSON))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "scheduled",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let job = scheduled_job(&app, &catalog_sync(parameters("scheduled", 1, 1))).unwrap();
    assert_eq!(
        scheduler.run(job).await.unwrap(),
        JobRunOutcome::succeeded(JobReport {
            processed: 0,
            changed: 1,
            ..JobReport::default()
        })
    );
    scheduler.shutdown().await;
    let runs = app.serving.meta.list_job_runs().unwrap();
    assert_eq!(runs[0].state, JobState::Succeeded);
    assert_eq!(runs[0].kind, JobKind::new("catalog_sync").unwrap());
    assert_eq!(runs[0].scope, "scheduled");
    let mut metrics_text = String::new();
    scheduler.metrics().write_metrics(&mut metrics_text);
    assert!(metrics_text.contains("kind=\"catalog_sync\""));
}

#[tokio::test]
async fn test_cancellation_drops_an_inflight_project_without_partial_publication() {
    let mut upstream = stalled_upstream(
        "/simple/flask/",
        vec![(
            "/simple/",
            r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"}]}"#,
        )],
    )
    .await;
    let (_dir, app) = app(vec![index(
        "cancel-project",
        crate::ECOSYSTEM,
        IndexKind::Cached {
            client: upstream.client.clone(),
            offline: false,
        },
    )]);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let job = scheduled_job(&app, &catalog_sync(parameters("cancel-project", 1, 1))).unwrap();
    let running = tokio::spawn({
        let scheduler = scheduler.clone();
        async move { scheduler.run(job).await }
    });
    await_stalled_request(&mut upstream).await;

    tokio::time::timeout(Duration::from_secs(2), scheduler.shutdown())
        .await
        .expect("the cancelled project job exits");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .expect("the project job reports cancellation")
            .unwrap()
            .unwrap(),
        JobRunOutcome::cancelled(JobReport {
            processed: 0,
            changed: 1,
            ..JobReport::default()
        })
    );
    assert!(
        crate::store::catalog_state(&app.serving.meta, "cancel-project")
            .unwrap()
            .active
            .is_some()
    );
    assert!(
        crate::store::get_index(&app.serving.meta, "cancel-project/flask")
            .unwrap()
            .is_none()
    );
    release_stalled_request(upstream).await;
}

#[tokio::test]
async fn test_cancellation_drops_an_inflight_root_without_publication() {
    let mut upstream = stalled_upstream("/simple/", Vec::new()).await;
    let (_dir, app) = app(vec![index(
        "cancel-root",
        crate::ECOSYSTEM,
        IndexKind::Cached {
            client: upstream.client.clone(),
            offline: false,
        },
    )]);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let job = scheduled_job(&app, &catalog_sync(parameters("cancel-root", 1, 1))).unwrap();
    let running = tokio::spawn({
        let scheduler = scheduler.clone();
        async move { scheduler.run(job).await }
    });
    await_stalled_request(&mut upstream).await;

    tokio::time::timeout(Duration::from_secs(2), scheduler.shutdown())
        .await
        .expect("the cancelled root job exits");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .expect("the root job reports cancellation")
            .unwrap()
            .unwrap(),
        JobRunOutcome::cancelled(JobReport::default())
    );
    assert!(
        crate::store::catalog_state(&app.serving.meta, "cancel-root")
            .unwrap()
            .active
            .is_none()
    );
    release_stalled_request(upstream).await;
}

#[tokio::test]
async fn test_cancellation_wins_after_a_project_success_and_failure() {
    let mut upstream = stalled_upstream(
        "/simple/stalled/",
        vec![
            (
                "/simple/",
                r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Broken"},{"name":"Healthy"},{"name":"Stalled"}]}"#,
            ),
            ("/simple/broken/", "invalid"),
            (
                "/simple/healthy/",
                r#"{"meta":{"api-version":"1.4"},"versions":[],"name":"healthy","files":[]}"#,
            ),
        ],
    )
    .await;
    let (_dir, app) = app(vec![index(
        "cancel-after-results",
        crate::ECOSYSTEM,
        IndexKind::Cached {
            client: upstream.client.clone(),
            offline: false,
        },
    )]);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let job = scheduled_job(&app, &catalog_sync(parameters("cancel-after-results", 3, 1))).unwrap();
    let running = tokio::spawn({
        let scheduler = scheduler.clone();
        async move { scheduler.run(job).await }
    });
    await_stalled_request(&mut upstream).await;

    scheduler.shutdown().await;

    assert_eq!(
        running.await.unwrap().unwrap(),
        JobRunOutcome::cancelled(JobReport {
            processed: 2,
            changed: 2,
            ..JobReport::default()
        })
    );
    release_stalled_request(upstream).await;
}

#[tokio::test]
async fn test_public_job_reports_status_failures() {
    for (status, expected) in [(503, "retryable_upstream"), (400, "upstream:")] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
        let (_dir, app) = app(vec![index(
            "status",
            crate::ECOSYSTEM,
            IndexKind::Cached { client, offline: false },
        )]);
        assert!(
            run(&app, parameters("status", 1, 1))
                .await
                .unwrap_err()
                .contains(expected)
        );
    }
}

#[rstest]
#[case::bounded_read_deadline(
    Duration::from_mins(1),
    Duration::from_secs(30),
    "retryable_timeout: upstream request timed out"
)]
#[case::job_timeout(
    Duration::from_secs(5),
    Duration::from_secs(5),
    "retryable_timeout: catalog sync exceeded 5s"
)]
#[tokio::test]
async fn test_public_job_reports_timeout_failure(
    #[case] timeout: Duration,
    #[case] stalled_for: Duration,
    #[case] expected: &str,
) {
    let mut upstream = stalled_upstream("/simple/", Vec::new()).await;
    let (_dir, app) = app(vec![index(
        "timeout",
        crate::ECOSYSTEM,
        IndexKind::Cached {
            client: upstream.client.clone(),
            offline: false,
        },
    )]);
    let mut parameters = parameters("timeout", 1, 1);
    parameters.timeout = timeout;
    let running = tokio::spawn(async move { run(&app, parameters).await });
    await_stalled_request(&mut upstream).await;

    tokio::time::pause();
    tokio::time::advance(stalled_for).await;
    assert_eq!(running.await.unwrap().unwrap_err(), expected);
    release_stalled_request(upstream).await;
}

#[tokio::test]
async fn test_public_job_categorizes_transport_and_invalid_root_failures() {
    let client = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let (_dir, transport_app) = app(vec![index(
        "transport",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);
    assert_eq!(
        run(&transport_app, parameters("transport", 1, 1)).await.unwrap_err(),
        "retryable_upstream: upstream connection failed"
    );

    for (response, expected) in [
        (
            ResponseTemplate::new(200).set_body_bytes(br#"{"meta":{"api-version":"1.4"},"projects":[]}"#.to_vec()),
            "upstream:",
        ),
        (ResponseTemplate::new(200).set_body_raw("{", JSON), "catalog_sync:"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/"))
            .respond_with(response)
            .mount(&server)
            .await;
        let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
        let (_dir, app) = app(vec![index(
            "root-category",
            crate::ECOSYSTEM,
            IndexKind::Cached { client, offline: false },
        )]);

        assert!(
            run(&app, parameters("root-category", 1, 1))
                .await
                .unwrap_err()
                .starts_with(expected)
        );
    }
}

#[tokio::test]
async fn test_public_job_categorizes_project_status_content_type_and_data_failures() {
    let cases = [
        (ResponseTemplate::new(503), "retryable_upstream"),
        (
            ResponseTemplate::new(200)
                .set_body_bytes(br#"{"meta":{"api-version":"1.4"},"versions":[],"name":"flask","files":[]}"#.to_vec()),
            "upstream:",
        ),
        (ResponseTemplate::new(200).set_body_raw("{", JSON), "project_sync:"),
    ];
    for (response, expected) in cases {
        let server = MockServer::start().await;
        mount_root(&server, &["Flask"]).await;
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(response)
            .mount(&server)
            .await;
        let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
        let (_dir, app) = app(vec![index(
            "project-category",
            crate::ECOSYSTEM,
            IndexKind::Cached { client, offline: false },
        )]);

        assert!(
            run(&app, parameters("project-category", 1, 1))
                .await
                .unwrap_err()
                .contains(expected)
        );
    }
}

#[tokio::test]
async fn test_public_job_rejects_incompatible_runtime_repositories_and_sources() {
    let client = UpstreamClient::new("https://example.invalid/simple/").unwrap();
    let cases = [
        (Vec::new(), parameters("missing", 1, 1), "unknown repository"),
        (
            vec![index(
                "oci",
                peryx_core::Ecosystem::new("other"),
                IndexKind::Cached {
                    client: client.clone(),
                    offline: false,
                },
            )],
            parameters("oci", 1, 1),
            "not a PyPI repository",
        ),
        (
            vec![index("hosted", crate::ECOSYSTEM, IndexKind::Hosted { volatile: false })],
            parameters("hosted", 1, 1),
            "not an online cached repository",
        ),
        (
            vec![index(
                "offline",
                crate::ECOSYSTEM,
                IndexKind::Cached {
                    client: client.clone(),
                    offline: true,
                },
            )],
            parameters("offline", 1, 1),
            "not an online cached repository",
        ),
    ];
    for (indexes, parameters, expected) in cases {
        let (_dir, app) = app(indexes);
        assert!(run(&app, parameters).await.unwrap_err().contains(expected));
    }

    let (_dir, legacy_app) = app(vec![index(
        "legacy",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);
    let mut legacy_parameters = parameters("legacy", 1, 1);
    legacy_parameters.source = Some("missing".to_owned());
    assert!(
        run(&legacy_app, legacy_parameters)
            .await
            .unwrap_err()
            .contains("no named upstream sources")
    );

    let client = UpstreamClient::new("https://example.invalid/simple/").unwrap();
    let (_dir, routed_app) = app_with_routes(
        vec![index(
            "source",
            crate::ECOSYSTEM,
            IndexKind::Cached {
                client: client.clone(),
                offline: false,
            },
        )],
        vec![(
            "source".to_owned(),
            UpstreamRouter::new(vec![NamedUpstream::new("primary", client)]).unwrap(),
        )],
    );
    let mut routed_parameters = parameters("source", 1, 1);
    routed_parameters.source = Some("missing".to_owned());
    assert!(
        run(&routed_app, routed_parameters)
            .await
            .unwrap_err()
            .contains("unknown upstream source")
    );

    let client = UpstreamClient::new("https://example.invalid/simple/").unwrap();
    let (_dir, mut app) = self::app(vec![index(
        "read-only",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);
    Arc::get_mut(&mut app).unwrap().set_read_only(true).unwrap();
    assert!(
        run(&app, parameters("read-only", 1, 1))
            .await
            .unwrap_err()
            .contains("read-only")
    );
}
