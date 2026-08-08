use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use peryx_driver::jobs::{
    CatalogSyncParameters, JobLimits, JobReport, JobScheduler, Schedule, ScheduledJob, run_schedules, scheduled_job,
};
use peryx_driver::state::AppState;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{JobKind, JobState, MetaError, MetaStore};
use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamRouter};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::catalog_projects_or_error;

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
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut app = AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 1_000));
    crate::install(&mut app);
    (dir, Arc::new(app))
}

async fn run(app: &Arc<AppState>, parameters: CatalogSyncParameters) -> Result<JobReport, String> {
    let scheduler = JobScheduler::new(app.serving.clone(), JobLimits::node_local());
    let job = scheduled_job(app, &ScheduledJob::CatalogSync(parameters)).unwrap();
    let result = scheduler.run(job).await;
    scheduler.shutdown().await;
    result
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
            format!(r#"{{"meta":{{"api-version":"1.4"}},"name":"{project}","files":[]}}"#),
            JSON,
        ))
        .expect(expected)
        .mount(server)
        .await;
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
        Err(error) if error == "no installed ecosystem supports cache_maintenance"
    ));

    assert_eq!(
        run(&app, parameters("bounded", 1, 1)).await.unwrap(),
        JobReport {
            processed: 1,
            changed: 2
        }
    );
    let runs = app.meta.list_job_runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].kind, JobKind::CatalogSync);
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
            changed: 1
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
                .set_body_raw(r#"{"meta":{"api-version":"1.4"},"name":"stable","files":[]}"#, JSON),
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
            changed: 2
        }
    );
    assert_eq!(
        run(&app, parameters("revalidation", 2, 2)).await.unwrap(),
        JobReport {
            processed: 2,
            changed: 1
        }
    );
    server.verify().await;
}

#[tokio::test]
async fn test_concurrent_public_jobs_coalesce_root_publication() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(20))
                .set_body_raw(r#"{"meta":{"api-version":"1.4"},"projects":[]}"#, JSON),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "coalesced",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);

    let (first, second) = tokio::join!(
        run(&app, parameters("coalesced", 1, 1)),
        run(&app, parameters("coalesced", 1, 1))
    );
    let mut reports = [first.unwrap(), second.unwrap()];
    reports.sort_by_key(|report| report.changed);

    assert_eq!(
        reports,
        [
            JobReport {
                processed: 0,
                changed: 0
            },
            JobReport {
                processed: 0,
                changed: 1
            }
        ]
    );
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
    let (_dir, mut app) = app(vec![index(
        "selected",
        crate::ECOSYSTEM,
        IndexKind::Cached {
            client: primary_client.clone(),
            offline: false,
        },
    )]);
    Arc::get_mut(&mut app).unwrap().upstream_routes.insert(
        "selected".to_owned(),
        UpstreamRouter::new(vec![
            NamedUpstream::new("primary", primary_client),
            NamedUpstream::new("selected", selected_client),
        ])
        .unwrap(),
    );
    let mut parameters = parameters("selected", 1, 1);
    parameters.source = Some("selected".to_owned());

    assert_eq!(
        run(&app, parameters).await.unwrap(),
        JobReport {
            processed: 0,
            changed: 1
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
    let (_dir, mut app) = app(vec![index(
        "routed",
        crate::ECOSYSTEM,
        IndexKind::Cached {
            client: primary_client.clone(),
            offline: false,
        },
    )]);
    Arc::get_mut(&mut app).unwrap().upstream_routes.insert(
        "routed".to_owned(),
        UpstreamRouter::new(vec![
            NamedUpstream::new("primary", primary_client),
            NamedUpstream::new("fallback", fallback_client),
        ])
        .unwrap(),
    );

    assert_eq!(
        run(&app, parameters("routed", 1, 1)).await.unwrap(),
        JobReport {
            processed: 1,
            changed: 2
        }
    );
    primary.verify().await;
    fallback.verify().await;
}

#[tokio::test]
async fn test_scheduled_catalog_job_uses_the_public_factory_and_scheduler() {
    let server = MockServer::start().await;
    mount_root(&server, &[]).await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "scheduled",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let cancel = CancellationToken::new();
    let timer = tokio::spawn(run_schedules(
        app.clone(),
        scheduler.clone(),
        vec![Schedule {
            job: ScheduledJob::CatalogSync(parameters("scheduled", 1, 1)),
            interval: Duration::from_millis(1),
        }],
        cancel.clone(),
    ));
    loop {
        if app
            .meta
            .list_job_runs()
            .unwrap()
            .first()
            .is_some_and(|run| run.state == JobState::Succeeded)
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    cancel.cancel();
    timer.await.unwrap();
    scheduler.shutdown().await;
    assert_eq!(app.meta.list_job_runs().unwrap()[0].kind, JobKind::CatalogSync);
}

#[tokio::test]
async fn test_cancellation_drops_an_inflight_project_without_partial_publication() {
    let server = MockServer::start().await;
    mount_root(&server, &["Flask"]).await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_raw(r#"{"meta":{"api-version":"1.4"},"name":"flask","files":[]}"#, JSON),
        )
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "cancel-project",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let job = scheduled_job(&app, &ScheduledJob::CatalogSync(parameters("cancel-project", 1, 1))).unwrap();
    let running = tokio::spawn({
        let scheduler = scheduler.clone();
        async move { scheduler.run(job).await }
    });
    loop {
        if server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path() == "/simple/flask/")
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    scheduler.shutdown().await;
    assert_eq!(
        running.await.unwrap().unwrap(),
        JobReport {
            processed: 0,
            changed: 1
        }
    );
    assert!(
        crate::store::catalog_state(&app.meta, "cancel-project")
            .unwrap()
            .active
            .is_some()
    );
    assert!(
        crate::store::active_project_generation(&app.meta, "cancel-project", "flask")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_cancellation_drops_an_inflight_root_without_publication() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_raw(r#"{"meta":{"api-version":"1.4"},"projects":[]}"#, JSON),
        )
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "cancel-root",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);
    let scheduler = Arc::new(JobScheduler::new(app.serving.clone(), JobLimits::node_local()));
    let job = scheduled_job(&app, &ScheduledJob::CatalogSync(parameters("cancel-root", 1, 1))).unwrap();
    let running = tokio::spawn({
        let scheduler = scheduler.clone();
        async move { scheduler.run(job).await }
    });
    loop {
        if !server.received_requests().await.unwrap().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }

    scheduler.shutdown().await;
    assert_eq!(running.await.unwrap().unwrap(), JobReport::default());
    assert!(
        crate::store::catalog_state(&app.meta, "cancel-root")
            .unwrap()
            .active
            .is_none()
    );
}

#[tokio::test]
async fn test_public_job_reports_retryable_status_and_timeout_failures() {
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

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .set_body_raw(r#"{"meta":{"api-version":"1.4"},"projects":[]}"#, JSON),
        )
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let (_dir, app) = app(vec![index(
        "timeout",
        crate::ECOSYSTEM,
        IndexKind::Cached { client, offline: false },
    )]);
    let mut parameters = parameters("timeout", 1, 1);
    parameters.timeout = Duration::from_millis(10);
    assert!(
        run(&app, parameters)
            .await
            .unwrap_err()
            .starts_with("retryable_timeout:")
    );
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
                .set_body_bytes(br#"{"meta":{"api-version":"1.4"},"name":"flask","files":[]}"#.to_vec()),
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
    let (_dir, mut routed_app) = self::app(vec![index(
        "source",
        crate::ECOSYSTEM,
        IndexKind::Cached {
            client: client.clone(),
            offline: false,
        },
    )]);
    Arc::get_mut(&mut routed_app).unwrap().upstream_routes.insert(
        "source".to_owned(),
        UpstreamRouter::new(vec![NamedUpstream::new("primary", client)]).unwrap(),
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
    Arc::get_mut(&mut app).unwrap().read_only = true;
    assert!(
        run(&app, parameters("read-only", 1, 1))
            .await
            .unwrap_err()
            .contains("read-only")
    );
}
