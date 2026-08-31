use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use peryx_driver::jobs::{
    JobLimits, JobReport, JobRunOutcome, JobScheduler, PluginScheduledJob, ScheduledJob, scheduled_job,
};
use peryx_driver::serving::{JobConfig, JobIndexConfig};
use peryx_driver::state::AppState;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{JobKind, JobState, MetaError, MetaStore};
use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamRouter};
use rstest::rstest;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    CatalogSyncFactory, CatalogSyncParameters, DEFAULT_CATALOG_CONCURRENCY, DEFAULT_CATALOG_PROJECTS,
    DEFAULT_CATALOG_TIMEOUT, catalog_projects_or_error, compile, scheduled_from_options,
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

struct StalledUpstream {
    client: UpstreamClient,
    entered: oneshot::Receiver<()>,
    release: oneshot::Sender<Option<&'static str>>,
    server: tokio::task::JoinHandle<()>,
}

async fn stalled_upstream(stalled_path: &'static str, root_body: Option<&'static str>) -> StalledUpstream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (entered_sender, entered) = oneshot::channel();
    let (release, release_receiver) = oneshot::channel::<Option<&'static str>>();
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
                if let Ok(Some(body)) = release_receiver.take().unwrap().await {
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
                return;
            }
            let body = root_body.expect("only the root request may precede the stalled request");
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
    upstream.release.send(None).unwrap();
    tokio::time::timeout(Duration::from_secs(2), upstream.server)
        .await
        .expect("the upstream server exits")
        .unwrap();
}

async fn answer_stalled_request(upstream: StalledUpstream, body: &'static str) {
    upstream.release.send(Some(body)).unwrap();
    tokio::time::timeout(Duration::from_secs(2), upstream.server)
        .await
        .expect("the upstream server exits")
        .unwrap();
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
    let (_dir, app) = app(vec![index(
        "revalidation",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);

    assert_eq!(
        run(&app, parameters("revalidation", 2, 2)).await.unwrap(),
        JobReport {
            processed: 2,
            changed: 2,
            ..JobReport::default()
        }
    );
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

#[tokio::test]
async fn test_concurrent_public_jobs_coalesce_root_publication() {
    let mut upstream = stalled_upstream("/simple/", None).await;
    let (_dir, app) = app(vec![index(
        "coalesced",
        crate::ECOSYSTEM,
        IndexKind::Cached {
            client: upstream.client.clone(),
            offline: false,
        },
    )]);

    let first = tokio::spawn({
        let app = app.clone();
        async move { run(&app, parameters("coalesced", 1, 1)).await }
    });
    let second = tokio::spawn({
        let app = app.clone();
        async move { run(&app, parameters("coalesced", 1, 1)).await }
    });
    await_stalled_request(&mut upstream).await;
    answer_stalled_request(upstream, r#"{"meta":{"api-version":"1.4"},"projects":[]}"#).await;
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    let mut reports = [first.unwrap(), second.unwrap()];
    reports.sort_by_key(|report| report.changed);

    assert_eq!(
        reports,
        [
            JobReport {
                processed: 0,
                changed: 0,
                ..JobReport::default()
            },
            JobReport {
                processed: 0,
                changed: 1,
                ..JobReport::default()
            }
        ]
    );
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
    assert_eq!(app.serving.meta.list_job_runs().unwrap()[0].state, JobState::Succeeded);
    assert_eq!(
        app.serving.meta.list_job_runs().unwrap()[0].kind,
        JobKind::new("catalog_sync").unwrap()
    );
}

#[tokio::test]
async fn test_cancellation_drops_an_inflight_project_without_partial_publication() {
    let mut upstream = stalled_upstream(
        "/simple/flask/",
        Some(r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"}]}"#),
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
        crate::store::active_project_generation(&app.serving.meta, "cancel-project", "flask")
            .unwrap()
            .is_none()
    );
    release_stalled_request(upstream).await;
}

#[tokio::test]
async fn test_cancellation_drops_an_inflight_root_without_publication() {
    let mut upstream = stalled_upstream("/simple/", None).await;
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

#[tokio::test]
async fn test_public_job_reports_timeout_failure() {
    let mut upstream = stalled_upstream("/simple/", None).await;
    let (_dir, app) = app(vec![index(
        "timeout",
        crate::ECOSYSTEM,
        IndexKind::Cached {
            client: upstream.client.clone(),
            offline: false,
        },
    )]);
    let mut parameters = parameters("timeout", 1, 1);
    parameters.timeout = Duration::from_secs(30);
    let running = tokio::spawn(async move { run(&app, parameters).await });
    await_stalled_request(&mut upstream).await;

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(running.await.unwrap().unwrap_err().starts_with("retryable_timeout:"));
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
