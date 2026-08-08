use peryx_storage::meta::{JobKind, JobOutcome, JobState, NewJobRun};
use rstest::rstest;

use super::*;
use crate::app;
use crate::cli::{JobCommand, JobListArgs, JobShowArgs};

fn list_command() -> JobCommand {
    JobCommand::List(JobListArgs {
        runtime: RuntimeArgs::default(),
    })
}

fn show_command(id: &str) -> JobCommand {
    JobCommand::Show(JobShowArgs {
        runtime: RuntimeArgs::default(),
        id: id.to_owned(),
    })
}

fn run_command(
    repository: &str,
    source: Option<&str>,
    max_projects: usize,
    concurrency: usize,
    timeout_secs: u64,
) -> JobCommand {
    JobCommand::Run {
        runtime: RuntimeArgs::default(),
        repository: repository.to_owned(),
        source: source.map(str::to_owned),
        max_projects,
        concurrency,
        timeout_secs,
    }
}

fn reindex_command(chunk_size: usize) -> JobCommand {
    JobCommand::Reindex {
        runtime: RuntimeArgs::default(),
        chunk_size,
    }
}

fn drain_command(authority: &str) -> JobCommand {
    JobCommand::Drain {
        runtime: RuntimeArgs::default(),
        authority: authority.to_owned(),
    }
}

fn catalog_server() -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let read = socket.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let body = if request.starts_with("GET /simple/ ") {
                r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"}]}"#
            } else {
                assert!(request.starts_with("GET /simple/flask/ "), "{request}");
                r#"{"meta":{"api-version":"1.4"},"name":"flask","files":[]}"#
            };
            write!(
                socket,
                "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.pypi.simple.v1+json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (format!("http://{address}/simple/"), handle)
}

fn start_job(meta: &MetaStore, scope: &str, started_at_unix: i64) -> String {
    meta.start_job_run(NewJobRun {
        kind: JobKind::CacheRefresh,
        scope,
        repository: None,
        started_at_unix,
    })
    .unwrap()
}

#[test]
fn test_job_list_prints_newest_first_with_every_state() {
    let (_dir, meta, config) = store_and_config();
    let running = start_job(&meta, "", 10);
    let succeeded = start_job(&meta, "root/pypi", 20);
    meta.finish_job_run(&succeeded, JobOutcome::succeeded(21, 12, 3))
        .unwrap();
    let failed = start_job(&meta, "pypi", 30);
    meta.finish_job_run(&failed, JobOutcome::failed(31, 4, 1, "upstream unavailable"))
        .unwrap();
    drop(meta);

    let mut out = Vec::new();
    app::job(&config, &list_command(), &mut out).unwrap();
    let page: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        page["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|attempt| attempt["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [&failed, &succeeded, &running]
    );
    assert_eq!(page["attempts"][0]["error"], "upstream unavailable");
    assert_eq!(page["attempts"][1]["items_changed"], 3);
    assert_eq!(page["attempts"][2]["state"], "running");
    assert!(page["next_cursor"].is_null());
}

#[test]
fn test_job_list_empty_prints_header() {
    let (_dir, meta, config) = store_and_config();
    drop(meta);
    let mut out = Vec::new();
    app::job(&config, &list_command(), &mut out).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
        serde_json::json!({"attempts": [], "next_cursor": null})
    );
}

#[test]
fn test_job_show_prints_detail() {
    let (_dir, meta, config) = store_and_config();
    let id = start_job(&meta, "root/pypi", 40);
    meta.finish_job_run(&id, JobOutcome::failed(42, 8, 2, "timed out"))
        .unwrap();
    drop(meta);

    let mut out = Vec::new();
    app::job(&config, &show_command(&id), &mut out).unwrap();
    let attempt: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(attempt["id"], id);
    assert_eq!(attempt["kind"], "cache_refresh");
    assert_eq!(attempt["scope"], "root/pypi");
    assert_eq!(attempt["state"], "failed");
    assert_eq!(attempt["finished_at_unix"], 42);
    assert_eq!(attempt["items_processed"], 8);
    assert_eq!(attempt["items_changed"], 2);
    assert_eq!(attempt["error"], "timed out");
}

#[test]
fn test_job_show_rejects_unknown_id() {
    let (_dir, meta, config) = store_and_config();
    drop(meta);
    let err = app::job(&config, &show_command("missing"), &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("unknown job run \"missing\""));
}

#[test]
fn test_job_reports_missing_store() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let err = app::job(&config, &list_command(), &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("open metadata store"));
}

#[test]
fn test_job_list_propagates_header_write_failure() {
    let (_dir, meta, config) = store_and_config();
    drop(meta);
    let err = app::job(&config, &list_command(), &mut FailImmediately).unwrap_err();
    assert!(err.to_string().contains("write failed"));
}

#[test]
fn test_job_list_propagates_row_write_failure() {
    let (_dir, meta, config) = store_and_config();
    start_job(&meta, "root/pypi", 50);
    drop(meta);
    let err = app::job(
        &config,
        &list_command(),
        &mut FailOnText {
            needle: "cache_refresh",
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("write failed"));
}

#[test]
fn test_job_show_propagates_write_failure() {
    let (_dir, meta, config) = store_and_config();
    let id = start_job(&meta, "root/pypi", 60);
    drop(meta);
    let err = app::job(
        &config,
        &show_command(&id),
        &mut FailOnText {
            needle: "state",
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("write failed"));
}

#[test]
fn test_job_run_executes_the_registered_catalog_job_and_prints_progress() {
    let (upstream, server) = catalog_server();
    let (_dir, meta, mut config) = store_and_config();
    let crate::config::IndexKind::Cached { routing, .. } = &mut config.indexes[0].kind else {
        panic!("default pypi index is cached");
    };
    routing.upstreams[0].url = upstream;
    drop(meta);
    let mut out = Vec::new();

    app::job(&config, &run_command("pypi", None, 1, 1, 30), &mut out).unwrap();

    assert_eq!(String::from_utf8(out).unwrap(), "processed\t1\nchanged\t2\n");
    server.join().unwrap();
    let runs = MetaStore::open(config.data_dir.join("peryx.redb"))
        .unwrap()
        .list_job_runs()
        .unwrap();
    assert_eq!(runs[0].kind, JobKind::CatalogSync);
    assert_eq!(runs[0].state, JobState::Succeeded);
    let mut history = Vec::new();
    app::job(&config, &list_command(), &mut history).unwrap();
    let history: serde_json::Value = serde_json::from_slice(&history).unwrap();
    assert_eq!(history["attempts"][0]["kind"], "catalog_sync");
    assert_eq!(history["attempts"][0]["scope"], "pypi");
    assert_eq!(history["attempts"][0]["state"], "succeeded");
}

#[rstest]
#[case::repository("", None, 1, 1, 1, "repository must not be empty")]
#[case::source("pypi", Some(" "), 1, 1, 1, "source must not be empty")]
#[case::zero_projects("pypi", None, 0, 1, 1, "max-projects must be positive")]
#[case::many_projects("pypi", None, 100_001, 1, 1, "max-projects exceeds the per-run limit")]
#[case::zero_concurrency("pypi", None, 1, 0, 1, "concurrency must be positive")]
#[case::much_concurrency("pypi", None, 1, 33, 1, "concurrency exceeds the per-run limit")]
#[case::zero_timeout("pypi", None, 1, 1, 0, "timeout-secs must be positive")]
#[case::long_timeout("pypi", None, 1, 1, 86_401, "timeout-secs exceeds the per-run limit")]
fn test_job_run_rejects_invalid_limits_before_opening_the_store(
    #[case] repository: &str,
    #[case] source: Option<&str>,
    #[case] max_projects: usize,
    #[case] concurrency: usize,
    #[case] timeout_secs: u64,
    #[case] expected: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);

    let error = app::job(
        &config,
        &run_command(repository, source, max_projects, concurrency, timeout_secs),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert_eq!(error.to_string(), expected);
}

#[test]
fn test_job_reindex_rebuilds_the_search_index_and_records_a_node_wide_run() {
    let (_dir, meta, config) = store_and_config();
    drop(meta);
    let mut out = Vec::new();

    app::job(&config, &reindex_command(1), &mut out).unwrap();

    assert_eq!(String::from_utf8(out).unwrap(), "processed\t0\nchanged\t0\n");
    let runs = MetaStore::open(config.data_dir.join("peryx.redb"))
        .unwrap()
        .list_job_runs()
        .unwrap();
    assert_eq!(runs[0].kind, JobKind::SearchRebuild);
    assert_eq!(runs[0].scope, "");
    assert_eq!(runs[0].state, JobState::Succeeded);
}

#[test]
fn test_job_drain_finalizes_retained_intents_and_records_an_authority_drain_run() {
    let (_dir, meta, config) = store_and_config();
    let limits = peryx_storage::meta::IntentLimits {
        max_records: 100,
        max_bytes: 1 << 20,
        backpressure_percent: 80,
    };
    meta.stage_intent(
        peryx_storage::meta::IntentAdmission {
            authority: "corp/flask",
            key: "corp\u{0}flask\u{0}k1",
            digest: "digest-a",
            size: 3,
            payload: b"body",
        },
        limits,
        1,
    )
    .unwrap();
    meta.stage_intent(
        peryx_storage::meta::IntentAdmission {
            authority: "corp/flask",
            key: "corp\u{0}flask\u{0}k2",
            digest: "digest-b",
            size: 4,
            payload: b"body2",
        },
        limits,
        1,
    )
    .unwrap();
    drop(meta);
    let mut out = Vec::new();

    app::job(&config, &drain_command("corp/flask"), &mut out).unwrap();

    assert_eq!(String::from_utf8(out).unwrap(), "processed\t2\nchanged\t2\n");
    let meta = MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    assert_eq!(
        meta.staged_intent("corp\u{0}flask\u{0}k1").unwrap().unwrap().phase,
        peryx_storage::meta::IntentPhase::Admitted
    );
    assert!(meta.list_pending_intents(10).unwrap().is_empty());
    let runs = meta.list_job_runs().unwrap();
    assert_eq!(runs[0].kind, JobKind::AuthorityDrain);
    assert_eq!(runs[0].repository.as_deref(), Some("corp/flask"));
    assert_eq!(runs[0].state, JobState::Succeeded);
}

#[test]
fn test_job_drain_rejects_an_empty_authority_before_opening_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);

    let error = app::job(&config, &drain_command("  "), &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), "authority must not be empty");
}

#[rstest]
#[case::zero(0, "chunk-size must be positive")]
#[case::too_large(1_000_001, "chunk-size exceeds the per-run limit")]
fn test_job_reindex_rejects_invalid_chunk_size_before_opening_the_store(
    #[case] chunk_size: usize,
    #[case] expected: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);

    let error = app::job(&config, &reindex_command(chunk_size), &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), expected);
}
